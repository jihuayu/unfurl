import type { MediaAsset, RawHeadMetadata, UnfurlData } from "../types";
import { sanitizeText, toAbsoluteUrl } from "../utils";

function pickFirst(metadata: RawHeadMetadata, keys: string[]): string | undefined {
  for (const key of keys) {
    const value = metadata.meta.get(key.toLowerCase())?.[0];
    const sanitized = sanitizeText(value);
    if (sanitized) {
      return sanitized;
    }
  }
  return undefined;
}

function parseDimension(value: string | undefined): number | undefined {
  if (!value) {
    return undefined;
  }

  const parsed = Number.parseInt(value, 10);
  return Number.isFinite(parsed) && parsed > 0 ? parsed : undefined;
}

function buildMedia(url: string | undefined, baseUrl: string, width?: string, height?: string): MediaAsset | null {
  const absoluteUrl = toAbsoluteUrl(url, baseUrl);
  if (!absoluteUrl) {
    return null;
  }

  const media: MediaAsset = { url: absoluteUrl };
  const parsedWidth = parseDimension(width);
  const parsedHeight = parseDimension(height);
  if (parsedWidth) {
    media.width = parsedWidth;
  }
  if (parsedHeight) {
    media.height = parsedHeight;
  }
  return media;
}

function normalizePublisher(value: string | undefined): string | null {
  const sanitized = sanitizeText(value);
  if (!sanitized) {
    return null;
  }

  return sanitized.startsWith("@") ? sanitized.slice(1) : sanitized;
}

export function mergeMetaTags(metadata: RawHeadMetadata, baseUrl: string): UnfurlData {
  const title = pickFirst(metadata, ["og:title", "twitter:title"])
    ?? sanitizeText(metadata.titleChunks.join(" "))
    ?? null;
  const description = pickFirst(metadata, ["og:description", "twitter:description", "description"])
    ?? null;
  const pageUrl = toAbsoluteUrl(
    pickFirst(metadata, ["og:url"]) ?? metadata.canonical,
    baseUrl
  ) ?? baseUrl;
  const author = pickFirst(metadata, ["article:author", "author", "twitter:creator"])
    ?? null;
  const publisher = normalizePublisher(
    pickFirst(metadata, ["og:site_name", "application-name", "publisher", "twitter:site"])
  );
  const date = pickFirst(metadata, ["article:published_time", "article:modified_time", "date", "pubdate"])
    ?? null;
  const lang = sanitizeText(metadata.lang ?? pickFirst(metadata, ["og:locale", "content-language"])) ?? null;
  const image = buildMedia(
    pickFirst(metadata, ["og:image:secure_url", "og:image", "twitter:image", "twitter:image:src"]),
    baseUrl,
    pickFirst(metadata, ["og:image:width", "twitter:image:width"]),
    pickFirst(metadata, ["og:image:height", "twitter:image:height"])
  );
  const logoUrl = toAbsoluteUrl(metadata.icons[0], baseUrl);
  const video = buildMedia(
    pickFirst(metadata, ["og:video:secure_url", "og:video", "twitter:player"]),
    baseUrl,
    pickFirst(metadata, ["og:video:width"]),
    pickFirst(metadata, ["og:video:height"])
  );
  const audio = buildMedia(
    pickFirst(metadata, ["og:audio:secure_url", "og:audio"]),
    baseUrl
  );

  return {
    title,
    description,
    image,
    url: pageUrl,
    author,
    publisher,
    date,
    lang,
    logo: logoUrl ? { url: logoUrl } : null,
    video,
    audio
  };
}