use std::{path::Path, str::FromStr, sync::Arc};

use async_trait::async_trait;
use aws_config::{BehaviorVersion, Region};
use aws_credential_types::Credentials;
use aws_sdk_s3::{Client as S3Client, primitives::ByteStream};
use bytes::Bytes;
use sqlx::{
    Row,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};
use time::OffsetDateTime;

use crate::{
    config::{Config, ImageCacheBackend},
    error::AppError,
    models::{CachedImage, ImageCacheHit, ImageCacheWrite},
};

#[async_trait]
pub trait ImageCacheStore: Send + Sync {
    async fn get(&self, key: &str, object_key: &str) -> Result<Option<ImageCacheHit>, AppError>;
    async fn put(&self, entry: ImageCacheWrite) -> Result<ImageCacheHit, AppError>;
    fn label(&self) -> &'static str;
}

pub async fn build_image_cache(config: &Config) -> Result<Arc<dyn ImageCacheStore>, AppError> {
    match config.image_cache_backend {
        ImageCacheBackend::Sqlite => Ok(Arc::new(
            SqliteImageCache::new(
                &config.sqlite_path,
                config.sqlite_image_max_connections,
                config.sqlite_idle_timeout_secs,
            )
            .await?,
        )),
        ImageCacheBackend::S3 => Ok(Arc::new(S3ImageCache::new(config).await?)),
    }
}

pub struct SqliteImageCache {
    pool: sqlx::SqlitePool,
}

impl SqliteImageCache {
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
      CREATE TABLE IF NOT EXISTS image_cache (
        cache_key TEXT PRIMARY KEY,
        content_type TEXT NOT NULL,
        image_bytes BLOB NOT NULL,
        optimized INTEGER NOT NULL,
        expires_at INTEGER NOT NULL
      )
      "#,
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_image_cache_expires_at ON image_cache (expires_at)",
        )
        .execute(&pool)
        .await?;

        Ok(Self { pool })
    }
}

#[async_trait]
impl ImageCacheStore for SqliteImageCache {
    async fn get(&self, key: &str, _object_key: &str) -> Result<Option<ImageCacheHit>, AppError> {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let row = sqlx::query(
            "SELECT content_type, image_bytes, optimized FROM image_cache WHERE cache_key = ?1 AND expires_at > ?2",
        )
        .bind(key)
        .bind(now)
        .fetch_optional(&self.pool)
        .await?;

        match row {
            Some(row) => Ok(Some(ImageCacheHit::Inline(CachedImage {
                content_type: row.try_get::<&str, _>("content_type")?.to_string(),
                bytes: Bytes::from(row.try_get::<Vec<u8>, _>("image_bytes")?),
                optimized: row.try_get::<i64, _>("optimized")? == 1,
            }))),
            None => Ok(None),
        }
    }

    async fn put(&self, entry: ImageCacheWrite) -> Result<ImageCacheHit, AppError> {
        let expires_at = OffsetDateTime::now_utc().unix_timestamp() + entry.ttl as i64;
        sqlx::query(
            r#"
      INSERT INTO image_cache (cache_key, content_type, image_bytes, optimized, expires_at)
      VALUES (?1, ?2, ?3, ?4, ?5)
      ON CONFLICT(cache_key) DO UPDATE SET
        content_type = excluded.content_type,
        image_bytes = excluded.image_bytes,
        optimized = excluded.optimized,
        expires_at = excluded.expires_at
      "#,
        )
        .bind(&entry.cache_key)
        .bind(&entry.content_type)
        .bind(entry.bytes.as_ref())
        .bind(if entry.optimized { 1_i64 } else { 0_i64 })
        .bind(expires_at)
        .execute(&self.pool)
        .await?;

        Ok(ImageCacheHit::Inline(CachedImage {
            bytes: entry.bytes,
            content_type: entry.content_type,
            optimized: entry.optimized,
        }))
    }

    fn label(&self) -> &'static str {
        "sqlite"
    }
}

pub struct S3ImageCache {
    client: S3Client,
    bucket: String,
    public_base_url: String,
}

impl S3ImageCache {
    async fn new(config: &Config) -> Result<Self, AppError> {
        let mut loader = aws_config::defaults(BehaviorVersion::latest())
            .region(Region::new(config.s3_region.clone()));
        if let (Some(access_key_id), Some(secret_access_key)) = (
            config.s3_access_key_id.clone(),
            config.s3_secret_access_key.clone(),
        ) {
            loader = loader.credentials_provider(Credentials::new(
                access_key_id,
                secret_access_key,
                None,
                None,
                "config",
            ));
        }
        let shared_config = loader.load().await;
        let mut s3_config = aws_sdk_s3::config::Builder::from(&shared_config);
        if let Some(endpoint) = &config.s3_endpoint {
            s3_config = s3_config.endpoint_url(endpoint);
        }
        if config.s3_force_path_style {
            s3_config = s3_config.force_path_style(true);
        }

        Ok(Self {
            client: S3Client::from_conf(s3_config.build()),
            bucket: config.s3_bucket.clone().expect("s3 bucket validated"),
            public_base_url: config
                .s3_public_base_url
                .clone()
                .expect("s3 public base url validated"),
        })
    }

    fn public_url(&self, object_key: &str) -> String {
        format!(
            "{}/{}",
            self.public_base_url.trim_end_matches('/'),
            object_key.trim_start_matches('/')
        )
    }
}

#[async_trait]
impl ImageCacheStore for S3ImageCache {
    async fn get(&self, _key: &str, object_key: &str) -> Result<Option<ImageCacheHit>, AppError> {
        match self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(object_key)
            .send()
            .await
        {
            Ok(_) => Ok(Some(ImageCacheHit::Redirect {
                location: self.public_url(object_key),
            })),
            Err(error)
                if error
                    .as_service_error()
                    .is_some_and(|service_error| service_error.is_not_found()) =>
            {
                Ok(None)
            }
            Err(error) => Err(AppError::internal_with_message(format!(
                "failed to read image cache object: {error}"
            ))),
        }
    }

    async fn put(&self, entry: ImageCacheWrite) -> Result<ImageCacheHit, AppError> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(&entry.object_key)
            .content_type(entry.content_type)
            .cache_control(format!("public, max-age={}, immutable", entry.ttl))
            .body(ByteStream::from(entry.bytes.to_vec()))
            .send()
            .await
            .map_err(|error| {
                AppError::internal_with_message(format!(
                    "failed to upload image cache object: {error}"
                ))
            })?;

        Ok(ImageCacheHit::Redirect {
            location: self.public_url(&entry.object_key),
        })
    }

    fn label(&self) -> &'static str {
        "s3"
    }
}
