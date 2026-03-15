import { validatePublicUrl } from "../utils";

const TRACKING_PARAM_PATTERN = /^(utm_.+|fbclid|gclid)$/i;

export function normalizeTargetUrl(rawUrl: string): string {
  const url = validatePublicUrl(rawUrl);
  url.hash = "";
  url.protocol = url.protocol.toLowerCase();
  url.hostname = url.hostname.toLowerCase();
  url.pathname = normalizePathname(url.pathname);

  const entries = [...url.searchParams.entries()]
    .filter(([key]) => !TRACKING_PARAM_PATTERN.test(key))
    .sort(([leftKey, leftValue], [rightKey, rightValue]) => {
      if (leftKey === rightKey) {
        return leftValue.localeCompare(rightValue);
      }
      return leftKey.localeCompare(rightKey);
    });

  url.search = "";
  for (const [key, value] of entries) {
    url.searchParams.append(key, value);
  }

  return url.toString();
}

function normalizePathname(pathname: string): string {
  const normalized = pathname.length > 1 && pathname.endsWith("/")
    ? pathname.slice(0, -1)
    : pathname;
  return normalized || "/";
}

export function buildUnfurlCacheKey(rawUrl: string): string {
  return `unfurl:v1:${normalizeTargetUrl(rawUrl)}`;
}

export function createEdgeCacheRequest(cacheKey: string): Request {
  return new Request(`https://cache.unfurl.internal/${encodeURIComponent(cacheKey)}`);
}
