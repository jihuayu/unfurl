use std::{path::Path, str::FromStr, sync::Arc};

use async_trait::async_trait;
use redis::AsyncCommands;
use sqlx::{
    Connection, Row,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::{
    config::{CacheBackend, Config},
    error::AppError,
    models::{CacheEnvelope, CacheRead, UnfurlData},
};

#[async_trait]
pub trait CacheStore: Send + Sync {
    async fn get(&self, key: &str) -> Result<Option<CacheRead>, AppError>;
    async fn set(&self, key: &str, data: &UnfurlData, ttl: u64) -> Result<CacheEnvelope, AppError>;
    fn label(&self) -> &'static str;
}

// Serialize additive migration across independent pools/processes. A deferred
// transaction would let both readers observe the old schema before ALTER.
pub(crate) async fn ensure_sqlite_access_column(
    pool: &sqlx::SqlitePool,
    table: &str,
) -> Result<(), AppError> {
    assert!(matches!(table, "unfurl_cache" | "image_cache"));
    let mut connection = pool.acquire().await?;
    let mut tx = connection.begin_with("BEGIN IMMEDIATE").await?;
    let columns = sqlx::query(&format!("PRAGMA table_info({table})"))
        .fetch_all(&mut *tx)
        .await?;
    if !columns
        .iter()
        .any(|row| row.get::<String, _>("name") == "last_access")
    {
        sqlx::query(&format!(
            "ALTER TABLE {table} ADD COLUMN last_access BIGINT NOT NULL DEFAULT 0"
        ))
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn build_cache(config: &Config) -> Result<Arc<dyn CacheStore>, AppError> {
    match config.cache_backend {
        CacheBackend::Postgres => Ok(Arc::new(PostgresCache::new(config).await?)),
        CacheBackend::None => Ok(Arc::new(NoopCache)),
        CacheBackend::Sqlite => Ok(Arc::new(
            SqliteCache::new(
                &config.sqlite_path,
                config.sqlite_meta_max_connections,
                config.sqlite_idle_timeout_secs,
            )
            .await?,
        )),
        CacheBackend::Redis => {
            let client =
                redis::Client::open(config.redis_url.clone().expect("redis url validated"))?;
            Ok(Arc::new(RedisCache { client }))
        }
    }
}

pub struct NoopCache;

#[async_trait]
impl CacheStore for NoopCache {
    async fn get(&self, _key: &str) -> Result<Option<CacheRead>, AppError> {
        Ok(None)
    }

    async fn set(
        &self,
        _key: &str,
        data: &UnfurlData,
        ttl: u64,
    ) -> Result<CacheEnvelope, AppError> {
        build_envelope(data.clone(), ttl)
    }

    fn label(&self) -> &'static str {
        "none"
    }
}

pub struct SqliteCache {
    pool: sqlx::SqlitePool,
}

impl SqliteCache {
    async fn new(
        path: &Path,
        max_connections: u32,
        idle_timeout_secs: u64,
    ) -> Result<Self, AppError> {
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
            .max_connections(max_connections.max(1))
            .min_connections(0)
            .idle_timeout(Some(std::time::Duration::from_secs(
                idle_timeout_secs.max(1),
            )))
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

        ensure_sqlite_access_column(&pool, "unfurl_cache").await?;
        Ok(Self { pool })
    }
}

#[async_trait]
impl CacheStore for SqliteCache {
    async fn get(&self, key: &str) -> Result<Option<CacheRead>, AppError> {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let row =
            sqlx::query("SELECT payload_json, expires_at FROM unfurl_cache WHERE cache_key = ?1")
                .bind(key)
                .fetch_optional(&self.pool)
                .await?;

        if row.is_some() {
            sqlx::query("UPDATE unfurl_cache SET last_access=?1 WHERE cache_key=?2")
                .bind(now)
                .bind(key)
                .execute(&self.pool)
                .await?;
        }

        match row {
            Some(row) => Ok(Some(CacheRead {
                envelope: serde_json::from_str(row.try_get::<&str, _>("payload_json")?)?,
                is_stale: row.try_get::<i64, _>("expires_at")? <= now,
            })),
            None => Ok(None),
        }
    }

    async fn set(&self, key: &str, data: &UnfurlData, ttl: u64) -> Result<CacheEnvelope, AppError> {
        let envelope = build_envelope(data.clone(), ttl)?;
        let payload = serde_json::to_string(&envelope)?;
        let expires_at = OffsetDateTime::now_utc().unix_timestamp() + ttl as i64;

        let mut tx = self.pool.begin().await?;
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
        .execute(&mut *tx)
        .await?;

        let now = OffsetDateTime::now_utc().unix_timestamp();
        sqlx::query("UPDATE unfurl_cache SET last_access=?1 WHERE cache_key=?2")
            .bind(now)
            .bind(key)
            .execute(&mut *tx)
            .await?;
        // Keep a seven-day stale window, with hard logical byte and entry budgets.
        sqlx::query("DELETE FROM unfurl_cache WHERE expires_at < ?1 OR cache_key IN (SELECT cache_key FROM (SELECT cache_key, ROW_NUMBER() OVER (ORDER BY last_access DESC, cache_key) AS rank, SUM(length(CAST(payload_json AS BLOB))) OVER (ORDER BY last_access DESC, cache_key) AS bytes FROM unfurl_cache) AS ranked WHERE rank > 10000 OR bytes > 67108864)")
            .bind(now - 7 * 24 * 3600).execute(&mut *tx).await?;
        tx.commit().await?;

        Ok(envelope)
    }

    fn label(&self) -> &'static str {
        "sqlite"
    }
}

pub struct PostgresCache {
    pool: sqlx::PgPool,
}

impl PostgresCache {
    async fn new(config: &Config) -> Result<Self, AppError> {
        let url = config
            .database_url
            .as_deref()
            .ok_or_else(|| AppError::internal_with_message("DATABASE_URL is required"))?;
        let options = sqlx::postgres::PgConnectOptions::from_str(url)
            .map_err(|_| AppError::internal_with_message("invalid PostgreSQL URL"))?;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(std::time::Duration::from_secs(10))
            .connect_with(options)
            .await?;
        sqlx::query(
            r#"
      CREATE TABLE IF NOT EXISTS unfurl_cache (
        cache_key TEXT PRIMARY KEY,
        payload_json TEXT NOT NULL,
        cached_at TEXT NOT NULL,
        ttl_seconds BIGINT NOT NULL,
        expires_at BIGINT NOT NULL
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

        sqlx::query("ALTER TABLE unfurl_cache ADD COLUMN IF NOT EXISTS last_access BIGINT NOT NULL DEFAULT 0").execute(&pool).await?;
        Ok(Self { pool })
    }
}

#[async_trait]
impl CacheStore for PostgresCache {
    async fn get(&self, key: &str) -> Result<Option<CacheRead>, AppError> {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let row =
            sqlx::query("SELECT payload_json, expires_at FROM unfurl_cache WHERE cache_key = $1")
                .bind(key)
                .fetch_optional(&self.pool)
                .await?;

        if row.is_some() {
            sqlx::query("UPDATE unfurl_cache SET last_access=$1 WHERE cache_key=$2")
                .bind(now)
                .bind(key)
                .execute(&self.pool)
                .await?;
        }

        match row {
            Some(row) => Ok(Some(CacheRead {
                envelope: serde_json::from_str(row.try_get::<&str, _>("payload_json")?)?,
                is_stale: row.try_get::<i64, _>("expires_at")? <= now,
            })),
            None => Ok(None),
        }
    }

    async fn set(&self, key: &str, data: &UnfurlData, ttl: u64) -> Result<CacheEnvelope, AppError> {
        let envelope = build_envelope(data.clone(), ttl)?;
        let payload = serde_json::to_string(&envelope)?;
        let expires_at = OffsetDateTime::now_utc().unix_timestamp() + ttl as i64;

        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(742301)")
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            r#"
      INSERT INTO unfurl_cache (cache_key, payload_json, cached_at, ttl_seconds, expires_at)
      VALUES ($1, $2, $3, $4, $5)
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
        .execute(&mut *tx)
        .await?;

        let now = OffsetDateTime::now_utc().unix_timestamp();
        sqlx::query("UPDATE unfurl_cache SET last_access=$1 WHERE cache_key=$2")
            .bind(now)
            .bind(key)
            .execute(&mut *tx)
            .await?;
        // Keep a seven-day stale window, with hard logical byte and entry budgets.
        sqlx::query("DELETE FROM unfurl_cache WHERE expires_at < $1 OR cache_key IN (SELECT cache_key FROM (SELECT cache_key, ROW_NUMBER() OVER (ORDER BY last_access DESC, cache_key) AS rank, SUM(octet_length(payload_json)) OVER (ORDER BY last_access DESC, cache_key) AS bytes FROM unfurl_cache) AS ranked WHERE rank > 10000 OR bytes > 67108864)")
            .bind(now - 7 * 24 * 3600).execute(&mut *tx).await?;
        tx.commit().await?;

        Ok(envelope)
    }

    fn label(&self) -> &'static str {
        "postgres"
    }
}

pub struct RedisCache {
    client: redis::Client,
}

#[async_trait]
impl CacheStore for RedisCache {
    async fn get(&self, key: &str) -> Result<Option<CacheRead>, AppError> {
        let mut connection = self.client.get_multiplexed_async_connection().await?;
        let value: Option<String> = connection.get(key).await?;
        value
            .map(|payload| {
                serde_json::from_str(&payload).map(|envelope| CacheRead {
                    envelope,
                    is_stale: false,
                })
            })
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

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    #[tokio::test]
    async fn concurrent_sqlite_upgrade_preserves_legacy_cache() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("legacy.sqlite");
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&path)
                    .create_if_missing(true),
            )
            .await
            .unwrap();
        sqlx::query("CREATE TABLE unfurl_cache(cache_key TEXT PRIMARY KEY,payload_json TEXT NOT NULL,cached_at TEXT NOT NULL,ttl_seconds INTEGER NOT NULL,expires_at INTEGER NOT NULL)").execute(&pool).await.unwrap();
        let data: UnfurlData =
            serde_json::from_value(serde_json::json!({"url":"https://example.com/old"})).unwrap();
        let payload = serde_json::to_string(&build_envelope(data.clone(), 3600).unwrap()).unwrap();
        sqlx::query("INSERT INTO unfurl_cache VALUES('legacy',?1,'old',3600,?2)")
            .bind(payload)
            .bind(OffsetDateTime::now_utc().unix_timestamp() + 3600)
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let path = path.clone();
            let data = data.clone();
            tasks.push(tokio::spawn(async move {
                let cache = SqliteCache::new(&path, 1, 60).await.unwrap();
                assert_eq!(
                    cache.get("legacy").await.unwrap().unwrap().envelope.data,
                    data
                );
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn sqlite_budget_preserves_hot_entries_and_stale_grace() {
        let directory = tempfile::tempdir().unwrap();
        let cache = SqliteCache::new(&directory.path().join("cache.sqlite"), 1, 60)
            .await
            .unwrap();
        let data: UnfurlData =
            serde_json::from_value(serde_json::json!({"url":"https://example.com"})).unwrap();
        cache.set("hot", &data, 3600).await.unwrap();
        let payload: String =
            sqlx::query_scalar("SELECT payload_json FROM unfurl_cache WHERE cache_key='hot'")
                .fetch_one(&cache.pool)
                .await
                .unwrap();
        let now = OffsetDateTime::now_utc().unix_timestamp();
        sqlx::query("WITH RECURSIVE seq(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM seq WHERE n < 10001) INSERT INTO unfurl_cache(cache_key,payload_json,cached_at,ttl_seconds,expires_at,last_access) SELECT 'cold-' || n,?1,'old',3600,?2,0 FROM seq").bind(payload).bind(now+3600).execute(&cache.pool).await.unwrap();
        cache.get("hot").await.unwrap();
        cache.set("new", &data, 3600).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM unfurl_cache")
            .fetch_one(&cache.pool)
            .await
            .unwrap();
        assert_eq!(count, 10000);
        assert!(cache.get("hot").await.unwrap().is_some());
        sqlx::query("UPDATE unfurl_cache SET expires_at=?1 WHERE cache_key='hot'")
            .bind(now - 60)
            .execute(&cache.pool)
            .await
            .unwrap();
        cache.set("new", &data, 3600).await.unwrap();
        assert!(cache.get("hot").await.unwrap().unwrap().is_stale);
        sqlx::query("UPDATE unfurl_cache SET expires_at=?1 WHERE cache_key='hot'")
            .bind(now - 8 * 24 * 3600)
            .execute(&cache.pool)
            .await
            .unwrap();
        cache.set("new", &data, 3600).await.unwrap();
        assert!(cache.get("hot").await.unwrap().is_none());
    }
}
