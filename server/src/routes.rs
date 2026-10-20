use std::{collections::HashMap, time::Instant};

use axum::{
    Router,
    body::Body,
    extract::{Query, State},
    http::{HeaderMap, Method, Response, StatusCode, header},
    response::IntoResponse,
    routing::get,
};
use bytes::Bytes;
const MAX_HTML_BYTES: usize = 8 * 1024 * 1024;
const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;

use crate::{
    error::AppError,
    extract::{extract_head_metadata, merge_meta_tags},
    fetcher::fetch_page,
    image_proxy::{process_image, validate_raster},
    image_worker::process_image_with_helper,
    models::{CacheStatus, ImageCacheHit, ImageCacheWrite, ImageFormat, ImageRequest, UnfurlData},
    state::AppState,
    utils::{
        DEFAULT_IMAGE_FIT, DEFAULT_IMAGE_QUALITY, build_image_proxy_url_for_request,
        build_processed_image_cache_key, build_processed_image_object_key, build_unfurl_cache_key,
        choose_image_format, clamp_quality, ensure_image_content_type, error_response,
        health_response, normalize_target_url, parse_boolean_param, parse_image_fit,
        parse_image_format, parse_number_param, parse_optional_number_param, request_origin,
        strip_body_for_head, success_response, validate_public_url,
    },
};

struct RefreshGuard(String);
fn refresh_keys() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static KEYS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    KEYS.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}
impl RefreshGuard {
    fn acquire(key: &str) -> Option<Self> {
        refresh_keys()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(key.to_owned())
            .then(|| Self(key.to_owned()))
    }
}
impl Drop for RefreshGuard {
    fn drop(&mut self) {
        refresh_keys()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&self.0);
    }
}

async fn cache_key_lock(key: &str) -> tokio::sync::OwnedMutexGuard<()> {
    use std::sync::{Arc, Mutex, OnceLock, Weak};
    static LOCKS: OnceLock<Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let lock = {
        let mut locks = LOCKS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        locks.retain(|_, value| value.strong_count() > 0);
        match locks.get(key).and_then(Weak::upgrade) {
            Some(lock) => lock,
            None => {
                let lock = Arc::new(tokio::sync::Mutex::new(()));
                locks.insert(key.to_owned(), Arc::downgrade(&lock));
                lock
            }
        }
    };
    lock.lock_owned().await
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/health", get(health))
        .route("/api", get(unfurl))
        .route("/proxy/image", get(image_proxy))
        .fallback(route_not_found)
        .method_not_allowed_fallback(method_not_allowed)
}

async fn health(method: Method) -> impl IntoResponse {
    strip_body_for_head(&method, health_response(Instant::now()))
}

async fn route_not_found(method: Method) -> Response<Body> {
    let started_at = Instant::now();
    strip_body_for_head(
        &method,
        error_response(
            AppError::new(StatusCode::NOT_FOUND, "NOT_FOUND", "Route not found"),
            started_at,
        ),
    )
}

async fn method_not_allowed(method: Method) -> Response<Body> {
    let started_at = Instant::now();
    strip_body_for_head(
        &method,
        error_response(
            AppError::new(
                StatusCode::METHOD_NOT_ALLOWED,
                "METHOD_NOT_ALLOWED",
                "Only GET and HEAD are supported",
            ),
            started_at,
        ),
    )
}

