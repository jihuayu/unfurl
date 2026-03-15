use std::{cmp::min, net::IpAddr, time::Instant};

use axum::{
    body::Body,
    http::{HeaderMap, HeaderName, HeaderValue, Method, Response, StatusCode, header},
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use url::Url;

use crate::{
    error::AppError,
    models::{
        ApiErrorResponse, ApiErrorShape, ApiSuccessResponse, CacheStatus, ImageFit, ImageFormat,
        ImageRequest, ResponseHeadersShape, UnfurlData,
    },
};

const LOCAL_SUFFIXES: [&str; 3] = [".local", ".internal", ".localhost"];

pub const DEFAULT_IMAGE_QUALITY: u8 = 80;
pub const DEFAULT_IMAGE_FIT: ImageFit = ImageFit::ScaleDown;

pub fn success_response(
    data: UnfurlData,
    cache_status: CacheStatus,
    started_at: Instant,
    cache_control_ttl: u64,
    extra_headers: &[(&str, &str)],
) -> Response<Body> {
    let response_time = format!("{}ms", started_at.elapsed().as_millis());
    let body = ApiSuccessResponse {
        status: "success",
        data,
        headers: ResponseHeadersShape {
            cache_status: Some(cache_status),
            response_time: response_time.clone(),
        },
    };

    json_response_pretty(StatusCode::OK, &body, |headers| {
        headers.insert(
            header::CACHE_CONTROL,
            header_value(&format!("public, max-age={cache_control_ttl}")),
        );
        headers.insert("x-response-time", header_value(&response_time));
        headers.insert(
            "x-cache-status",
            HeaderValue::from_static(match cache_status {
                CacheStatus::Hit => "HIT",
                CacheStatus::Miss => "MISS",
            }),
        );
        for (key, value) in extra_headers {
            if let Ok(header_name) = HeaderName::from_bytes(key.as_bytes()) {
                headers.insert(header_name, header_value(value));
            }
        }
    })
}

pub fn error_response(error: AppError, started_at: Instant) -> Response<Body> {
    let response_time = format!("{}ms", started_at.elapsed().as_millis());
    let body = ApiErrorResponse {
        status: "error",
        error: ApiErrorShape {
            code: error.code,
            message: error.message,
        },
        headers: Some(ResponseHeadersShape {
            cache_status: None,
            response_time: response_time.clone(),
        }),
    };

    json_response_pretty(error.status, &body, |headers| {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        headers.insert("x-response-time", header_value(&response_time));
    })
}

pub fn health_response(started_at: Instant) -> Response<Body> {
    let response_time = format!("{}ms", started_at.elapsed().as_millis());
    let body = ApiSuccessResponse {
        status: "success",
        data: UnfurlData {
            title: None,
            description: None,
            image: None,
            url: "health://ok".to_string(),
            author: None,
            publisher: None,
            date: None,
            lang: None,
            logo: None,
            video: None,
            audio: None,
        },
        headers: ResponseHeadersShape {
            cache_status: Some(CacheStatus::Miss),
            response_time: response_time.clone(),
        },
    };

    json_response_pretty(StatusCode::OK, &body, |headers| {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        headers.insert("x-response-time", header_value(&response_time));
        headers.insert("x-cache-status", HeaderValue::from_static("MISS"));
    })
}

pub fn strip_body_for_head(method: &Method, response: Response<Body>) -> Response<Body> {
    if *method != Method::HEAD {
        return response;
    }

    let (parts, _) = response.into_parts();
    Response::from_parts(parts, Body::empty())
}

fn json_response_pretty<T: Serialize>(
    status: StatusCode,
    body: &T,
    header_mutator: impl FnOnce(&mut HeaderMap),
) -> Response<Body> {
    let payload =
        serde_json::to_vec_pretty(body).unwrap_or_else(|_| br#"{"status":"error"}"#.to_vec());
    let mut response = Response::new(Body::from(payload));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    header_mutator(headers);
    response
}

pub fn parse_boolean_param(value: Option<&str>, default_value: bool) -> Result<bool, AppError> {
    let Some(value) = value.filter(|value| !value.is_empty()) else {
        return Ok(default_value);
    };

    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "INVALID_BOOLEAN",
            format!("Invalid boolean value: {value}"),
        )),
    }
}

