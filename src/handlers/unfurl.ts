import { buildUnfurlCacheKey, normalizeTargetUrl } from "../cache/cache-keys";
import { readUnfurlCache, writeUnfurlCache } from "../cache/cache-manager";
import { extractHeadMetadata } from "../extractors/meta-handler";
import { mergeMetaTags } from "../extractors/meta-tags";
import { fetchPage } from "../fetcher/fetch-page";
import type { Env } from "../types";
import { AppError, buildImageProxyUrl, createSuccessResponse, getApiResponseCacheTtl, getOgCacheTtl, parseBooleanParam, parseNumberParam, validatePublicUrl } from "../utils";

export async function handleUnfurl(
  request: Request,
  env: Env,
  ctx: ExecutionContext,
  options: { fetchImpl?: typeof fetch } = {}
): Promise<Response> {
  const startedAt = Date.now();
  const requestUrl = new URL(request.url);
  const rawTargetUrl = requestUrl.searchParams.get("url");
  if (!rawTargetUrl) {
    throw new AppError(400, "MISSING_QUERY_PARAM", "Query parameter url is required");
  }

  const force = parseBooleanParam(requestUrl.searchParams.get("force"), false);
  const ttl = parseNumberParam("ttl", requestUrl.searchParams.get("ttl"), {
    min: 60,
    max: 604800,
    defaultValue: getOgCacheTtl(env)
  });

  const normalizedTargetUrl = normalizeTargetUrl(rawTargetUrl);
  const cacheKey = buildUnfurlCacheKey(rawTargetUrl);

  if (!force) {
    const cached = await readUnfurlCache(env, cacheKey, ctx);
    if (cached.value) {
      return createSuccessResponse(cached.value.data, "HIT", startedAt, getApiResponseCacheTtl(env), {
        "x-cache-source": cached.source ?? "unknown"
      });
    }
  }

  const targetUrl = validatePublicUrl(normalizedTargetUrl).toString();
  const upstreamResponse = await fetchPage(targetUrl, {
    fetchImpl: options.fetchImpl
  });

  if (!upstreamResponse.ok) {
    throw new AppError(upstreamResponse.status, "UPSTREAM_FETCH_ERROR", `Origin returned ${upstreamResponse.status}`);
  }

  const contentType = upstreamResponse.headers.get("content-type")?.toLowerCase() ?? "";
  if (!contentType.includes("text/html") && !contentType.includes("application/xhtml+xml")) {
    throw new AppError(415, "UNSUPPORTED_CONTENT_TYPE", "Only HTML pages can be unfurled");
  }

  const metadata = await extractHeadMetadata(upstreamResponse);
  const data = mergeMetaTags(metadata, targetUrl);

  if (data.image) {
    data.image.proxy = buildImageProxyUrl(request.url, data.image.url, targetUrl);
  }
  if (data.logo) {
    data.logo.proxy = buildImageProxyUrl(request.url, data.logo.url, targetUrl);
  }

  await writeUnfurlCache(env, cacheKey, data, ttl, ctx);
  return createSuccessResponse(data, "MISS", startedAt, getApiResponseCacheTtl(env), {
    "x-cache-source": "origin"
  });
}