async fn unfurl(
    State(state): State<AppState>,
    method: Method,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response<Body> {
    let started_at = Instant::now();
    let mut timings = RequestTimings::default();
    let result = unfurl_inner(state, headers, params, started_at, &mut timings).await;
    let mut response = match result {
        Ok(response) => response,
        Err(error) => error_response(error, started_at),
    };
    apply_server_timing_header(&mut response, &timings);
    strip_body_for_head(&method, response)
}

async fn unfurl_inner(
    state: AppState,
    headers: HeaderMap,
    params: HashMap<String, String>,
    started_at: Instant,
    timings: &mut RequestTimings,
) -> Result<Response<Body>, AppError> {
    let raw_target_url = params.get("url").ok_or_else(|| {
        AppError::new(
            StatusCode::BAD_REQUEST,
            "MISSING_QUERY_PARAM",
            "Query parameter url is required",
        )
    })?;
    let force = parse_boolean_param(params.get("force").map(String::as_str), false)?;
    let ttl = parse_number_param(
        "ttl",
        params.get("ttl").map(String::as_str),
        Some(60),
        Some(604800),
        Some(state.config.og_cache_ttl),
    )?;
    let cache_key = build_unfurl_cache_key(raw_target_url)?;

    if !force {
        let cache_lookup_started_at = Instant::now();
        if let Some(mut read) = state.cache.get(&cache_key).await? {
            timings.cache_read_ms = Some(elapsed_ms(cache_lookup_started_at));
            let origin = request_origin(&headers, state.config.public_base_url.as_deref());
            let target_url = normalize_target_url(raw_target_url)?;
            rewrite_assets_to_local_proxy_and_warm_cache(
                &state,
                &mut read.envelope.data,
                &origin,
                &target_url,
                false,
            );
            if read.is_stale {
                let target_url = normalize_target_url(raw_target_url)?;
                let origin = request_origin(&headers, state.config.public_base_url.as_deref());
                spawn_unfurl_cache_refresh(
                    state.clone(),
                    cache_key.clone(),
                    target_url,
                    origin,
                    ttl,
                );
                return Ok(success_response(
                    read.envelope.data,
                    CacheStatus::Hit,
                    started_at,
                    state.config.api_response_cache_ttl,
                    &[
                        ("x-cache-source", state.cache.label()),
                        ("x-cache-stale", "1"),
                        ("x-cache-refresh", "async"),
                    ],
                ));
            }
            return Ok(success_response(
                read.envelope.data,
                CacheStatus::Hit,
                started_at,
                state.config.api_response_cache_ttl,
                &[("x-cache-source", state.cache.label())],
            ));
        }
        timings.cache_read_ms = Some(elapsed_ms(cache_lookup_started_at));
    }

    let target_url = normalize_target_url(raw_target_url)?;
    let origin = request_origin(&headers, state.config.public_base_url.as_deref());
    let data = fetch_and_store_unfurl(
        &state,
        &cache_key,
        &target_url,
        &origin,
        ttl,
        force,
        Some(timings),
    )
    .await?;
    Ok(success_response(
        data,
        CacheStatus::Miss,
        started_at,
        state.config.api_response_cache_ttl,
        &[("x-cache-source", "origin")],
    ))
}

fn spawn_unfurl_cache_refresh(
    state: AppState,
    cache_key: String,
    target_url: String,
    origin: String,
    ttl: u64,
) {
    let Some(guard) = RefreshGuard::acquire(&cache_key) else {
        return;
    };
    tokio::spawn(async move {
        let _guard = guard;
        if let Err(error) =
            fetch_and_store_unfurl(&state, &cache_key, &target_url, &origin, ttl, false, None).await
        {
            tracing::warn!(
                error = ?error,
                target_url,
                "async metadata cache refresh failed"
            );
        }
    });
}

async fn fetch_and_store_unfurl(
    state: &AppState,
    cache_key: &str,
    target_url: &str,
    origin: &str,
    ttl: u64,
    force: bool,
    mut timings: Option<&mut RequestTimings>,
) -> Result<UnfurlData, AppError> {
    let _key_guard = cache_key_lock(cache_key).await;
    if !force && let Some(read) = state.cache.get(cache_key).await? {
        if !read.is_stale {
            let mut data = read.envelope.data;
            rewrite_assets_to_local_proxy_and_warm_cache(
                state, &mut data, origin, target_url, false,
            );
            return Ok(data);
        }
    }
    let _miss_permit = tokio::time::timeout(
        std::time::Duration::from_millis(state.config.fetch_timeout_ms),
        state.api_miss_limiter.clone().acquire_owned(),
    )
    .await
    .map_err(|_| {
        AppError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "API_BUSY",
            "Metadata processing is busy",
        )
    })?
    .map_err(|_| AppError::internal_with_message("api miss limiter closed"))?;
    let fetch_started_at = Instant::now();
    let upstream_response =
        fetch_page(&state.client, target_url, state.config.fetch_timeout_ms).await?;
    if let Some(timings) = timings.as_mut() {
        timings.fetch_upstream_ms = Some(elapsed_ms(fetch_started_at));
    }
    let status = StatusCode::from_u16(upstream_response.status().as_u16())
        .unwrap_or(StatusCode::BAD_GATEWAY);
    if !upstream_response.status().is_success() {
        return Err(AppError::new(
            status,
            "UPSTREAM_FETCH_ERROR",
            format!("Origin returned {status}"),
        ));
    }

    let content_type = upstream_response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !content_type.contains("text/html") && !content_type.contains("application/xhtml+xml") {
        return Err(AppError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "UNSUPPORTED_CONTENT_TYPE",
            "Only HTML pages can be unfurled",
        ));
    }

    let html = read_response_body(upstream_response, MAX_HTML_BYTES).await?;
    let html = String::from_utf8_lossy(&html);
    let metadata = extract_head_metadata(&html);
    let mut data = merge_meta_tags(&metadata, target_url);
    let cache_write_started_at = Instant::now();
    state.cache.set(cache_key, &data, ttl).await?;
    rewrite_assets_to_local_proxy_and_warm_cache(state, &mut data, origin, target_url, true);
    if let Some(timings) = timings.as_mut() {
        timings.cache_write_ms = Some(elapsed_ms(cache_write_started_at));
    }
    Ok(data)
}