pub fn parse_number_param(
    name: &str,
    value: Option<&str>,
    min_value: Option<u64>,
    max_value: Option<u64>,
    default_value: Option<u64>,
) -> Result<u64, AppError> {
    let raw = match value.filter(|value| !value.is_empty()) {
        Some(value) => value,
        None => {
            return default_value.ok_or_else(|| {
                AppError::new(
                    StatusCode::BAD_REQUEST,
                    "MISSING_QUERY_PARAM",
                    format!("Missing required query parameter: {name}"),
                )
            });
        }
    };

    let parsed = raw.parse::<u64>().map_err(|_| {
        AppError::new(
            StatusCode::BAD_REQUEST,
            "INVALID_NUMBER",
            format!("Invalid numeric value for {name}"),
        )
    })?;
    if let Some(minimum) = min_value
        && parsed < minimum
    {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "INVALID_NUMBER",
            format!("{name} must be >= {minimum}"),
        ));
    }
    if let Some(maximum) = max_value
        && parsed > maximum
    {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "INVALID_NUMBER",
            format!("{name} must be <= {maximum}"),
        ));
    }

    Ok(parsed)
}

pub fn parse_optional_number_param(
    name: &str,
    value: Option<&str>,
    min_value: Option<u64>,
    max_value: Option<u64>,
) -> Result<Option<u32>, AppError> {
    match value.filter(|candidate| !candidate.is_empty()) {
        Some(raw) => Ok(Some(
            parse_number_param(name, Some(raw), min_value, max_value, None)? as u32,
        )),
        None => Ok(None),
    }
}

pub fn parse_image_format(value: Option<&str>) -> Result<ImageFormat, AppError> {
    let Some(value) = value.filter(|value| !value.is_empty()) else {
        return Ok(ImageFormat::Auto);
    };

    match value.trim().to_ascii_lowercase().as_str() {
        "auto" => Ok(ImageFormat::Auto),
        "avif" => Ok(ImageFormat::Avif),
        "webp" => Ok(ImageFormat::Webp),
        "jpeg" | "jpg" => Ok(ImageFormat::Jpeg),
        "png" => Ok(ImageFormat::Png),
        _ => Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "INVALID_IMAGE_FORMAT",
            format!("Unsupported image format: {value}"),
        )),
    }
}

pub fn parse_image_fit(value: Option<&str>) -> Result<ImageFit, AppError> {
    let Some(value) = value.filter(|value| !value.is_empty()) else {
        return Ok(DEFAULT_IMAGE_FIT);
    };

    match value.trim().to_ascii_lowercase().as_str() {
        "scale-down" => Ok(ImageFit::ScaleDown),
        "contain" => Ok(ImageFit::Contain),
        "cover" => Ok(ImageFit::Cover),
        "crop" => Ok(ImageFit::Crop),
        "pad" => Ok(ImageFit::Pad),
        _ => Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "INVALID_IMAGE_FIT",
            format!("Unsupported image fit: {value}"),
        )),
    }
}

pub fn sanitize_text(value: Option<&str>) -> Option<String> {
    value
        .map(|candidate| candidate.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|candidate| !candidate.is_empty())
}

pub fn to_absolute_url(value: Option<&str>, base_url: &str) -> Option<String> {
    let candidate = sanitize_text(value)?;
    Url::parse(base_url)
        .ok()
        .and_then(|base| base.join(&candidate).ok())
        .map(|url| url.to_string())
}

pub fn validate_public_url(raw_url: &str) -> Result<Url, AppError> {
    let url = Url::parse(raw_url).map_err(|_| {
        AppError::new(
            StatusCode::BAD_REQUEST,
            "INVALID_URL",
            "Invalid URL provided",
        )
    })?;

    if !matches!(url.scheme(), "http" | "https") {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "INVALID_URL_PROTOCOL",
            "Only http and https URLs are supported",
        ));
    }

    let hostname = url
        .host_str()
        .ok_or_else(|| {
            AppError::new(
                StatusCode::BAD_REQUEST,
                "INVALID_URL",
                "Invalid URL provided",
            )
        })?
        .to_ascii_lowercase();

    if hostname == "localhost"
        || LOCAL_SUFFIXES
            .iter()
            .any(|suffix| hostname.ends_with(suffix))
    {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "PRIVATE_HOST",
            "Private or local hosts are not allowed",
        ));
    }

    if is_blocked_ip_literal(&hostname) {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "PRIVATE_IP",
            "Private or loopback IPs are not allowed",
        ));
    }

    Ok(url)
}

fn is_blocked_ip_literal(hostname: &str) -> bool {
    hostname
        .parse::<IpAddr>()
        .map(|address| match address {
            IpAddr::V4(value) => value.is_private() || value.is_loopback() || value.is_link_local(),
            IpAddr::V6(value) => {
                value.is_loopback() || value.is_unique_local() || value.is_unicast_link_local()
            }
        })
        .unwrap_or(false)
}

