use std::{env, path::PathBuf};

use crate::error::AppError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheBackend {
    Sqlite,
    Redis,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub api_response_cache_ttl: u64,
    pub image_cache_ttl: u64,
    pub og_cache_ttl: u64,
    pub fetch_timeout_ms: u64,
    pub cache_backend: CacheBackend,
    pub sqlite_path: PathBuf,
    pub redis_url: Option<String>,
}

impl Config {
    pub fn from_env() -> Result<Self, AppError> {
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

        Ok(Self {
            host: env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string()),
            port: parse_env_u16("PORT", 8080)?,
            api_response_cache_ttl: parse_env_u64("API_RESPONSE_CACHE_TTL", 3600)?,
            image_cache_ttl: parse_env_u64("IMAGE_CACHE_TTL", 86400)?,
            og_cache_ttl: parse_env_u64("OG_CACHE_TTL", 43200)?,
            fetch_timeout_ms: parse_env_u64("FETCH_TIMEOUT_MS", 8000)?,
            cache_backend,
            sqlite_path: PathBuf::from(
                env::var("SQLITE_PATH").unwrap_or_else(|_| "/data/unfurl.db".to_string()),
            ),
            redis_url,
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