fn rewrite_assets_to_local_proxy_and_warm_cache(
    state: &AppState,
    data: &mut UnfurlData,
    origin: &str,
    referer_url: &str,
    warm_cache: bool,
) {
    let mut warm_targets = Vec::new();
    let image_request = metadata_image_request();

    if let Some(image) = data.image.as_mut() {
        let source_url = image.url.clone();
        let local_url =
            build_image_proxy_url_for_request(origin, &source_url, referer_url, &image_request);
        image.url = local_url.clone();
        image.proxy = Some(local_url);
        warm_targets.push(source_url);
    }
    if let Some(logo) = data.logo.as_mut() {
        let source_url = logo.url.clone();
        let local_url =
            build_image_proxy_url_for_request(origin, &source_url, referer_url, &image_request);
        logo.url = local_url.clone();
        logo.proxy = Some(local_url);
        if !warm_targets.contains(&source_url) {
            warm_targets.push(source_url);
        }
    }

    for target_url in warm_targets.into_iter().filter(|_| warm_cache) {
        spawn_image_cache_warm_if_needed(
            state.clone(),
            target_url,
            Some(referer_url.to_string()),
            image_request.clone(),
        );
    }
}

fn metadata_image_request() -> ImageRequest {
    ImageRequest {
        width: None,
        height: None,
        quality: DEFAULT_IMAGE_QUALITY,
        format: ImageFormat::Jpeg,
        fit: DEFAULT_IMAGE_FIT,
    }
}

