use unfurl_server::{
    extract::{extract_head_metadata, merge_meta_tags},
    models::{ImageFit, ImageFormat, ImageRequest},
    utils::{
        build_processed_image_cache_key, build_processed_image_object_key, build_unfurl_cache_key,
        choose_image_format, normalize_target_url,
    },
};

#[cfg(unix)]
#[tokio::test]
async fn timed_out_worker_is_killed_even_when_stdin_blocks() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let worker = directory.path().join("worker");
    let pid_file = directory.path().join("pid");
    std::fs::write(
        &worker,
        format!(
            "#!/bin/sh\nprintf '%s' \"$$\" > '{}'\nexec sleep 30\n",
            pid_file.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o700)).unwrap();
    let request = ImageRequest {
        width: None,
        height: None,
        quality: 80,
        format: ImageFormat::Webp,
        fit: ImageFit::ScaleDown,
    };
    let started = std::time::Instant::now();
    let error = unfurl_server::image_worker::process_image_with_helper(
        Some(&worker),
        &vec![0; 512 * 1024],
        "image/png",
        &request,
        1000,
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, "IMAGE_TRANSFORM_TIMEOUT");
    assert!(started.elapsed() < std::time::Duration::from_secs(3));
    let pid = std::fs::read_to_string(pid_file).unwrap();
    for _ in 0..20 {
        if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("超时子进程仍存活：{pid}");
}

#[test]
fn large_valid_image_skips_expensive_avif_encoding() {
    let mut png = Vec::new();
    image::DynamicImage::new_rgb8(2001, 2000)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    let request = ImageRequest {
        width: Some(1200),
        height: None,
        quality: 80,
        format: ImageFormat::Avif,
        fit: ImageFit::ScaleDown,
    };
    let result = unfurl_server::image_proxy::process_image(&png, "image/png", &request).unwrap();
    assert_eq!(result.bytes, png);
    assert_eq!(result.content_type, "image/png");
}

#[test]
fn cache_key_preserves_path_and_query_case() {
    let lower = build_unfurl_cache_key("https://Example.com/Post?Case=A").unwrap();
    let upper = build_unfurl_cache_key("https://example.com/post?case=A").unwrap();
    assert_ne!(lower, upper);
}

#[test]
fn normalize_target_url_drops_empty_query_marker() {
    let normalized = normalize_target_url("https://Example.com/Post").unwrap();
    assert_eq!(normalized, "https://example.com/Post");
}

#[test]
fn choose_image_format_negotiates_avif_then_webp_then_jpeg() {
    assert_eq!(
        choose_image_format(Some("image/avif,image/webp,image/*"), &ImageFormat::Auto),
        ImageFormat::Avif
    );
    assert_eq!(
        choose_image_format(Some("image/webp,image/*"), &ImageFormat::Auto),
        ImageFormat::Webp
    );
    assert_eq!(
        choose_image_format(Some("image/png,image/*"), &ImageFormat::Auto),
        ImageFormat::Jpeg
    );
}

#[test]
fn metadata_parser_ignores_body_meta() {
    let html = r#"<!doctype html>
<html lang="zh-CN">
  <head>
    <meta property="og:title" content="Head Title" />
  </head>
  <body>
    <meta property="og:title" content="Body Title" />
  </body>
</html>"#;

    let metadata = extract_head_metadata(html);
    let data = merge_meta_tags(&metadata, "https://example.com/post");
    assert_eq!(data.title.as_deref(), Some("Head Title"));
    assert_eq!(data.lang.as_deref(), Some("zh-CN"));
}

#[test]
fn processed_image_cache_key_changes_with_request_signature() {
    let first = build_processed_image_cache_key(
        "https://cdn.example.com/cover.png",
        Some("https://example.com/post"),
        &ImageRequest {
            width: Some(1200),
            height: Some(630),
            quality: 80,
            format: ImageFormat::Avif,
            fit: ImageFit::Cover,
        },
    );
    let second = build_processed_image_cache_key(
        "https://cdn.example.com/cover.png",
        Some("https://example.com/post"),
        &ImageRequest {
            width: Some(1200),
            height: Some(630),
            quality: 90,
            format: ImageFormat::Avif,
            fit: ImageFit::Cover,
        },
    );

    assert_ne!(first, second);
}

#[test]
fn processed_image_object_key_uses_expected_extension() {
    let object_key = build_processed_image_object_key(
        "image-cache",
        "image:v1:abcdef123456",
        &ImageFormat::Webp,
    );

    assert_eq!(object_key, "image-cache/v1/ab/abcdef123456.webp");
}

#[tokio::test]
async fn helper_preserves_image_resource_limit_status() {
    use image::{ImageEncoder, codecs::png::PngEncoder};
    use unfurl_server::{
        image_worker::process_image_with_helper,
        models::{ImageFit, ImageFormat, ImageRequest},
    };
    let mut png = Vec::new();
    PngEncoder::new(&mut png)
        .write_image(
            &vec![0; 17000 * 3],
            17000,
            1,
            image::ExtendedColorType::Rgb8,
        )
        .unwrap();
    let worker = std::path::PathBuf::from(env!("CARGO_BIN_EXE_image_worker"));
    let request = ImageRequest {
        width: Some(20),
        height: Some(20),
        quality: 80,
        format: ImageFormat::Png,
        fit: ImageFit::Contain,
    };
    let error = process_image_with_helper(Some(&worker), &png, "image/png", &request, 3000)
        .await
        .unwrap_err();
    assert_eq!(error.status, axum::http::StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(error.code, "IMAGE_RESOURCE_LIMIT");
}

#[test]
fn image_proxy_rejects_svg_and_mislabeled_active_payloads() {
    use unfurl_server::image_proxy::validate_raster;
    let svg = b"<svg xmlns='http://www.w3.org/2000/svg'><script>void 0</script></svg>";
    assert!(validate_raster(svg, "image/svg+xml").is_err());
    assert!(validate_raster(svg, "image/png").is_err());
    assert!(validate_raster(b"GIF89a", "image/gif").is_ok());
    assert!(validate_raster(b"GIF89a", "image/png").is_err());
}

#[test]
fn image_cache_generation_excludes_legacy_unchecked_objects() {
    let request = ImageRequest {
        width: None,
        height: None,
        quality: 80,
        format: ImageFormat::Webp,
        fit: ImageFit::Cover,
    };
    assert!(
        build_processed_image_cache_key("https://example.com/image", None, &request)
            .starts_with("image:v2:")
    );
}
