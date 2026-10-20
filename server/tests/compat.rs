use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use sqlx::{Row, sqlite::SqlitePoolOptions};
use tempfile::TempDir;
use tokio::{sync::Semaphore, time::sleep};
use tower::ServiceExt;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header as match_header, method, path, query_param},
};

use unfurl_server::{
    build_app_with_client,
    cache::CacheStore,
    config::{CacheBackend, Config, ImageCacheBackend},
    image_cache::ImageCacheStore,
    models::{
        CacheEnvelope, CacheRead, ImageCacheHit, ImageCacheRead, ImageCacheWrite, UnfurlData,
    },
    router_with_state,
    state::AppState,
};

fn test_config(sqlite_path: PathBuf) -> Config {
    Config {
        host: "127.0.0.1".to_string(),
        port: 0,
        public_base_url: None,
        low_memory_mode: false,
        api_response_cache_ttl: 3600,
        image_cache_ttl: 259200,
        image_browser_cache_ttl: 86400,
        og_cache_ttl: 43200,
        fetch_timeout_ms: 8000,
        api_miss_max_concurrency: 8,
        image_miss_max_concurrency: 1,
        http_pool_max_idle_per_host: 8,
        http_pool_idle_timeout_secs: 90,
        sqlite_meta_max_connections: 5,
        sqlite_image_max_connections: 5,
        sqlite_idle_timeout_secs: 300,
        database_url: None,
        cache_backend: CacheBackend::Sqlite,
        image_cache_backend: ImageCacheBackend::Sqlite,
        sqlite_path,
        image_worker_bin: None,
        redis_url: None,
        s3_endpoint: None,
        s3_region: "us-east-1".to_string(),
        s3_bucket: None,
        s3_access_key_id: None,
        s3_secret_access_key: None,
        s3_public_base_url: None,
        s3_force_path_style: false,
        s3_prefix: "image-cache".to_string(),
    }
}

async fn build_test_app(
    sqlite_path: PathBuf,
    resolved_host: &'static str,
    target: &str,
) -> axum::Router {
    let url = url::Url::parse(target).unwrap();
    let port = url.port_or_known_default().unwrap();
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::limited(10))
        .resolve(resolved_host, SocketAddr::from(([127, 0, 0, 1], port)))
        .build()
        .unwrap();
    build_app_with_client(test_config(sqlite_path), client)
        .await
        .unwrap()
}

fn sample_html(image_url: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
  <head>
    <title>Fallback title</title>
    <meta property="og:title" content="Example Title" />
    <meta property="og:description" content="Example Description" />
    <meta property="og:image" content="{image_url}" />
    <meta property="og:image:width" content="1200" />
    <meta property="og:image:height" content="630" />
    <meta property="og:url" content="https://example.com/post?case=first" />
    <meta name="twitter:site" content="@publisher" />
    <link rel="icon" href="/favicon.png" />
  </head>
  <body>
    <meta property="og:title" content="Body should be ignored" />
  </body>
</html>"#
    )
}

#[tokio::test]
async fn failed_image_worker_serves_and_caches_original_image() {
    let upstream = MockServer::start().await;
    let png = sample_png();
    Mock::given(method("GET"))
        .and(path("/original.png"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(png.clone(), "image/png"))
        .expect(1)
        .mount(&upstream)
        .await;
    let directory = TempDir::new().unwrap();
    let mut config = test_config(directory.path().join("cache.db"));
    config.low_memory_mode = true;
    config.image_worker_bin = Some(directory.path().join("missing-worker"));
    let target = upstream.uri().replace("127.0.0.1", "mock.example.test");
    let port = url::Url::parse(&upstream.uri()).unwrap().port().unwrap();
    let client = reqwest::Client::builder()
        .no_proxy()
        .resolve(
            "mock.example.test",
            SocketAddr::from(([127, 0, 0, 1], port)),
        )
        .build()
        .unwrap();
    let app = build_app_with_client(config, client).await.unwrap();
    let uri = format!(
        "/proxy/image?url={}&f=avif",
        urlencoding::encode(&format!("{target}/original.png"))
    );
    for expected in ["MISS", "HIT"] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "image/png");
        assert_eq!(response.headers()["x-cache-status"], expected);
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            png.as_slice()
        );
    }
}

fn sqlite_url(path: &Path) -> String {
    format!("sqlite://{}", path.to_string_lossy().replace('\\', "/"))
}

async fn image_cache_row_count(sqlite_path: &Path) -> i64 {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect(&sqlite_url(sqlite_path))
        .await
        .unwrap();
    let row = sqlx::query("SELECT COUNT(*) AS count FROM image_cache")
        .fetch_one(&pool)
        .await
        .unwrap();
    row.try_get::<i64, _>("count").unwrap()
}

async fn wait_for_image_cache_rows(sqlite_path: &Path, expected_minimum: i64) {
    let mut last_count = 0;
    for _ in 0..100 {
        last_count = image_cache_row_count(sqlite_path).await;
        if last_count >= expected_minimum {
            return;
        }
        sleep(Duration::from_millis(20)).await;
    }
    panic!("image cache reached {last_count} rows, expected {expected_minimum}");
}