async fn image_proxy(
    State(state): State<AppState>,
    method: Method,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response<Body> {
    let started_at = Instant::now();
    let mut timings = RequestTimings::default();
    let result = image_proxy_inner(state, headers, params, &mut timings).await;
    let mut response = match result {
        Ok(mut response) => {
            response.headers_mut().insert(
                "x-response-time",
                header::HeaderValue::from_str(&format!("{}ms", started_at.elapsed().as_millis()))
                    .unwrap_or_else(|_| header::HeaderValue::from_static("")),
            );
            response
        }
        Err(error) => error_response(error, started_at),
    };
    apply_server_timing_header(&mut response, &timings);
    strip_body_for_head(&method, response)
}

async fn image_proxy_inner(
    state: AppState,
    headers: HeaderMap,
    params: HashMap<String, String>,
    timings: &mut RequestTimings,
) -> Result<Response<Body>, AppError> {
    let raw_target_url = params.get("url").ok_or_else(|| {
        AppError::new(
            StatusCode::BAD_REQUEST,
            "MISSING_QUERY_PARAM",
            "Query parameter url is required",
        )
    })?;
    let target_url = validate_public_url(raw_target_url)?.to_string();
    let referer = params
        .get("referer")
        .map(String::as_str)
        .map(validate_public_url)
        .transpose()?
        .map(|url| url.to_string());
    let width = parse_optional_number_param(
        "w",
        params.get("w").map(String::as_str),
        Some(1),
        Some(4096),
    )?;
    let height = parse_optional_number_param(
        "h",
        params.get("h").map(String::as_str),
        Some(1),
        Some(4096),
    )?;
    let quality = clamp_quality(parse_number_param(
        "q",
        params.get("q").map(String::as_str),
        Some(1),
        Some(100),
        Some(80),
    )?);
    let requested_format = parse_image_format(params.get("f").map(String::as_str))?;
    let fit = parse_image_fit(params.get("fit").map(String::as_str))?;
    let negotiated_format = choose_image_format(
        headers
            .get(header::ACCEPT)
            .and_then(|value| value.to_str().ok()),
        &requested_format,
    );
    let image_request = ImageRequest {
        width,
        height,
        quality,
        format: negotiated_format.clone(),
        fit,
    };
    let cache_target = build_image_cache_target(&state, target_url, referer, image_request);

    let cache_lookup_started_at = Instant::now();
    if let Some(read) = state
        .image_cache
        .get(&cache_target.cache_key, &cache_target.object_key)
        .await?
    {
        timings.cache_read_ms = Some(elapsed_ms(cache_lookup_started_at));
        if read.is_stale {
            spawn_image_cache_refresh(state.clone(), cache_target);
        }
        return image_cache_hit_response(
            read.hit,
            state.config.image_cache_ttl,
            state.config.image_browser_cache_ttl,
            CacheStatus::Hit,
            state.image_cache.label(),
            read.is_stale,
        );
    }
    timings.cache_read_ms = Some(elapsed_ms(cache_lookup_started_at));

    let cached = fetch_and_store_image(&state, cache_target, Some(timings)).await?;

    image_cache_hit_response(
        cached,
        state.config.image_cache_ttl,
        state.config.image_browser_cache_ttl,
        CacheStatus::Miss,
        "origin",
        false,
    )
}

#[derive(Clone)]
struct ImageCacheTarget {
    target_url: String,
    referer: Option<String>,
    request: ImageRequest,
    cache_key: String,
    object_key: String,
}

fn build_image_cache_target(
    state: &AppState,
    target_url: String,
    referer: Option<String>,
    request: ImageRequest,
) -> ImageCacheTarget {
    let cache_key = build_processed_image_cache_key(&target_url, referer.as_deref(), &request);
    let object_key =
        build_processed_image_object_key(&state.config.s3_prefix, &cache_key, &request.format);
    ImageCacheTarget {
        target_url,
        referer,
        request,
        cache_key,
        object_key,
    }
}

fn spawn_image_cache_refresh(state: AppState, target: ImageCacheTarget) {
    let Some(guard) = RefreshGuard::acquire(&target.cache_key) else {
        return;
    };
    tokio::spawn(async move {
        let _guard = guard;
        let target_url = target.target_url.clone();
        if let Err(error) = fetch_and_store_image(&state, target, None).await {
            tracing::warn!(
                error = ?error,
                target_url,
                "async image cache refresh failed"
            );
        }
    });
}

fn spawn_image_cache_warm_if_needed(
    state: AppState,
    target_url: String,
    referer: Option<String>,
    request: ImageRequest,
) {
    tokio::spawn(async move {
        let target = build_image_cache_target(&state, target_url.clone(), referer, request);
        match state
            .image_cache
            .get(&target.cache_key, &target.object_key)
            .await
        {
            Ok(Some(read)) if !read.is_stale => {}
            Ok(_) => {
                if let Err(error) = fetch_and_store_image(&state, target, None).await {
                    tracing::warn!(
                        error = ?error,
                        target_url,
                        "metadata image cache warm failed"
                    );
                }
            }
            Err(error) => {
                tracing::warn!(
                    error = ?error,
                    target_url,
                    "metadata image cache lookup failed"
                );
            }
        }
    });
}

async fn fetch_and_store_image(
    state: &AppState,
    target: ImageCacheTarget,
    mut timings: Option<&mut RequestTimings>,
) -> Result<ImageCacheHit, AppError> {
    let _key_guard = cache_key_lock(&target.cache_key).await;
    if let Some(read) = state
        .image_cache
        .get(&target.cache_key, &target.object_key)
        .await?
    {
        if !read.is_stale {
            return Ok(read.hit);
        }
    }
    // Metadata warming and stale-cache refresh bypass the public image route.
    // Validate here as well: literal IPs do not pass through the DNS resolver.
    let upstream_url = validate_public_url(&target.target_url)?;
    let _miss_permit = tokio::time::timeout(
        std::time::Duration::from_millis(state.config.fetch_timeout_ms),
        state.image_miss_limiter.clone().acquire_owned(),
    )
    .await
    .map_err(|_| {
        AppError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "IMAGE_BUSY",
            "Image processing is busy",
        )
    })?
    .map_err(|_| AppError::internal_with_message("image miss limiter closed"))?;
    let fetch_started_at = Instant::now();
    let mut request = state.client.get(upstream_url).header(
        header::ACCEPT,
        "image/avif,image/webp,image/jpeg,image/png,image/*;q=0.8,*/*;q=0.5",
    );
    if let Some(referer) = target.referer.as_deref() {
        request = request.header(header::REFERER, referer);
    }
    let upstream_response = request
        .timeout(std::time::Duration::from_millis(
            state.config.fetch_timeout_ms,
        ))
        .send()
        .await
        .map_err(|error| {
            AppError::new(
                StatusCode::BAD_GATEWAY,
                "FETCH_FAILED",
                format!("Unable to fetch image: {error}"),
            )
        })?;
    if let Some(timings) = timings.as_mut() {
        timings.fetch_upstream_ms = Some(elapsed_ms(fetch_started_at));
    }
    let status = StatusCode::from_u16(upstream_response.status().as_u16())
        .unwrap_or(StatusCode::BAD_GATEWAY);
    if !upstream_response.status().is_success() {
        return Err(AppError::new(
            status,
            "UPSTREAM_FETCH_ERROR",
            format!("Image origin returned {status}"),
        ));
    }

    let content_type = ensure_image_content_type(
        upstream_response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
    )?
    .to_string();
    let bytes = read_response_body(upstream_response, MAX_IMAGE_BYTES).await?;
    // Guard originals before a failing helper can fall back or an S3 cache can redirect.
    validate_raster(&bytes, &content_type)?;
    let transform_started_at = Instant::now();
    let processed = if state.config.low_memory_mode {
        process_image_with_helper(
            state.config.image_worker_bin.as_ref(),
            &bytes,
            &content_type,
            &target.request,
            state.config.fetch_timeout_ms,
        )
        .await
        .unwrap_or_else(|error| {
            tracing::warn!(
                code = error.code,
                "image worker failed; serving original image"
            );
            crate::models::ProcessedImage {
                bytes: bytes.to_vec(),
                content_type: content_type.clone(),
                optimized: false,
            }
        })
    } else {
        let request_for_transform = target.request.clone();
        let content_type_for_transform = content_type.clone();
        let bytes_for_transform = bytes.to_vec();
        tokio::task::spawn_blocking(move || {
            process_image(
                &bytes_for_transform,
                &content_type_for_transform,
                &request_for_transform,
            )
        })
        .await
        .map_err(|error| {
            AppError::internal_with_message(format!("image transform task join failed: {error}"))
        })??
    };
    if let Some(timings) = timings.as_mut() {
        timings.transform_ms = Some(elapsed_ms(transform_started_at));
    }

    validate_raster(&processed.bytes, &processed.content_type)?;
    let cache_write_started_at = Instant::now();
    let cached = state
        .image_cache
        .put(ImageCacheWrite {
            cache_key: target.cache_key,
            object_key: target.object_key,
            bytes: Bytes::from(processed.bytes),
            content_type: processed.content_type,
            optimized: processed.optimized,
            ttl: state.config.image_cache_ttl,
        })
        .await?;
    if let Some(timings) = timings.as_mut() {
        timings.cache_write_ms = Some(elapsed_ms(cache_write_started_at));
    }

    Ok(cached)
}

