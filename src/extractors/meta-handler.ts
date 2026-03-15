import type { RawHeadMetadata } from "../types";
import { sanitizeText } from "../utils";

function appendMetaValue(metadata: RawHeadMetadata, key: string, value: string): void {
  const normalizedKey = key.toLowerCase();
  const existing = metadata.meta.get(normalizedKey) ?? [];
  existing.push(value);
  metadata.meta.set(normalizedKey, existing);
}

class HtmlHandler {
  constructor(private readonly metadata: RawHeadMetadata) {}

  element(element: Element): void {
    const lang = sanitizeText(element.getAttribute("lang"));
    if (lang) {
      this.metadata.lang = lang;
    }
  }
}

class MetaHandler {
  constructor(private readonly metadata: RawHeadMetadata) {}

  element(element: Element): void {
    const key = sanitizeText(element.getAttribute("property"))
      ?? sanitizeText(element.getAttribute("name"))
      ?? sanitizeText(element.getAttribute("itemprop"));
    const content = sanitizeText(element.getAttribute("content"));
    if (!key || !content) {
      return;
    }

    appendMetaValue(this.metadata, key, content);
  }
}

class LinkHandler {
  constructor(private readonly metadata: RawHeadMetadata) {}

  element(element: Element): void {
    const href = sanitizeText(element.getAttribute("href"));
    const rel = sanitizeText(element.getAttribute("rel"))?.toLowerCase();
    if (!href || !rel) {
      return;
    }

    if (rel.includes("canonical") && !this.metadata.canonical) {
      this.metadata.canonical = href;
    }

    if (["icon", "shortcut icon", "apple-touch-icon", "mask-icon"].some((token) => rel.includes(token))) {
      if (!this.metadata.icons.includes(href)) {
        this.metadata.icons.push(href);
      }
    }
  }
}

class TitleHandler {
  constructor(private readonly metadata: RawHeadMetadata) {}

  text(text: Text): void {
    const chunk = sanitizeText(text.text);
    if (chunk) {
      this.metadata.titleChunks.push(chunk);
    }
  }
}

export function createEmptyMetadata(): RawHeadMetadata {
  return {
    titleChunks: [],
    meta: new Map<string, string[]>(),
    icons: []
  };
}

export async function extractHeadMetadata(response: Response): Promise<RawHeadMetadata> {
  if (typeof HTMLRewriter === "undefined") {
    return extractHeadMetadataFallback(response);
  }

  const metadata = createEmptyMetadata();
  const transformed = new HTMLRewriter()
    .on("html", new HtmlHandler(metadata))
    .on("head meta", new MetaHandler(metadata))
    .on("head link", new LinkHandler(metadata))
    .on("head title", new TitleHandler(metadata))
    .transform(response);

  await transformed.arrayBuffer();

  return metadata;
}

async function extractHeadMetadataFallback(response: Response): Promise<RawHeadMetadata> {
  const metadata = createEmptyMetadata();
  const html = await response.text();
  const bodyIndex = html.search(/<body\b/i);
  const headSlice = bodyIndex >= 0 ? html.slice(0, bodyIndex) : html;

  const htmlTagMatch = headSlice.match(/<html\b[^>]*>/i)?.[0];
  const langMatch = htmlTagMatch?.match(/\blang\s*=\s*["']([^"']+)["']/i);
  if (langMatch?.[1]) {
    metadata.lang = sanitizeText(langMatch[1]);
  }

  const titleMatch = headSlice.match(/<title[^>]*>([\s\S]*?)<\/title>/i);
  if (titleMatch?.[1]) {
    const text = sanitizeText(titleMatch[1].replace(/<[^>]+>/g, ""));
    if (text) {
      metadata.titleChunks.push(text);
    }
  }

  for (const tag of headSlice.match(/<meta\b[^>]*>/gi) ?? []) {
    const key = sanitizeText(extractAttribute(tag, "property"))
      ?? sanitizeText(extractAttribute(tag, "name"))
      ?? sanitizeText(extractAttribute(tag, "itemprop"));
    const content = sanitizeText(extractAttribute(tag, "content"));
    if (!key || !content) {
      continue;
    }
    appendMetaValue(metadata, key, content);
  }

  for (const tag of headSlice.match(/<link\b[^>]*>/gi) ?? []) {
    const href = sanitizeText(extractAttribute(tag, "href"));
    const rel = sanitizeText(extractAttribute(tag, "rel"))?.toLowerCase();
    if (!href || !rel) {
      continue;
    }
    if (rel.includes("canonical") && !metadata.canonical) {
      metadata.canonical = href;
    }
    if (["icon", "shortcut icon", "apple-touch-icon", "mask-icon"].some((token) => rel.includes(token))) {
      if (!metadata.icons.includes(href)) {
        metadata.icons.push(href);
      }
    }
  }

  return metadata;
}

function extractAttribute(tag: string, attribute: string): string | undefined {
  const pattern = new RegExp(`\\b${attribute}\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))`, "i");
  const match = tag.match(pattern);
  return match?.[1] ?? match?.[2] ?? match?.[3];
}
