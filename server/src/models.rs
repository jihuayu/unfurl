use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum CacheStatus {
    Hit,
    Miss,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseHeadersShape {
    #[serde(rename = "x-cache-status", skip_serializing_if = "Option::is_none")]
    pub cache_status: Option<CacheStatus>,
    #[serde(rename = "x-response-time")]
    pub response_time: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ImageFormat {
    Avif,
    Webp,
    Jpeg,
    Png,
    Auto,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ImageFit {
    ScaleDown,
    Contain,
    Cover,
    Crop,
    Pad,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MediaAsset {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LogoAsset {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UnfurlData {
    pub title: Option<String>,
    pub description: Option<String>,
    pub image: Option<MediaAsset>,
    pub url: String,
    pub author: Option<String>,
    pub publisher: Option<String>,
    pub date: Option<String>,
    pub lang: Option<String>,
    pub logo: Option<LogoAsset>,
    pub video: Option<MediaAsset>,
    pub audio: Option<MediaAsset>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiSuccessResponse {
    pub status: &'static str,
    pub data: UnfurlData,
    pub headers: ResponseHeadersShape,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiErrorResponse {
    pub status: &'static str,
    pub error: ApiErrorShape,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<ResponseHeadersShape>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiErrorShape {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawHeadMetadata {
    pub lang: Option<String>,
    pub title_chunks: Vec<String>,
    pub meta: HashMap<String, Vec<String>>,
    pub icons: Vec<String>,
    pub canonical: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEnvelope {
    pub data: UnfurlData,
    #[serde(rename = "cachedAt")]
    pub cached_at: String,
    pub ttl: u64,
}

#[derive(Debug, Clone)]
pub struct ImageRequest {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub quality: u8,
    pub format: ImageFormat,
    pub fit: ImageFit,
}

#[derive(Debug, Clone)]
pub struct ProcessedImage {
    pub bytes: Vec<u8>,
    pub content_type: &'static str,
    pub optimized: bool,
}