async fn read_response_body(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Bytes, AppError> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(AppError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "UPSTREAM_BODY_TOO_LARGE",
            "Upstream response exceeds the configured size limit",
        ));
    }
    let mut body =
        Vec::with_capacity(response.content_length().unwrap_or(0).min(limit as u64) as usize);
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        AppError::new(
            StatusCode::BAD_GATEWAY,
            "FETCH_FAILED",
            format!("Unable to read upstream body: {error}"),
        )
    })? {
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(AppError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "UPSTREAM_BODY_TOO_LARGE",
                "Upstream response exceeds the configured size limit",
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(body))
}

fn image_cache_hit_response(
    hit: ImageCacheHit,
    image_cache_ttl: u64,
    image_browser_cache_ttl: u64,
    cache_status: CacheStatus,
    cache_source: &str,
    is_stale: bool,
) -> Result<Response<Body>, AppError> {
    match hit {
        ImageCacheHit::Inline(image) => {
            validate_raster(&image.bytes, &image.content_type)?;
            let mut response = Response::new(Body::from(image.bytes));
            *response.status_mut() = StatusCode::OK;
            let headers = response.headers_mut();
            headers.insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_str(&image.content_type).map_err(|error| {
                    AppError::internal_with_message(format!(
                        "failed to set image content-type header: {error}"
                    ))
                })?,
            );
            headers.insert(
                header::CACHE_CONTROL,
                header::HeaderValue::from_str(&format!(
                    "public, max-age={}, immutable",
                    image_browser_cache_ttl
                ))
                .map_err(|error| {
                    AppError::internal_with_message(format!(
                        "failed to set image cache-control header: {error}"
                    ))
                })?,
            );
            headers.insert(
                "cloudflare-cdn-cache-control",
                header::HeaderValue::from_str(&format!("public, s-maxage={image_cache_ttl}"))
                    .map_err(|error| {
                        AppError::internal_with_message(format!(
                            "failed to set image cloudflare cache-control header: {error}"
                        ))
                    })?,
            );
            headers.insert(
                "x-content-type-options",
                header::HeaderValue::from_static("nosniff"),
            );
            headers.insert(
                "content-security-policy",
                header::HeaderValue::from_static("sandbox; default-src 'none'"),
            );
            headers.insert(header::VARY, header::HeaderValue::from_static("Accept"));
            headers.insert(
                "x-image-optimized",
                header::HeaderValue::from_static(if image.optimized { "1" } else { "0" }),
            );
            headers.insert(
                "x-cache-status",
                header::HeaderValue::from_static(match cache_status {
                    CacheStatus::Hit => "HIT",
                    CacheStatus::Miss => "MISS",
                }),
            );
            headers.insert(
                "x-cache-source",
                header::HeaderValue::from_str(cache_source).map_err(|error| {
                    AppError::internal_with_message(format!(
                        "failed to set image cache source header: {error}"
                    ))
                })?,
            );
            if is_stale {
                headers.insert("x-cache-stale", header::HeaderValue::from_static("1"));
                headers.insert("x-cache-refresh", header::HeaderValue::from_static("async"));
            }
            Ok(response)
        }
        ImageCacheHit::Redirect { location } => {
            let mut response = Response::new(Body::empty());
            *response.status_mut() = StatusCode::FOUND;
            let headers = response.headers_mut();
            headers.insert(
                header::LOCATION,
                header::HeaderValue::from_str(&location).map_err(|error| {
                    AppError::internal_with_message(format!(
                        "failed to set redirect location header: {error}"
                    ))
                })?,
            );
            headers.insert(
                header::CACHE_CONTROL,
                header::HeaderValue::from_str(&format!(
                    "public, max-age={}, immutable",
                    image_browser_cache_ttl
                ))
                .map_err(|error| {
                    AppError::internal_with_message(format!(
                        "failed to set redirect cache-control header: {error}"
                    ))
                })?,
            );
            headers.insert(
                "cloudflare-cdn-cache-control",
                header::HeaderValue::from_str(&format!("public, s-maxage={image_cache_ttl}"))
                    .map_err(|error| {
                        AppError::internal_with_message(format!(
                            "failed to set redirect cloudflare cache-control header: {error}"
                        ))
                    })?,
            );
            headers.insert(
                "x-cache-status",
                header::HeaderValue::from_static(match cache_status {
                    CacheStatus::Hit => "HIT",
                    CacheStatus::Miss => "MISS",
                }),
            );
            headers.insert(
                "x-cache-source",
                header::HeaderValue::from_str(cache_source).map_err(|error| {
                    AppError::internal_with_message(format!(
                        "failed to set image cache source header: {error}"
                    ))
                })?,
            );
            if is_stale {
                headers.insert("x-cache-stale", header::HeaderValue::from_static("1"));
                headers.insert("x-cache-refresh", header::HeaderValue::from_static("async"));
            }
            Ok(response)
        }
    }
}