pub fn normalize_target_url(raw_url: &str) -> Result<String, AppError> {
    let mut url = validate_public_url(raw_url)?;
    url.set_fragment(None);
    let scheme = url.scheme().to_ascii_lowercase();
    let _ = url.set_scheme(&scheme);
    if let Some(host) = url.host_str().map(|host| host.to_ascii_lowercase()) {
        let _ = url.set_host(Some(&host));
    }

    let normalized_path = normalize_pathname(url.path());
    url.set_path(&normalized_path);

    let mut query_pairs = url
        .query_pairs()
        .filter(|(key, _)| {
            let normalized = key.to_ascii_lowercase();
            !normalized.starts_with("utm_") && normalized != "fbclid" && normalized != "gclid"
        })
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect::<Vec<_>>();
    query_pairs.sort_by(|left, right| left.cmp(right));
    url.query_pairs_mut().clear();
    for (key, value) in query_pairs {
        url.query_pairs_mut().append_pair(&key, &value);
    }

    Ok(url.to_string())
}

fn normalize_pathname(pathname: &str) -> String {
    let normalized = if pathname.len() > 1 && pathname.ends_with('/') {
        &pathname[..pathname.len() - 1]
    } else {
        pathname
    };
    if normalized.is_empty() {
        "/".to_string()
    } else {
        normalized.to_string()
    }
}

pub fn build_unfurl_cache_key(raw_url: &str) -> Result<String, AppError> {
    Ok(format!("unfurl:v1:{}", normalize_target_url(raw_url)?))
}

pub fn choose_image_format(
    accept_header: Option<&str>,
    requested_format: &ImageFormat,
) -> ImageFormat {
    if *requested_format != ImageFormat::Auto {
        return requested_format.clone();
    }

    let normalized = accept_header.unwrap_or_default().to_ascii_lowercase();
    if normalized.contains("image/avif") {
        ImageFormat::Avif
    } else if normalized.contains("image/webp") {
        ImageFormat::Webp
    } else {
        ImageFormat::Jpeg
    }
}

pub fn ensure_image_content_type(content_type: Option<&str>) -> Result<&str, AppError> {
    let content_type = content_type.unwrap_or_default();
    if !content_type.to_ascii_lowercase().starts_with("image/") {
        return Err(AppError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "UNSUPPORTED_MEDIA_TYPE",
            "Origin did not return an image payload",
        ));
    }

    Ok(content_type)
}

pub fn build_image_proxy_url(origin: &str, asset_url: &str, referer_url: &str) -> String {
    format!(
        "{origin}/proxy/image?url={}&referer={}",
        urlencoding::encode(asset_url),
        urlencoding::encode(referer_url)
    )
}

pub fn build_processed_image_cache_key(
    target_url: &str,
    referer: Option<&str>,
    request: &ImageRequest,
) -> String {
    let raw = format!(
        "image:v1|url={target_url}|referer={}|w={}|h={}|q={}|fit={}|format={}",
        referer.unwrap_or_default(),
        request
            .width
            .map(|value| value.to_string())
            .unwrap_or_default(),
        request
            .height
            .map(|value| value.to_string())
            .unwrap_or_default(),
        request.quality,
        image_fit_slug(request.fit),
        image_format_slug(&request.format),
    );
    let digest = Sha256::digest(raw.as_bytes());
    format!("image:v1:{digest:x}")
}

pub fn build_processed_image_object_key(
    prefix: &str,
    cache_key: &str,
    format: &ImageFormat,
) -> String {
    let digest = cache_key.rsplit(':').next().unwrap_or(cache_key);
    let folder = &digest[..2.min(digest.len())];
    let prefix = prefix.trim_matches('/');
    format!(
        "{prefix}/v1/{folder}/{digest}.{}",
        image_extension_for_format(format)
    )
}

pub fn image_extension_for_format(format: &ImageFormat) -> &'static str {
    match format {
        ImageFormat::Avif => "avif",
        ImageFormat::Webp => "webp",
        ImageFormat::Jpeg | ImageFormat::Auto => "jpg",
        ImageFormat::Png => "png",
    }
}

pub fn request_origin(headers: &HeaderMap) -> String {
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(header::HOST))
        .and_then(|value| value.to_str().ok())
        .unwrap_or("localhost:8080");
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("http");
    format!("{scheme}://{host}")
}

pub fn clamp_quality(quality: u64) -> u8 {
    min(100, quality) as u8
}

fn header_value(value: &str) -> HeaderValue {
    HeaderValue::from_str(value).unwrap_or_else(|_| HeaderValue::from_static(""))
}

fn image_fit_slug(fit: ImageFit) -> &'static str {
    match fit {
        ImageFit::ScaleDown => "scale-down",
        ImageFit::Contain => "contain",
        ImageFit::Cover => "cover",
        ImageFit::Crop => "crop",
        ImageFit::Pad => "pad",
    }
}

fn image_format_slug(format: &ImageFormat) -> &'static str {
    match format {
        ImageFormat::Avif => "avif",
        ImageFormat::Webp => "webp",
        ImageFormat::Jpeg => "jpeg",
        ImageFormat::Png => "png",
        ImageFormat::Auto => "auto",
    }
}
