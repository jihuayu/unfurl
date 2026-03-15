use std::{collections::HashMap, time::Instant};

use axum::{
    Router,
    body::Body,
    extract::{Query, State},
    http::{HeaderMap, Method, Response, StatusCode, header},
    response::IntoResponse,
    routing::get,
};

use crate::{
    error::AppError,
    extract::{extract_head_metadata, merge_meta_tags},
    fetcher::fetch_page,
    image_proxy::process_image,
    models::{CacheStatus, ImageRequest},
    state::AppState,
    utils::{
        build_image_proxy_url, build_unfurl_cache_key, choose_image_format, clamp_quality,
        ensure_image_content_type, error_response, health_response, parse_boolean_param,
        parse_image_fit, parse_image_format, parse_number_param, parse_optional_number_param,
        request_origin, strip_body_for_head, success_response, validate_public_url,
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
    let result = unfurl_inner(state, headers, params, started_at).await;
    strip_body_for_head(
        &method,
        match result {
            Ok(response) => response,
            Err(error) => error_response(error, started_at),
        },
    )
}

async fn unfurl_inner(
    state: AppState,
    headers: HeaderMap,
    params: HashMap<String, String>,
    started_at: Instant,
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

    if !force && let Some(envelope) = state.cache.get(&cache_key).await? {
        return Ok(success_response(
            envelope.data,
            CacheStatus::Hit,
            started_at,
            state.config.api_response_cache_ttl,
            &[("x-cache-source", state.cache.label())],
        ));
    }

    let target_url = validate_public_url(raw_target_url)?.to_string();
    let upstream_response =
        fetch_page(&state.client, &target_url, state.config.fetch_timeout_ms).await?;
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

    state.cache.set(&cache_key, &data, ttl).await?;
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
    let result = image_proxy_inner(state, headers, params).await;
    strip_body_for_head(
        &method,
        match result {
            Ok(response) => response,
            Err(error) => error_response(error, started_at),
        },
    )
}

async fn image_proxy_inner(
    state: AppState,
    headers: HeaderMap,
    params: HashMap<String, String>,
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
    let processed = process_image(
        &bytes,
        &content_type,
        &ImageRequest {
            width,
            height,
            quality,
            format: negotiated_format,
            fit,
        },
    )?;

    let mut response = Response::new(Body::from(processed.bytes));
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static(processed.content_type),
    );
    headers.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_str(&format!(
            "public, max-age={}, immutable",
            state.config.image_cache_ttl
        ))
        .unwrap(),
    );
    headers.insert(header::VARY, header::HeaderValue::from_static("Accept"));
    headers.insert(
        "x-image-optimized",
        header::HeaderValue::from_static(if processed.optimized { "1" } else { "0" }),
    );
    Ok(response)
}