async fn expire_cache_table(sqlite_path: &Path, table: &str) {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect(&sqlite_url(sqlite_path))
        .await
        .unwrap();
    let statement = format!("UPDATE {table} SET expires_at = 0");
    sqlx::query(&statement).execute(&pool).await.unwrap();
}

async fn cached_metadata_title(sqlite_path: &Path) -> Option<String> {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect(&sqlite_url(sqlite_path))
        .await
        .unwrap();
    let row = sqlx::query("SELECT payload_json FROM unfurl_cache LIMIT 1")
        .fetch_optional(&pool)
        .await
        .unwrap()?;
    let payload = row.try_get::<&str, _>("payload_json").unwrap();
    let value: serde_json::Value = serde_json::from_str(payload).unwrap();
    value["data"]["title"].as_str().map(ToOwned::to_owned)
}

async fn cached_image_bytes(sqlite_path: &Path) -> Option<Vec<u8>> {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect(&sqlite_url(sqlite_path))
        .await
        .unwrap();
    let row = sqlx::query("SELECT image_bytes FROM image_cache LIMIT 1")
        .fetch_optional(&pool)
        .await
        .unwrap()?;
    Some(row.try_get::<Vec<u8>, _>("image_bytes").unwrap())
}

async fn wait_for_cached_metadata_title(sqlite_path: &Path, expected: &str) {
    for _ in 0..100 {
        if cached_metadata_title(sqlite_path).await.as_deref() == Some(expected) {
            return;
        }
        sleep(Duration::from_millis(20)).await;
    }
    panic!("metadata cache did not refresh to title {expected}");
}

async fn wait_for_cached_image_change(sqlite_path: &Path, previous: &[u8]) {
    for _ in 0..100 {
        if cached_image_bytes(sqlite_path)
            .await
            .is_some_and(|bytes| bytes != previous)
        {
            return;
        }
        sleep(Duration::from_millis(20)).await;
    }
    panic!("image cache did not refresh");
}

