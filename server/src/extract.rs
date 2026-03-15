use scraper::{ElementRef, Html, Selector};

use crate::{
    models::{LogoAsset, MediaAsset, RawHeadMetadata, UnfurlData},
    utils::{sanitize_text, to_absolute_url},
};

pub fn extract_head_metadata(html: &str) -> RawHeadMetadata {
    let document = Html::parse_document(html);
    let html_selector = Selector::parse("html").expect("valid selector");
    let head_selector = Selector::parse("head").expect("valid selector");
    let meta_selector = Selector::parse("meta").expect("valid selector");
    let link_selector = Selector::parse("link").expect("valid selector");
    let title_selector = Selector::parse("title").expect("valid selector");

    let mut metadata = RawHeadMetadata {
        lang: document
            .select(&html_selector)
            .next()
            .and_then(|node| sanitize_text(node.value().attr("lang"))),
        title_chunks: Vec::new(),
        meta: std::collections::HashMap::new(),
        icons: Vec::new(),
        canonical: None,
    };

    let Some(head) = document.select(&head_selector).next() else {
        return metadata;
    };

    for node in head.select(&meta_selector) {
        let key = sanitize_text(node.value().attr("property"))
            .or_else(|| sanitize_text(node.value().attr("name")))
            .or_else(|| sanitize_text(node.value().attr("itemprop")));
        let content = sanitize_text(node.value().attr("content"));
        let (Some(key), Some(content)) = (key, content) else {
            continue;
        };
        metadata
            .meta
            .entry(key.to_ascii_lowercase())
            .or_default()
            .push(content);
    }

    for node in head.select(&link_selector) {
        let href = sanitize_text(node.value().attr("href"));
        let rel = sanitize_text(node.value().attr("rel")).map(|value| value.to_ascii_lowercase());
        let (Some(href), Some(rel)) = (href, rel) else {
            continue;
        };

        if rel.contains("canonical") && metadata.canonical.is_none() {
            metadata.canonical = Some(href.clone());
        }
        if ["icon", "shortcut icon", "apple-touch-icon", "mask-icon"]
            .iter()
            .any(|token| rel.contains(token))
            && !metadata.icons.contains(&href)
        {
            metadata.icons.push(href);
        }
    }

    for node in head.select(&title_selector) {
        let text = collect_text(node);
        if let Some(value) = sanitize_text(Some(&text)) {
            metadata.title_chunks.push(value);
        }
    }

    metadata
}

fn collect_text(node: ElementRef<'_>) -> String {
    node.text().collect::<Vec<_>>().join(" ")
}

fn pick_first(metadata: &RawHeadMetadata, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        metadata
            .meta
            .get(&key.to_ascii_lowercase())
            .and_then(|values| values.first())
            .and_then(|value| sanitize_text(Some(value)))
    })
}

fn parse_dimension(value: Option<String>) -> Option<u32> {
    value
        .and_then(|candidate| candidate.parse::<u32>().ok())
        .filter(|value| *value > 0)
}

fn build_media(
    url: Option<String>,
    base_url: &str,
    width: Option<String>,
    height: Option<String>,
) -> Option<MediaAsset> {
    let absolute_url = to_absolute_url(url.as_deref(), base_url)?;
    Some(MediaAsset {
        url: absolute_url,
        width: parse_dimension(width),
        height: parse_dimension(height),
        proxy: None,
    })
}

fn normalize_publisher(value: Option<String>) -> Option<String> {
    let sanitized = value.and_then(|candidate| sanitize_text(Some(&candidate)));
    sanitized.map(|value| value.strip_prefix('@').unwrap_or(&value).to_string())
}

pub fn merge_meta_tags(metadata: &RawHeadMetadata, base_url: &str) -> UnfurlData {
    let title = pick_first(metadata, &["og:title", "twitter:title"])
        .or_else(|| sanitize_text(Some(&metadata.title_chunks.join(" "))));
    let description = pick_first(
        metadata,
        &["og:description", "twitter:description", "description"],
    );
    let page_url = to_absolute_url(
        pick_first(metadata, &["og:url"])
            .as_deref()
            .or(metadata.canonical.as_deref()),
        base_url,
    )
    .unwrap_or_else(|| base_url.to_string());
    let author = pick_first(metadata, &["article:author", "author", "twitter:creator"]);
    let publisher = normalize_publisher(pick_first(
        metadata,
        &[
            "og:site_name",
            "application-name",
            "publisher",
            "twitter:site",
        ],
    ));
    let date = pick_first(
        metadata,
        &[
            "article:published_time",
            "article:modified_time",
            "date",
            "pubdate",
        ],
    );
    let lang = metadata
        .lang
        .clone()
        .or_else(|| pick_first(metadata, &["og:locale", "content-language"]));
    let image = build_media(
        pick_first(
            metadata,
            &[
                "og:image:secure_url",
                "og:image",
                "twitter:image",
                "twitter:image:src",
            ],
        ),
        base_url,
        pick_first(metadata, &["og:image:width", "twitter:image:width"]),
        pick_first(metadata, &["og:image:height", "twitter:image:height"]),
    );
    let logo = metadata
        .icons
        .first()
        .and_then(|icon| to_absolute_url(Some(icon), base_url))
        .map(|url| LogoAsset { url, proxy: None });
    let video = build_media(
        pick_first(
            metadata,
            &["og:video:secure_url", "og:video", "twitter:player"],
        ),
        base_url,
        pick_first(metadata, &["og:video:width"]),
        pick_first(metadata, &["og:video:height"]),
    );
    let audio = build_media(
        pick_first(metadata, &["og:audio:secure_url", "og:audio"]),
        base_url,
        None,
        None,
    );

    UnfurlData {
        title,
        description,
        image,
        url: page_url,
        author,
        publisher,
        date,
        lang,
        logo,
        video,
        audio,
    }
}
