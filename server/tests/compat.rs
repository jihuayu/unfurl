use std::{net::SocketAddr, path::PathBuf};

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use tempfile::TempDir;
use tower::ServiceExt;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header as match_header, method, path},
};

use unfurl_server::{
    build_app_with_client,
    config::{CacheBackend, Config},
};

fn test_config(sqlite_path: PathBuf) -> Config {
    Config {
        host: "127.0.0.1".to_string(),
        port: 0,
        api_response_cache_ttl: 3600,
        image_cache_ttl: 86400,
        og_cache_ttl: 43200,
        fetch_timeout_ms: 8000,
        cache_backend: CacheBackend::Sqlite,
        sqlite_path,
        redis_url: None,
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
    assert_eq!(
        first_headers.get(header::CACHE_CONTROL).unwrap(),
        "public, max-age=3600"
    );
    assert_eq!(first_json["status"], "success");
    assert_eq!(first_json["data"]["title"], "Example Title");
    assert_eq!(first_json["data"]["publisher"], "publisher");
    assert!(
        first_json["data"]["image"]["proxy"]
            .as_str()
            .unwrap()
            .contains("referer=")
    );

    let second = app
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
    let second_status = second.status();
    let second_headers = second.headers().clone();
    let second_body = second.into_body().collect().await.unwrap().to_bytes();
    let second_json: serde_json::Value = serde_json::from_slice(&second_body).unwrap();

    assert_eq!(second_status, StatusCode::OK);
    assert_eq!(second_headers.get("x-cache-status").unwrap(), "HIT");
    assert_eq!(second_headers.get("x-cache-source").unwrap(), "sqlite");
    assert_eq!(second_json["data"]["title"], "Example Title");
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
async fn image_proxy_forces_query_referer_and_transcodes() {
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

    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/proxy/image?url={target}&referer={referer}&w=64"))
                .header(header::ACCEPT, "image/avif,image/webp,image/*")
                .header(header::REFERER, "https://attacker.example/fake")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "image/avif"
    );
    assert_eq!(response.headers().get("x-image-optimized").unwrap(), "1");
    assert_eq!(
        response.headers().get(header::CACHE_CONTROL).unwrap(),
        "public, max-age=86400, immutable"
    );
}

fn sample_png() -> Vec<u8> {
    let image = image::RgbaImage::from_pixel(4, 4, image::Rgba([255, 0, 0, 255]));
    let mut cursor = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut cursor, image::ImageFormat::Png)
        .unwrap();
    cursor.into_inner()
}
