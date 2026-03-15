use std::{path::Path, str::FromStr, sync::Arc};

use async_trait::async_trait;
use redis::AsyncCommands;
use sqlx::{
    Row,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::{
    config::{CacheBackend, Config},
    error::AppError,
    models::{CacheEnvelope, UnfurlData},
};

#[async_trait]
pub trait CacheStore: Send + Sync {
    async fn get(&self, key: &str) -> Result<Option<CacheEnvelope>, AppError>;
    async fn set(&self, key: &str, data: &UnfurlData, ttl: u64) -> Result<CacheEnvelope, AppError>;
    fn label(&self) -> &'static str;
}

pub async fn build_cache(config: &Config) -> Result<Arc<dyn CacheStore>, AppError> {
    match config.cache_backend {
        CacheBackend::Sqlite => Ok(Arc::new(SqliteCache::new(&config.sqlite_path).await?)),
        CacheBackend::Redis => {
            let client =
                redis::Client::open(config.redis_url.clone().expect("redis url validated"))?;
            Ok(Arc::new(RedisCache { client }))
        }
    }
}

pub struct SqliteCache {
    pool: sqlx::SqlitePool,
}

impl SqliteCache {
    async fn new(path: &Path) -> Result<Self, AppError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent).await.map_err(|error| {
                AppError::internal_with_message(format!(
                    "failed to create sqlite directory: {error}"
                ))
            })?;
        }

        let options = SqliteConnectOptions::from_str(&format!(
            "sqlite://{}",
            path.to_string_lossy().replace('\\', "/")
        ))
        .map_err(|error| AppError::internal_with_message(format!("invalid sqlite path: {error}")))?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(std::time::Duration::from_secs(5));

        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await?;

        sqlx::query(
            r#"
      CREATE TABLE IF NOT EXISTS unfurl_cache (
        cache_key TEXT PRIMARY KEY,
        payload_json TEXT NOT NULL,
        cached_at TEXT NOT NULL,
        ttl_seconds INTEGER NOT NULL,
        expires_at INTEGER NOT NULL
      )
      "#,
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_unfurl_cache_expires_at ON unfurl_cache (expires_at)",
        )
        .execute(&pool)
        .await?;

        Ok(Self { pool })
    }
}

#[async_trait]
impl CacheStore for SqliteCache {
    async fn get(&self, key: &str) -> Result<Option<CacheEnvelope>, AppError> {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let row = sqlx::query(
            "SELECT payload_json FROM unfurl_cache WHERE cache_key = ?1 AND expires_at > ?2",
        )
        .bind(key)
        .bind(now)
        .fetch_optional(&self.pool)
        .await?;

        match row {
            Some(row) => Ok(Some(serde_json::from_str(
                row.try_get::<&str, _>("payload_json")?,
            )?)),
            None => Ok(None),
        }
    }

    async fn set(&self, key: &str, data: &UnfurlData, ttl: u64) -> Result<CacheEnvelope, AppError> {
        let envelope = build_envelope(data.clone(), ttl)?;
        let payload = serde_json::to_string(&envelope)?;
        let expires_at = OffsetDateTime::now_utc().unix_timestamp() + ttl as i64;

        sqlx::query(
            r#"
      INSERT INTO unfurl_cache (cache_key, payload_json, cached_at, ttl_seconds, expires_at)
      VALUES (?1, ?2, ?3, ?4, ?5)
      ON CONFLICT(cache_key) DO UPDATE SET
        payload_json = excluded.payload_json,
        cached_at = excluded.cached_at,
        ttl_seconds = excluded.ttl_seconds,
        expires_at = excluded.expires_at
      "#,
        )
        .bind(key)
        .bind(&payload)
        .bind(&envelope.cached_at)
        .bind(ttl as i64)
        .bind(expires_at)
        .execute(&self.pool)
        .await?;

        Ok(envelope)
    }

    fn label(&self) -> &'static str {
        "sqlite"
    }
}

pub struct RedisCache {
    client: redis::Client,
}

#[async_trait]
impl CacheStore for RedisCache {
    async fn get(&self, key: &str) -> Result<Option<CacheEnvelope>, AppError> {
        let mut connection = self.client.get_multiplexed_async_connection().await?;
        let value: Option<String> = connection.get(key).await?;
        value
            .map(|payload| serde_json::from_str(&payload))
            .transpose()
            .map_err(Into::into)
    }

    async fn set(&self, key: &str, data: &UnfurlData, ttl: u64) -> Result<CacheEnvelope, AppError> {
        let envelope = build_envelope(data.clone(), ttl)?;
        let payload = serde_json::to_string(&envelope)?;
        let mut connection = self.client.get_multiplexed_async_connection().await?;
        let _: () = connection.set_ex(key, payload, ttl).await?;
        Ok(envelope)
    }

    fn label(&self) -> &'static str {
        "redis"
    }
}

fn build_envelope(data: UnfurlData, ttl: u64) -> Result<CacheEnvelope, AppError> {
    Ok(CacheEnvelope {
        data,
        cached_at: OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .map_err(|error| {
                AppError::internal_with_message(format!(
                    "failed to format cache timestamp: {error}"
                ))
            })?,
        ttl,
    })
}