#[derive(Default)]
struct RequestTimings {
    cache_read_ms: Option<f64>,
    fetch_upstream_ms: Option<f64>,
    transform_ms: Option<f64>,
    cache_write_ms: Option<f64>,
}

fn elapsed_ms(started_at: Instant) -> f64 {
    started_at.elapsed().as_secs_f64() * 1000.0
}

fn apply_server_timing_header(response: &mut Response<Body>, timings: &RequestTimings) {
    let mut parts = Vec::new();
    if let Some(value) = timings.cache_read_ms {
        parts.push(format!("cache-read;dur={value:.3}"));
    }
    if let Some(value) = timings.fetch_upstream_ms {
        parts.push(format!("fetch-upstream;dur={value:.3}"));
    }
    if let Some(value) = timings.transform_ms {
        parts.push(format!("transform;dur={value:.3}"));
    }
    if let Some(value) = timings.cache_write_ms {
        parts.push(format!("cache-write;dur={value:.3}"));
    }
    if parts.is_empty() {
        return;
    }
    if let Ok(value) = header::HeaderValue::from_str(&parts.join(", ")) {
        response.headers_mut().insert("server-timing", value);
    }
}

#[cfg(test)]
mod upstream_guard_tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::Semaphore;

    #[tokio::test]
    async fn background_image_fetch_rejects_private_literals_before_network_io() {
        let state = AppState {
            config: crate::config::Config::from_env().unwrap(),
            client: reqwest::Client::new(),
            cache: Arc::new(crate::cache::NoopCache),
            image_cache: Arc::new(crate::image_cache::NoopImageCache),
            api_miss_limiter: Arc::new(Semaphore::new(1)),
            image_miss_limiter: Arc::new(Semaphore::new(1)),
        };
        for url in [
            "http://127.0.0.1:1/image.png",
            "http://[::ffff:127.0.0.1]:1/image.png",
        ] {
            let target =
                build_image_cache_target(&state, url.to_string(), None, metadata_image_request());
            let error = fetch_and_store_image(&state, target, None)
                .await
                .unwrap_err();
            assert_eq!(error.code, "PRIVATE_IP");
        }
    }
}

