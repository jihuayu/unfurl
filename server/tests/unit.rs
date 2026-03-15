use unfurl_server::{
    extract::{extract_head_metadata, merge_meta_tags},
    models::ImageFormat,
    utils::{build_unfurl_cache_key, choose_image_format},
};

#[test]
fn cache_key_preserves_path_and_query_case() {
    let lower = build_unfurl_cache_key("https://Example.com/Post?Case=A").unwrap();
    let upper = build_unfurl_cache_key("https://example.com/post?case=A").unwrap();
    assert_ne!(lower, upper);
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