#[tokio::test]
async fn api_returns_metadata_then_hits_cache() {
    let upstream = MockServer::start().await;
    let local_page_url = format!("{}/page", upstream.uri());
    let page_url = local_page_url
        .replace("127.0.0.1", "mock.example.test")
        .replace("localhost", "mock.example.test");
    let image_url = format!("{}/image.png", upstream.uri())
        .replace("127.0.0.1", "mock.example.test")
        .replace("localhost", "mock.example.test");
    let page_body = sample_html(&image_url);
    Mock::given(method("GET"))
        .and(path("/page"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(page_body, "text/html; charset=utf-8"),
        )
        .expect(1)
        .mount(&upstream)
        .await;

    let temp_dir = TempDir::new().unwrap();
    let app = build_test_app(
        temp_dir.path().join("cache.db"),
        "mock.example.test",
        &local_page_url,
    )
    .await;
    let url = urlencoding::encode(&page_url).into_owned();

    let first = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api?url={url}"))
                .header(header::HOST, "service.example")
                .header("x-forwarded-proto", "https")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let first_status = first.status();
    let first_headers = first.headers().clone();
    let first_body = first.into_body().collect().await.unwrap().to_bytes();
    let first_json: serde_json::Value = serde_json::from_slice(&first_body).unwrap();

    assert_eq!(first_status, StatusCode::OK);
    assert_eq!(first_headers.get("x-cache-status").unwrap(), "MISS");
    assert!(
        first_headers
            .get("server-timing")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("cache-write")
    );
    assert_eq!(
        first_headers.get(header::CACHE_CONTROL).unwrap(),
        "public, max-age=3600"
    );
    assert_eq!(first_json["status"], "success");
    assert_eq!(first_json["data"]["title"], "Example Title");
    assert_eq!(first_json["data"]["publisher"], "publisher");
    let image_url = first_json["data"]["image"]["url"].as_str().unwrap();
    let image_proxy = first_json["data"]["image"]["proxy"].as_str().unwrap();
    assert_eq!(image_url, image_proxy);
    assert!(image_url.starts_with("https://service.example/proxy/image?"));
    assert!(
        image_url.contains("referer="),
        "local image URL should keep the referer query for hotlink-protected origins"
    );
    assert!(image_url.contains("f=jpeg"));
    let logo_url = first_json["data"]["logo"]["url"].as_str().unwrap();
    assert_eq!(
        logo_url,
        first_json["data"]["logo"]["proxy"].as_str().unwrap()
    );
    assert!(logo_url.contains("referer="));

    let second = app
        .oneshot(
            Request::builder()
                .uri(format!("/api?url={url}"))
                .header(header::HOST, "another.example")
                .header("x-forwarded-proto", "https")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let second_status = second.status();
    let second_headers = second.headers().clone();
    let second_body = second.into_body().collect().await.unwrap().to_bytes();
    let second_json: serde_json::Value = serde_json::from_slice(&second_body).unwrap();

    assert_eq!(second_status, StatusCode::OK);
    assert_eq!(second_headers.get("x-cache-status").unwrap(), "HIT");
    assert_eq!(second_headers.get("x-cache-source").unwrap(), "sqlite");
    assert!(
        second_headers
            .get("server-timing")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("cache-read")
    );
    assert_eq!(second_json["data"]["title"], "Example Title");
    assert!(
        second_json["data"]["image"]["url"]
            .as_str()
            .unwrap()
            .starts_with("https://another.example/proxy/image?")
    );
}

#[tokio::test]
async fn api_warms_image_and_icon_cache_with_page_referer() {
    let upstream = MockServer::start().await;
    let local_page_url = format!("{}/page", upstream.uri());
    let page_url = local_page_url
        .replace("127.0.0.1", "mock.example.test")
        .replace("localhost", "mock.example.test");
    let image_url = format!("{}/image.png", upstream.uri())
        .replace("127.0.0.1", "mock.example.test")
        .replace("localhost", "mock.example.test");
    let page_body = sample_html(&image_url);
    let png_body = sample_png();

    Mock::given(method("GET"))
        .and(path("/page"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(page_body, "text/html; charset=utf-8"),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("GET"))
        .and(path("/image.png"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "image/png")
                .set_body_bytes(png_body.clone()),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("GET"))
        .and(path("/favicon.png"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "image/png")
                .set_body_bytes(png_body),
        )
        .expect(1)
        .mount(&upstream)
        .await;

    let temp_dir = TempDir::new().unwrap();
    let sqlite_path = temp_dir.path().join("cache.db");
    let app = build_test_app(sqlite_path.clone(), "mock.example.test", &local_page_url).await;
    let url = urlencoding::encode(&page_url).into_owned();

    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/api?url={url}"))
                .header(header::HOST, "service.example")
                .header("x-forwarded-proto", "https")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json["data"]["image"]["url"]
            .as_str()
            .unwrap()
            .contains("referer=")
    );
    assert!(
        json["data"]["logo"]["url"]
            .as_str()
            .unwrap()
            .contains("referer=")
    );
    wait_for_image_cache_rows(&sqlite_path, 2).await;

    let requests = upstream.received_requests().await.unwrap();
    for path in ["/image.png", "/favicon.png"] {
        let request = requests
            .iter()
            .find(|request| request.url.path() == path)
            .expect("metadata image warm request should reach upstream");
        assert_eq!(
            request.headers.get("referer").unwrap().to_str().unwrap(),
            page_url.as_str(),
            "metadata image warming should preserve the page referer for hotlink-protected origins"
        );
    }
}

#[tokio::test]
async fn api_serves_stale_metadata_and_refreshes_async() {
    let upstream = MockServer::start().await;
    let local_page_url = format!("{}/page", upstream.uri());
    let page_url = local_page_url
        .replace("127.0.0.1", "mock.example.test")
        .replace("localhost", "mock.example.test");

    Mock::given(method("GET"))
        .and(path("/page"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "<!doctype html><html><head><title>Old Title</title></head></html>",
            "text/html; charset=utf-8",
        ))
        .up_to_n_times(1)
        .mount(&upstream)
        .await;
    Mock::given(method("GET"))
        .and(path("/page"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(200))
                .set_body_raw(
                    "<!doctype html><html><head><title>New Title</title></head></html>",
                    "text/html; charset=utf-8",
                ),
        )
        .expect(1)
        .mount(&upstream)
        .await;

    let temp_dir = TempDir::new().unwrap();
    let sqlite_path = temp_dir.path().join("cache.db");
    let app = build_test_app(sqlite_path.clone(), "mock.example.test", &local_page_url).await;
    let url = urlencoding::encode(&page_url).into_owned();

    let first = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api?url={url}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    expire_cache_table(&sqlite_path, "unfurl_cache").await;

    let mut requests = Vec::new();
    for _ in 0..100 {
        let app = app.clone();
        let url = url.clone();
        requests.push(tokio::spawn(async move {
            app.oneshot(
                Request::builder()
                    .uri(format!("/api?url={url}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
        }));
    }
    let stale = requests.remove(0).await.unwrap();
    for request in requests {
        assert_eq!(request.await.unwrap().status(), StatusCode::OK);
    }
    let stale_headers = stale.headers().clone();
    let stale_body = stale.into_body().collect().await.unwrap().to_bytes();
    let stale_json: serde_json::Value = serde_json::from_slice(&stale_body).unwrap();

    assert_eq!(stale_headers.get("x-cache-status").unwrap(), "HIT");
    assert_eq!(stale_headers.get("x-cache-stale").unwrap(), "1");
    assert_eq!(stale_headers.get("x-cache-refresh").unwrap(), "async");
    assert_eq!(stale_json["data"]["title"], "Old Title");
    wait_for_cached_metadata_title(&sqlite_path, "New Title").await;
}

#[tokio::test]
async fn api_fetches_normalized_target_url() {
    let upstream = MockServer::start().await;
    let local_page_url = format!("{}/page", upstream.uri());
    let page_url = local_page_url
        .replace("127.0.0.1", "mock.example.test")
        .replace("localhost", "mock.example.test");
    Mock::given(method("GET"))
        .and(path("/page"))
        .and(query_param("a", "1"))
        .and(query_param("b", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "<!doctype html><html><head><title>Normalized</title></head></html>",
            "text/html; charset=utf-8",
        ))
        .expect(1)
        .mount(&upstream)
        .await;

    let temp_dir = TempDir::new().unwrap();
    let app = build_test_app(
        temp_dir.path().join("cache.db"),
        "mock.example.test",
        &local_page_url,
    )
    .await;
    let raw_url = format!("{page_url}/?utm_source=newsletter&b=2&a=1");
    let encoded = urlencoding::encode(&raw_url).into_owned();

    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/api?url={encoded}"))
                .header(header::HOST, "service.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["data"]["title"], "Normalized");
    assert_eq!(json["data"]["url"], format!("{page_url}?a=1&b=2"));
}

#[tokio::test]
async fn api_none_cache_backend_always_misses() {
    let upstream = MockServer::start().await;
    let local_page_url = format!("{}/page", upstream.uri());
    let page_url = local_page_url
        .replace("127.0.0.1", "mock.example.test")
        .replace("localhost", "mock.example.test");
    Mock::given(method("GET"))
        .and(path("/page"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "<!doctype html><html><head><title>No cache</title></head></html>",
            "text/html; charset=utf-8",
        ))
        .expect(2)
        .mount(&upstream)
        .await;

    let temp_dir = TempDir::new().unwrap();
    let mut config = test_config(temp_dir.path().join("cache.db"));
    config.cache_backend = CacheBackend::None;
    let url = url::Url::parse(&local_page_url).unwrap();
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::limited(10))
        .resolve(
            "mock.example.test",
            SocketAddr::from(([127, 0, 0, 1], url.port_or_known_default().unwrap())),
        )
        .build()
        .unwrap();
    let app = build_app_with_client(config, client).await.unwrap();
    let encoded = urlencoding::encode(&page_url).into_owned();

    for _ in 0..2 {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api?url={encoded}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers.get("x-cache-status").unwrap(), "MISS");
        assert_eq!(json["data"]["title"], "No cache");
    }
}

#[tokio::test]
async fn api_head_returns_empty_body() {
    let upstream = MockServer::start().await;
    let local_page_url = format!("{}/page", upstream.uri());
    let page_url = local_page_url
        .replace("127.0.0.1", "mock.example.test")
        .replace("localhost", "mock.example.test");
    let image_url = format!("{}/image.png", upstream.uri())
        .replace("127.0.0.1", "mock.example.test")
        .replace("localhost", "mock.example.test");
    Mock::given(method("GET"))
        .and(path("/page"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(sample_html(&image_url), "text/html; charset=utf-8"),
        )
        .mount(&upstream)
        .await;

    let temp_dir = TempDir::new().unwrap();
    let app = build_test_app(
        temp_dir.path().join("cache.db"),
        "mock.example.test",
        &local_page_url,
    )
    .await;
    let url = urlencoding::encode(&page_url).into_owned();

    let response = app
        .oneshot(
            Request::builder()
                .method("HEAD")
                .uri(format!("/api?url={url}"))
                .header(header::HOST, "service.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get(header::CONTENT_TYPE).unwrap(),
        "application/json; charset=utf-8"
    );
    assert!(body.is_empty());
}

#[tokio::test]
async fn unknown_route_returns_json_error() {
    let temp_dir = TempDir::new().unwrap();
    let app = build_app_with_client(
        test_config(temp_dir.path().join("cache.db")),
        reqwest::Client::new(),
    )
    .await
    .unwrap();

    let response = app
        .oneshot(
            Request::builder()
                .uri("/missing")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        headers.get(header::CONTENT_TYPE).unwrap(),
        "application/json; charset=utf-8"
    );
    assert_eq!(headers.get(header::CACHE_CONTROL).unwrap(), "no-store");
    assert_eq!(json["status"], "error");
    assert_eq!(json["error"]["code"], "NOT_FOUND");
}

#[tokio::test]
async fn unsupported_method_returns_json_error() {
    let temp_dir = TempDir::new().unwrap();
    let app = build_app_with_client(
        test_config(temp_dir.path().join("cache.db")),
        reqwest::Client::new(),
    )
    .await
    .unwrap();

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(
        headers.get(header::CONTENT_TYPE).unwrap(),
        "application/json; charset=utf-8"
    );
    assert_eq!(headers.get(header::CACHE_CONTROL).unwrap(), "no-store");
    assert_eq!(json["status"], "error");
    assert_eq!(json["error"]["code"], "METHOD_NOT_ALLOWED");
}

#[tokio::test]
async fn image_proxy_forces_query_referer_and_caches_processed_image() {
    let upstream = MockServer::start().await;
    let png_body = sample_png();
    let local_target_url = format!("{}/cover.png", upstream.uri());
    let target_url = local_target_url
        .replace("127.0.0.1", "image.example.test")
        .replace("localhost", "image.example.test");
    Mock::given(method("GET"))
        .and(path("/cover.png"))
        .and(match_header(
            "referer",
            "https://example.com/post?case=first",
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "image/png")
                .set_body_bytes(png_body),
        )
        .expect(1)
        .mount(&upstream)
        .await;

    let temp_dir = TempDir::new().unwrap();
    let app = build_test_app(
        temp_dir.path().join("cache.db"),
        "image.example.test",
        &local_target_url,
    )
    .await;
    let target = urlencoding::encode(&target_url).into_owned();
    let referer = urlencoding::encode("https://example.com/post?case=first").into_owned();
    let request = || {
        Request::builder()
            .uri(format!("/proxy/image?url={target}&referer={referer}&w=64"))
            .header(header::ACCEPT, "image/avif,image/webp,image/*")
            .header(header::REFERER, "https://attacker.example/fake")
            .body(Body::empty())
            .unwrap()
    };

    let response = app.clone().oneshot(request()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "image/avif"
    );
    assert_eq!(response.headers().get("x-image-optimized").unwrap(), "1");
    assert_eq!(response.headers().get("x-cache-status").unwrap(), "MISS");
    assert_eq!(
        response.headers().get(header::CACHE_CONTROL).unwrap(),
        "public, max-age=86400, immutable"
    );
    assert_eq!(
        response
            .headers()
            .get("cloudflare-cdn-cache-control")
            .unwrap(),
        "public, s-maxage=259200"
    );
    assert!(
        response
            .headers()
            .get("server-timing")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("transform")
    );

    let cached = app.oneshot(request()).await.unwrap();
    assert_eq!(cached.status(), StatusCode::OK);
    assert_eq!(cached.headers().get("x-cache-status").unwrap(), "HIT");
    assert_eq!(
        cached.headers().get(header::CONTENT_TYPE).unwrap(),
        "image/avif"
    );
}

#[tokio::test]
async fn image_proxy_rejects_unknown_formats_instead_of_caching_originals() {
    let upstream = MockServer::start().await;
    let icon_body = vec![0_u8, 0, 1, 0, 1, 0, 16, 16, 0, 0];
    let local_target_url = format!("{}/favicon.ico", upstream.uri());
    let target_url = local_target_url
        .replace("127.0.0.1", "image.example.test")
        .replace("localhost", "image.example.test");
    Mock::given(method("GET"))
        .and(path("/favicon.ico"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "image/x-icon")
                .set_body_bytes(icon_body.clone()),
        )
        .expect(1)
        .mount(&upstream)
        .await;

    let temp_dir = TempDir::new().unwrap();
    let app = build_test_app(
        temp_dir.path().join("cache.db"),
        "image.example.test",
        &local_target_url,
    )
    .await;
    let target = urlencoding::encode(&target_url).into_owned();
    let request = || {
        Request::builder()
            .uri(format!("/proxy/image?url={target}"))
            .header(header::ACCEPT, "image/avif,image/webp,image/*")
            .body(Body::empty())
            .unwrap()
    };

    let response = app.oneshot(request()).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(
        image_cache_row_count(&temp_dir.path().join("cache.db")).await,
        0
    );
}

#[tokio::test]
async fn image_proxy_serves_stale_image_and_refreshes_async() {
    let upstream = MockServer::start().await;
    let old_png = sample_png_with_color([255, 0, 0, 255]);
    let new_png = sample_png_with_color([0, 0, 255, 255]);
    let local_target_url = format!("{}/cover.png", upstream.uri());
    let target_url = local_target_url
        .replace("127.0.0.1", "image.example.test")
        .replace("localhost", "image.example.test");

    Mock::given(method("GET"))
        .and(path("/cover.png"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "image/png")
                .set_body_bytes(old_png.clone()),
        )
        .up_to_n_times(1)
        .mount(&upstream)
        .await;
    Mock::given(method("GET"))
        .and(path("/cover.png"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "image/png")
                .set_body_bytes(new_png),
        )
        .expect(1)
        .mount(&upstream)
        .await;

    let temp_dir = TempDir::new().unwrap();
    let sqlite_path = temp_dir.path().join("cache.db");
    let app = build_test_app(sqlite_path.clone(), "image.example.test", &local_target_url).await;
    let target = urlencoding::encode(&target_url).into_owned();
    let request = || {
        Request::builder()
            .uri(format!("/proxy/image?url={target}&f=png"))
            .body(Body::empty())
            .unwrap()
    };

    let first = app.clone().oneshot(request()).await.unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(first.headers().get("x-cache-status").unwrap(), "MISS");
    let first_body = first.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(first_body.as_ref(), old_png.as_slice());
    let cached_before = cached_image_bytes(&sqlite_path).await.unwrap();
    expire_cache_table(&sqlite_path, "image_cache").await;

    let stale = app.oneshot(request()).await.unwrap();
    let stale_headers = stale.headers().clone();
    let stale_body = stale.into_body().collect().await.unwrap().to_bytes();

    assert_eq!(stale_headers.get("x-cache-status").unwrap(), "HIT");
    assert_eq!(stale_headers.get("x-cache-stale").unwrap(), "1");
    assert_eq!(stale_headers.get("x-cache-refresh").unwrap(), "async");
    assert_eq!(stale_body.as_ref(), cached_before.as_slice());
    wait_for_cached_image_change(&sqlite_path, &cached_before).await;
}

#[tokio::test]
async fn image_proxy_none_cache_backend_always_misses() {
    let upstream = MockServer::start().await;
    let png_body = sample_png();
    let local_target_url = format!("{}/cover.png", upstream.uri());
    let target_url = local_target_url
        .replace("127.0.0.1", "image.example.test")
        .replace("localhost", "image.example.test");
    Mock::given(method("GET"))
        .and(path("/cover.png"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "image/png")
                .set_body_bytes(png_body),
        )
        .expect(2)
        .mount(&upstream)
        .await;

    let temp_dir = TempDir::new().unwrap();
    let mut config = test_config(temp_dir.path().join("cache.db"));
    config.image_cache_backend = ImageCacheBackend::None;
    let url = url::Url::parse(&local_target_url).unwrap();
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::limited(10))
        .resolve(
            "image.example.test",
            SocketAddr::from(([127, 0, 0, 1], url.port_or_known_default().unwrap())),
        )
        .build()
        .unwrap();
    let app = build_app_with_client(config, client).await.unwrap();
    let target = urlencoding::encode(&target_url).into_owned();

    for _ in 0..2 {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/proxy/image?url={target}&f=png"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers().get("x-cache-status").unwrap(), "MISS");
        assert_eq!(response.headers().get("x-image-optimized").unwrap(), "0");
    }
}

#[tokio::test]
async fn image_proxy_redirects_when_s3_cache_backend_is_used() {
    let state = AppState {
        config: test_config(PathBuf::from("unused.db")),
        client: reqwest::Client::new(),
        cache: Arc::new(NoopMetadataCache),
        image_cache: Arc::new(RedirectImageCache),
        api_miss_limiter: Arc::new(Semaphore::new(8)),
        image_miss_limiter: Arc::new(Semaphore::new(1)),
    };
    let app = router_with_state(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/proxy/image?url=https%3A%2F%2Fcdn.example.com%2Fcover.png")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(
        response.headers().get(header::LOCATION).unwrap(),
        "https://preview.example.com/image-cache/v1/f0/f0.jpg"
    );
}

#[tokio::test]
async fn image_proxy_uses_external_worker_in_low_memory_mode() {
    let upstream = MockServer::start().await;
    let png_body = sample_png();
    let local_target_url = format!("{}/cover.png", upstream.uri());
    let target_url = local_target_url
        .replace("127.0.0.1", "image.example.test")
        .replace("localhost", "image.example.test");
    Mock::given(method("GET"))
        .and(path("/cover.png"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "image/png")
                .set_body_bytes(png_body),
        )
        .mount(&upstream)
        .await;

    let temp_dir = TempDir::new().unwrap();
    let mut config = test_config(temp_dir.path().join("cache.db"));
    config.low_memory_mode = true;
    config.image_worker_bin = Some(PathBuf::from(env!("CARGO_BIN_EXE_image_worker")));
    let url = url::Url::parse(&local_target_url).unwrap();
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::limited(10))
        .resolve(
            "image.example.test",
            SocketAddr::from(([127, 0, 0, 1], url.port_or_known_default().unwrap())),
        )
        .build()
        .unwrap();
    let app = build_app_with_client(config, client).await.unwrap();
    let target = urlencoding::encode(&target_url).into_owned();

    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/proxy/image?url={target}&w=64"))
                .header(header::ACCEPT, "image/webp,image/*")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "image/webp"
    );
}

fn sample_png() -> Vec<u8> {
    sample_png_with_color([255, 0, 0, 255])
}

fn sample_png_with_color(color: [u8; 4]) -> Vec<u8> {
    let image = image::RgbaImage::from_pixel(4, 4, image::Rgba(color));
    let mut cursor = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut cursor, image::ImageFormat::Png)
        .unwrap();
    cursor.into_inner()
}

struct NoopMetadataCache;

#[async_trait]
impl CacheStore for NoopMetadataCache {
    async fn get(&self, _key: &str) -> Result<Option<CacheRead>, unfurl_server::error::AppError> {
        Ok(None)
    }

    async fn set(
        &self,
        _key: &str,
        _data: &UnfurlData,
        _ttl: u64,
    ) -> Result<CacheEnvelope, unfurl_server::error::AppError> {
        unreachable!("metadata cache should not be used in this test")
    }

    fn label(&self) -> &'static str {
        "noop"
    }
}

struct RedirectImageCache;

#[async_trait]
impl ImageCacheStore for RedirectImageCache {
    async fn get(
        &self,
        _key: &str,
        _object_key: &str,
    ) -> Result<Option<ImageCacheRead>, unfurl_server::error::AppError> {
        Ok(Some(ImageCacheRead {
            hit: ImageCacheHit::Redirect {
                location: "https://preview.example.com/image-cache/v1/f0/f0.jpg".to_string(),
            },
            is_stale: false,
        }))
    }

    async fn put(
        &self,
        _entry: ImageCacheWrite,
    ) -> Result<ImageCacheHit, unfurl_server::error::AppError> {
        unreachable!("image put should not be used when cache already hits")
    }

    fn label(&self) -> &'static str {
        "s3"
    }
}