#[cfg(test)]
mod response_budget_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn upstream(body: Vec<u8>, headers: String) -> reqwest::Response {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            let _ = stream.read(&mut request).await.unwrap();
            stream
                .write_all(
                    format!("HTTP/1.1 200 OK\r\nConnection: close\r\n{headers}\r\n").as_bytes(),
                )
                .await
                .unwrap();
            stream.write_all(&body).await.unwrap();
            stream.shutdown().await.unwrap();
        });
        reqwest::Client::new()
            .get(format!("http://{address}/"))
            .send()
            .await
            .unwrap()
    }
    #[tokio::test]
    async fn enforces_streamed_bytes_without_content_length() {
        for length in [1024, 1025] {
            let mut body = format!("{length:x}\r\n").into_bytes();
            body.extend(vec![b'x'; length]);
            body.extend_from_slice(b"\r\n0\r\n\r\n");
            let response = upstream(body, "Transfer-Encoding: chunked\r\n".into()).await;
            let result = read_response_body(response, 1024).await;
            if length == 1024 {
                assert_eq!(result.unwrap().len(), 1024)
            } else {
                assert_eq!(result.unwrap_err().status, StatusCode::PAYLOAD_TOO_LARGE)
            }
        }
    }
    #[tokio::test]
    async fn compressed_content_length_cannot_bypass_expanded_limit() {
        let body = vec![
            31, 139, 8, 0, 0, 0, 0, 0, 2, 255, 237, 193, 1, 13, 0, 0, 0, 194, 160, 218, 143, 111,
            15, 7, 20, 0, 0, 0, 240, 110, 193, 119, 16, 62, 0, 16, 0, 0,
        ];
        let headers = format!(
            "Content-Encoding: gzip\r\nContent-Length: {}\r\n",
            body.len()
        );
        assert!(body.len() < 1024);
        let response = upstream(body, headers).await;
        assert_eq!(
            read_response_body(response, 1024).await.unwrap_err().status,
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }
}

