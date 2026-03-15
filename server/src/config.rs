use std::{env, path::PathBuf};

use crate::error::AppError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheBackend {
    Sqlite,
    Redis,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageCacheBackend {
    Sqlite,
    S3,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub low_memory_mode: bool,
    pub api_response_cache_ttl: u64,
    pub image_cache_ttl: u64,
    pub og_cache_ttl: u64,
    pub fetch_timeout_ms: u64,
    pub api_miss_max_concurrency: usize,
    pub image_miss_max_concurrency: usize,
    pub http_pool_max_idle_per_host: usize,
    pub http_pool_idle_timeout_secs: u64,
    pub sqlite_meta_max_connections: u32,
    pub sqlite_image_max_connections: u32,
    pub sqlite_idle_timeout_secs: u64,
    pub cache_backend: CacheBackend,
    pub image_cache_backend: ImageCacheBackend,
    pub sqlite_path: PathBuf,
    pub image_worker_bin: Option<PathBuf>,
    pub redis_url: Option<String>,
    pub s3_endpoint: Option<String>,
    pub s3_region: String,
    pub s3_bucket: Option<String>,
    pub s3_access_key_id: Option<String>,
    pub s3_secret_access_key: Option<String>,
    pub s3_public_base_url: Option<String>,
    pub s3_force_path_style: bool,
    pub s3_prefix: String,
}

impl Config {
    pub fn from_env() -> Result<Self, AppError> {
        let low_memory_mode = parse_env_bool("LOW_MEMORY_MODE", false);
        let cache_backend = match env::var("CACHE_BACKEND")
            .unwrap_or_else(|_| "sqlite".to_string())
            .to_lowercase()
            .as_str()
        {
            "sqlite" => CacheBackend::Sqlite,
            "redis" => CacheBackend::Redis,
            other => {
                return Err(AppError::new(
                    axum::http::StatusCode::BAD_REQUEST,
                    "INVALID_CACHE_BACKEND",
                    format!("Unsupported cache backend: {other}"),
                ));
            }
        };
        let image_cache_backend = match env::var("IMAGE_CACHE_BACKEND")
            .unwrap_or_else(|_| "sqlite".to_string())
            .to_lowercase()
            .as_str()
        {
            "sqlite" => ImageCacheBackend::Sqlite,
            "s3" => ImageCacheBackend::S3,
            other => {
                return Err(AppError::new(
                    axum::http::StatusCode::BAD_REQUEST,
                    "INVALID_IMAGE_CACHE_BACKEND",
                    format!("Unsupported image cache backend: {other}"),
                ));
            }
        };

        let redis_url = env::var("REDIS_URL")
            .ok()
            .filter(|value| !value.trim().is_empty());
        if matches!(cache_backend, CacheBackend::Redis) && redis_url.is_none() {
            return Err(AppError::new(
                axum::http::StatusCode::BAD_REQUEST,
                "MISSING_CONFIG",
                "REDIS_URL is required when CACHE_BACKEND=redis",
            ));
        }

        let s3_bucket = env::var("S3_BUCKET")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let s3_public_base_url = env::var("S3_PUBLIC_BASE_URL")
            .ok()
            .filter(|value| !value.trim().is_empty());
        if matches!(image_cache_backend, ImageCacheBackend::S3)
            && (s3_bucket.is_none() || s3_public_base_url.is_none())
        {
            return Err(AppError::new(
                axum::http::StatusCode::BAD_REQUEST,
                "MISSING_CONFIG",
                "S3_BUCKET and S3_PUBLIC_BASE_URL are required when IMAGE_CACHE_BACKEND=s3",
            ));
        }

        Ok(Self {
            host: env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string()),
            port: parse_env_u16("PORT", 8080)?,
            low_memory_mode,
            api_response_cache_ttl: parse_env_u64("API_RESPONSE_CACHE_TTL", 3600)?,
            image_cache_ttl: parse_env_u64("IMAGE_CACHE_TTL", 86400)?,
            og_cache_ttl: parse_env_u64("OG_CACHE_TTL", 43200)?,
            fetch_timeout_ms: parse_env_u64("FETCH_TIMEOUT_MS", 8000)?,
            api_miss_max_concurrency: parse_env_usize(
                "API_MISS_MAX_CONCURRENCY",
                default_api_miss_max_concurrency(low_memory_mode),
            )?,
            image_miss_max_concurrency: parse_env_usize(
                "IMAGE_MISS_MAX_CONCURRENCY",
                default_image_miss_max_concurrency(low_memory_mode),
            )?,
            http_pool_max_idle_per_host: parse_env_usize(
                "HTTP_POOL_MAX_IDLE_PER_HOST",
                default_http_pool_max_idle_per_host(low_memory_mode),
            )?,
            http_pool_idle_timeout_secs: parse_env_u64(
                "HTTP_POOL_IDLE_TIMEOUT_SECS",
                default_http_pool_idle_timeout_secs(low_memory_mode),
            )?,
            sqlite_meta_max_connections: parse_env_u32(
                "SQLITE_META_MAX_CONNECTIONS",
                default_sqlite_meta_max_connections(low_memory_mode),
            )?,
            sqlite_image_max_connections: parse_env_u32(
                "SQLITE_IMAGE_MAX_CONNECTIONS",
                default_sqlite_image_max_connections(low_memory_mode),
            )?,
            sqlite_idle_timeout_secs: parse_env_u64(
                "SQLITE_IDLE_TIMEOUT_SECS",
                default_sqlite_idle_timeout_secs(low_memory_mode),
            )?,
            cache_backend,
            image_cache_backend,
            sqlite_path: PathBuf::from(
                env::var("SQLITE_PATH").unwrap_or_else(|_| "/data/unfurl.db".to_string()),
            ),
            image_worker_bin: env::var("IMAGE_WORKER_BIN")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .map(PathBuf::from),
            redis_url,
            s3_endpoint: env::var("S3_ENDPOINT")
                .ok()
                .filter(|value| !value.trim().is_empty()),
            s3_region: env::var("S3_REGION").unwrap_or_else(|_| "us-east-1".to_string()),
            s3_bucket,
            s3_access_key_id: env::var("S3_ACCESS_KEY_ID")
                .ok()
                .filter(|value| !value.trim().is_empty()),
            s3_secret_access_key: env::var("S3_SECRET_ACCESS_KEY")
                .ok()
                .filter(|value| !value.trim().is_empty()),
            s3_public_base_url,
            s3_force_path_style: parse_env_bool("S3_FORCE_PATH_STYLE", false),
            s3_prefix: env::var("S3_PREFIX").unwrap_or_else(|_| "image-cache".to_string()),
        })
    }
}