#[tokio::test]
async fn postgres_cache_preserves_binary_stale_data_and_survives_reconnect() {
    let base = std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL is required");
    let admin = sqlx::PgPool::connect(&base).await.unwrap();
    let schema = format!(
        "unfurl_test_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin)
        .await
        .unwrap();
    let mut url = url::Url::parse(&base).unwrap();
    url.query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));
    let mut config = test_config(PathBuf::new());
    config.database_url = Some(url.to_string());
    config.cache_backend = CacheBackend::Postgres;
    config.image_cache_backend = ImageCacheBackend::Postgres;
    let cache = unfurl_server::cache::build_cache(&config).await.unwrap();
    let data: UnfurlData = serde_json::from_value(
        serde_json::json!({"url":"https://example.com/test", "title":"Unicode 中文"}),
    )
    .unwrap();
    cache.set("test", &data, 0).await.unwrap();
    drop(cache);
    let cache = unfurl_server::cache::build_cache(&config).await.unwrap();
    let result = cache.get("test").await.unwrap().unwrap();
    assert!(result.is_stale);
    assert_eq!(result.envelope.data, data);
    cache.set("test", &data, 3600).await.unwrap();
    assert!(!cache.get("test").await.unwrap().unwrap().is_stale);
    let images = unfurl_server::image_cache::build_image_cache(&config)
        .await
        .unwrap();
    images
        .put(ImageCacheWrite {
            cache_key: "image".into(),
            object_key: "image".into(),
            bytes: vec![0, 255, 1, 0, 128].into(),
            content_type: "image/png".into(),
            optimized: true,
            ttl: 0,
        })
        .await
        .unwrap();
    drop(images);
    let images = unfurl_server::image_cache::build_image_cache(&config)
        .await
        .unwrap();
    let result = images.get("image", "image").await.unwrap().unwrap();
    assert!(result.is_stale);
    match result.hit {
        ImageCacheHit::Inline(image) => {
            assert_eq!(image.bytes.as_ref(), &[0, 255, 1, 0, 128]);
            assert!(image.optimized);
        }
        _ => panic!("expected inline binary cache"),
    }
    let scoped = sqlx::PgPool::connect(config.database_url.as_deref().unwrap())
        .await
        .unwrap();
    sqlx::query("INSERT INTO unfurl_cache(cache_key,payload_json,cached_at,ttl_seconds,expires_at,last_access) SELECT 'cold-'||n,payload_json,cached_at,ttl_seconds,expires_at,0 FROM generate_series(1,10001) n CROSS JOIN (SELECT * FROM unfurl_cache WHERE cache_key='test') seed").execute(&scoped).await.unwrap();
    sqlx::query("INSERT INTO image_cache(cache_key,content_type,image_bytes,optimized,expires_at,last_access) SELECT 'cold-'||n,content_type,image_bytes,optimized,expires_at,0 FROM generate_series(1,10001) n CROSS JOIN (SELECT * FROM image_cache WHERE cache_key='image') seed").execute(&scoped).await.unwrap();
    cache.get("test").await.unwrap();
    images.get("image", "image").await.unwrap();
    let mut workers = Vec::new();
    for index in 0..16 {
        let cache = cache.clone();
        let images = images.clone();
        let data = data.clone();
        workers.push(tokio::spawn(async move {
            cache
                .set(&format!("new-{index}"), &data, 3600)
                .await
                .unwrap();
            images
                .put(ImageCacheWrite {
                    cache_key: format!("new-{index}"),
                    object_key: String::new(),
                    bytes: vec![1, 2, 3].into(),
                    content_type: "image/png".into(),
                    optimized: false,
                    ttl: 3600,
                })
                .await
                .unwrap();
        }));
    }
    for worker in workers {
        worker.await.unwrap();
    }
    let metadata_count: i64 = sqlx::query_scalar("SELECT count(*) FROM unfurl_cache")
        .fetch_one(&scoped)
        .await
        .unwrap();
    let image_count: i64 = sqlx::query_scalar("SELECT count(*) FROM image_cache")
        .fetch_one(&scoped)
        .await
        .unwrap();
    assert_eq!(metadata_count, 10000);
    assert_eq!(image_count, 10000);
    assert!(cache.get("test").await.unwrap().is_some());
    assert!(images.get("image", "image").await.unwrap().is_some());
    // Highly compressible TOAST fixtures exercise the real logical-byte budgets
    // without allocating half a GiB of physical disk in the test cluster.
    sqlx::query("INSERT INTO unfurl_cache(cache_key,payload_json,cached_at,ttl_seconds,expires_at,last_access) SELECT 'large-'||n,jsonb_set(payload_json::jsonb,'{data,description}',to_jsonb(repeat('x',1048576)))::text,cached_at,ttl_seconds,expires_at,0 FROM generate_series(1,65) n CROSS JOIN (SELECT * FROM unfurl_cache WHERE cache_key='test') seed").execute(&scoped).await.unwrap();
    sqlx::query("INSERT INTO image_cache(cache_key,content_type,image_bytes,optimized,expires_at,last_access) SELECT 'large-'||n,'image/png',convert_to(repeat('x',16777216),'UTF8'),0,expires_at,0 FROM generate_series(1,33) n CROSS JOIN (SELECT * FROM image_cache WHERE cache_key='image') seed").execute(&scoped).await.unwrap();
    cache.set("test", &data, 3600).await.unwrap();
    images
        .put(ImageCacheWrite {
            cache_key: "image".into(),
            object_key: String::new(),
            bytes: vec![1, 2, 3].into(),
            content_type: "image/png".into(),
            optimized: false,
            ttl: 3600,
        })
        .await
        .unwrap();
    let metadata_bytes: i64 =
        sqlx::query_scalar("SELECT sum(octet_length(payload_json))::bigint FROM unfurl_cache")
            .fetch_one(&scoped)
            .await
            .unwrap();
    let image_bytes: i64 =
        sqlx::query_scalar("SELECT sum(octet_length(image_bytes))::bigint FROM image_cache")
            .fetch_one(&scoped)
            .await
            .unwrap();
    assert!(metadata_bytes <= 64 * 1024 * 1024);
    assert!(image_bytes <= 512 * 1024 * 1024);
    sqlx::query("UPDATE unfurl_cache SET expires_at=0 WHERE cache_key='test'")
        .execute(&scoped)
        .await
        .unwrap();
    sqlx::query("UPDATE image_cache SET expires_at=0 WHERE cache_key='image'")
        .execute(&scoped)
        .await
        .unwrap();
    cache.set("trigger", &data, 3600).await.unwrap();
    images
        .put(ImageCacheWrite {
            cache_key: "trigger".into(),
            object_key: String::new(),
            bytes: vec![1].into(),
            content_type: "image/png".into(),
            optimized: false,
            ttl: 3600,
        })
        .await
        .unwrap();
    assert!(cache.get("test").await.unwrap().is_none());
    assert!(images.get("image", "image").await.unwrap().is_none());
    scoped.close().await;
    drop(images);
    drop(cache);
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&admin)
        .await
        .unwrap();
}