#[cfg(test)]
mod singleflight_tests {
    use super::*;
    #[test]
    fn cached_inline_images_are_revalidated_and_sandboxed() {
        use crate::models::CachedImage;
        let malicious = ImageCacheHit::Inline(CachedImage {
            bytes: Bytes::from_static(b"<svg><script>void 0</script></svg>"),
            content_type: "image/svg+xml".into(),
            optimized: false,
        });
        assert_eq!(
            image_cache_hit_response(malicious, 60, 60, CacheStatus::Hit, "sqlite", false)
                .unwrap_err()
                .status,
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        );
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(1, 1)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let valid = ImageCacheHit::Inline(CachedImage {
            bytes: png.into_inner().into(),
            content_type: "image/png".into(),
            optimized: false,
        });
        let response =
            image_cache_hit_response(valid, 60, 60, CacheStatus::Hit, "sqlite", false).unwrap();
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        assert_eq!(
            response.headers()["content-security-policy"],
            "sandbox; default-src 'none'"
        );
    }

    #[test]
    fn refresh_ownership_coalesces_and_recovers_after_failure() {
        let owner = RefreshGuard::acquire("test:same").unwrap();
        for _ in 0..100 {
            assert!(RefreshGuard::acquire("test:same").is_none());
        }
        assert!(RefreshGuard::acquire("test:other").is_some());
        drop(owner);
        assert!(RefreshGuard::acquire("test:same").is_some());
    }
}
