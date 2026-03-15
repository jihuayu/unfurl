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

use crate::{
    error::AppError,
    extract::{extract_head_metadata, merge_meta_tags},
    fetcher::fetch_page,
    image_proxy::process_image,
    image_worker::process_image_with_helper,
    models::{CacheStatus, ImageCacheHit, ImageCacheWrite, ImageRequest},
    state::AppState,
    utils::{
        build_image_proxy_url, build_processed_image_cache_key, build_processed_image_object_key,
        build_unfurl_cache_key, choose_image_format, clamp_quality, ensure_image_content_type,
        error_response, health_response, parse_boolean_param, parse_image_fit, parse_image_format,
        parse_number_param, parse_optional_number_param, request_origin, strip_body_for_head,
        success_response, validate_public_url,
    },
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/health", get(health))
        .route("/api", get(unfurl))
        .route("/proxy/image", get(image_proxy))
}

async fn health(method: Method) -> impl IntoResponse {
    strip_body_for_head(&method, health_response(Instant::now()))
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
        if let Some(envelope) = state.cache.get(&cache_key).await? {
            timings.cache_read_ms = Some(elapsed_ms(cache_lookup_started_at));
            return Ok(success_response(
                envelope.data,
                CacheStatus::Hit,
                started_at,
                state.config.api_response_cache_ttl,
                &[("x-cache-source", state.cache.label())],
            ));
        }
        timings.cache_read_ms = Some(elapsed_ms(cache_lookup_started_at));
    }

    let target_url = validate_public_url(raw_target_url)?.to_string();
    let _miss_permit = state
        .api_miss_limiter
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| AppError::internal_with_message("api miss limiter closed"))?;
    let fetch_started_at = Instant::now();
    let upstream_response =
        fetch_page(&state.client, &target_url, state.config.fetch_timeout_ms).await?;
    timings.fetch_upstream_ms = Some(elapsed_ms(fetch_started_at));
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

    let html = upstream_response.text().await.map_err(|error| {
        AppError::new(
            StatusCode::BAD_GATEWAY,
            "FETCH_FAILED",
            format!("Unable to read upstream body: {error}"),
        )
    })?;
    let metadata = extract_head_metadata(&html);
    let mut data = merge_meta_tags(&metadata, &target_url);
    let origin = request_origin(&headers);

    if let Some(image) = data.image.as_mut() {
        image.proxy = Some(build_image_proxy_url(&origin, &image.url, &target_url));
    }
    if let Some(logo) = data.logo.as_mut() {
        logo.proxy = Some(build_image_proxy_url(&origin, &logo.url, &target_url));
    }

    let cache_write_started_at = Instant::now();
    state.cache.set(&cache_key, &data, ttl).await?;
    timings.cache_write_ms = Some(elapsed_ms(cache_write_started_at));
    Ok(success_response(
        data,
        CacheStatus::Miss,
        started_at,
        state.config.api_response_cache_ttl,
        &[("x-cache-source", "origin")],
    ))
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
    let image_cache_key =
        build_processed_image_cache_key(&target_url, referer.as_deref(), &image_request);
    let image_object_key = build_processed_image_object_key(
        &state.config.s3_prefix,
        &image_cache_key,
        &negotiated_format,
    );

    let cache_lookup_started_at = Instant::now();
    if let Some(hit) = state
        .image_cache
        .get(&image_cache_key, &image_object_key)
        .await?
    {
        timings.cache_read_ms = Some(elapsed_ms(cache_lookup_started_at));
        return image_cache_hit_response(
            hit,
            state.config.image_cache_ttl,
            CacheStatus::Hit,
            state.image_cache.label(),
        );
    }
    timings.cache_read_ms = Some(elapsed_ms(cache_lookup_started_at));

    let _miss_permit = state
        .image_miss_limiter
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| AppError::internal_with_message("image miss limiter closed"))?;
    let fetch_started_at = Instant::now();
    let mut request = state.client.get(&target_url).header(
        header::ACCEPT,
        "image/avif,image/webp,image/jpeg,image/png,image/*;q=0.8,*/*;q=0.5",
    );
    if let Some(referer) = referer.as_deref() {
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
    timings.fetch_upstream_ms = Some(elapsed_ms(fetch_started_at));
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
    let bytes = upstream_response.bytes().await.map_err(|error| {
        AppError::new(
            StatusCode::BAD_GATEWAY,
            "FETCH_FAILED",
            format!("Unable to read image body: {error}"),
        )
    })?;
    let transform_started_at = Instant::now();
    let processed = if state.config.low_memory_mode {
        process_image_with_helper(
            state.config.image_worker_bin.as_ref(),
            &bytes,
            &content_type,
            &image_request,
            state.config.fetch_timeout_ms,
        )
        .await?
    } else {
        let request_for_transform = image_request.clone();
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
    timings.transform_ms = Some(elapsed_ms(transform_started_at));

    let cache_write_started_at = Instant::now();
    let cached = state
        .image_cache
        .put(ImageCacheWrite {
            cache_key: image_cache_key,
            object_key: image_object_key,
            bytes: Bytes::from(processed.bytes),
            content_type: processed.content_type,
            optimized: processed.optimized,
            ttl: state.config.image_cache_ttl,
        })
        .await?;
    timings.cache_write_ms = Some(elapsed_ms(cache_write_started_at));

    image_cache_hit_response(
        cached,
        state.config.image_cache_ttl,
        CacheStatus::Miss,
        "origin",
    )
}

fn image_cache_hit_response(
    hit: ImageCacheHit,
    image_cache_ttl: u64,
    cache_status: CacheStatus,
    cache_source: &str,
) -> Result<Response<Body>, AppError> {
    match hit {
        ImageCacheHit::Inline(image) => {
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
                    image_cache_ttl
                ))
                .map_err(|error| {
                    AppError::internal_with_message(format!(
                        "failed to set image cache-control header: {error}"
                    ))
                })?,
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
                    image_cache_ttl
                ))
                .map_err(|error| {
                    AppError::internal_with_message(format!(
                        "failed to set redirect cache-control header: {error}"
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