#[tokio::test]
async fn concurrent_cold_requests_share_one_origin_fetch() {
    let upstream = MockServer::start().await;
    let local = format!("{}/coalesced", upstream.uri());
    let target = local.replace("127.0.0.1", "mock.example.test");
    Mock::given(method("GET"))
        .and(path("/coalesced"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(100))
                .set_body_raw(
                    "<html><head><title>shared</title></head></html>",
                    "text/html",
                ),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let directory = TempDir::new().unwrap();
    let app = build_test_app(
        directory.path().join("cache.db"),
        "mock.example.test",
        &local,
    )
    .await;
    let mut tasks = Vec::new();
    for _ in 0..100 {
        let app = app.clone();
        let uri = format!("/api?url={}", urlencoding::encode(&target));
        tasks.push(tokio::spawn(async move {
            app.oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap()
        }));
    }
    for task in tasks {
        assert_eq!(task.await.unwrap().status(), StatusCode::OK);
    }
}

#[tokio::test]
async fn failed_helper_never_caches_mislabeled_svg() {
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/evil.png"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "<svg xmlns='http://www.w3.org/2000/svg'><script>void 0</script></svg>",
            "image/png",
        ))
        .expect(1)
        .mount(&upstream)
        .await;
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("cache.db");
    let mut config = test_config(database.clone());
    config.low_memory_mode = true;
    config.image_worker_bin = Some(directory.path().join("missing-worker"));
    let target = upstream.uri().replace("127.0.0.1", "mock.example.test");
    let port = url::Url::parse(&upstream.uri()).unwrap().port().unwrap();
    let client = reqwest::Client::builder()
        .no_proxy()
        .resolve(
            "mock.example.test",
            SocketAddr::from(([127, 0, 0, 1], port)),
        )
        .build()
        .unwrap();
    let app = build_app_with_client(config, client).await.unwrap();
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/proxy/image?url={}",
                    urlencoding::encode(&format!("{target}/evil.png"))
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(image_cache_row_count(&database).await, 0);
}