fn parse_env_u16(name: &str, default_value: u16) -> Result<u16, AppError> {
    match env::var(name) {
        Ok(value) => value.parse::<u16>().map_err(|_| {
            AppError::new(
                axum::http::StatusCode::BAD_REQUEST,
                "INVALID_CONFIG",
                format!("{name} must be a valid u16"),
            )
        }),
        Err(_) => Ok(default_value),
    }
}

fn parse_env_u64(name: &str, default_value: u64) -> Result<u64, AppError> {
    match env::var(name) {
        Ok(value) => value.parse::<u64>().map_err(|_| {
            AppError::new(
                axum::http::StatusCode::BAD_REQUEST,
                "INVALID_CONFIG",
                format!("{name} must be a positive integer"),
            )
        }),
        Err(_) => Ok(default_value),
    }
}

fn parse_env_u32(name: &str, default_value: u32) -> Result<u32, AppError> {
    match env::var(name) {
        Ok(value) => value.parse::<u32>().map_err(|_| {
            AppError::new(
                axum::http::StatusCode::BAD_REQUEST,
                "INVALID_CONFIG",
                format!("{name} must be a positive integer"),
            )
        }),
        Err(_) => Ok(default_value),
    }
}

fn parse_env_bool(name: &str, default_value: bool) -> bool {
    env::var(name)
        .ok()
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes"
            )
        })
        .unwrap_or(default_value)
}

fn parse_env_usize(name: &str, default_value: usize) -> Result<usize, AppError> {
    match env::var(name) {
        Ok(value) => value.parse::<usize>().map_err(|_| {
            AppError::new(
                axum::http::StatusCode::BAD_REQUEST,
                "INVALID_CONFIG",
                format!("{name} must be a positive integer"),
            )
        }),
        Err(_) => Ok(default_value),
    }
}

fn default_api_miss_max_concurrency(low_memory_mode: bool) -> usize {
    if low_memory_mode {
        return 4;
    }
    std::thread::available_parallelism()
        .map(|value| value.get().saturating_mul(4).max(8))
        .unwrap_or(8)
}

fn default_image_miss_max_concurrency(low_memory_mode: bool) -> usize {
    if low_memory_mode {
        return 1;
    }
    std::thread::available_parallelism()
        .map(|value| value.get().saturating_sub(1).max(1))
        .unwrap_or(1)
}

fn default_http_pool_max_idle_per_host(low_memory_mode: bool) -> usize {
    if low_memory_mode { 1 } else { 8 }
}

fn default_http_pool_idle_timeout_secs(low_memory_mode: bool) -> u64 {
    if low_memory_mode { 15 } else { 90 }
}

fn default_sqlite_meta_max_connections(low_memory_mode: bool) -> u32 {
    if low_memory_mode { 1 } else { 5 }
}

fn default_sqlite_image_max_connections(low_memory_mode: bool) -> u32 {
    if low_memory_mode { 1 } else { 5 }
}

fn default_sqlite_idle_timeout_secs(low_memory_mode: bool) -> u64 {
    if low_memory_mode { 15 } else { 300 }
}
