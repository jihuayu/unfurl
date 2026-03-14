import type { Env, ImageFit, ImageFormat, ImageProxyOptions } from "../types";
import {
  AppError,
  DEFAULT_IMAGE_FIT,
  DEFAULT_IMAGE_QUALITY,
  chooseImageFormat,
  ensureImageContentType,
  getImageCacheTtl,
  parseImageFit,
  parseImageFormat,
  parseNumberParam,
  parseOptionalNumberParam,
  validatePublicUrl
} from "../utils";

interface CloudflareImageOptions {
  fit: ImageFit;
  format: Exclude<ImageFormat, "auto">;
  quality: number;
  width?: number;
  height?: number;
}

export async function handleImageProxy(
  request: Request,
  _env: Env,
  _ctx: ExecutionContext,
  options: ImageProxyOptions = {}
): Promise<Response> {
  const requestUrl = new URL(request.url);
  const rawTargetUrl = requestUrl.searchParams.get("url");
  if (!rawTargetUrl) {
    throw new AppError(400, "MISSING_QUERY_PARAM", "Query parameter url is required");
  }

  const targetUrl = validatePublicUrl(rawTargetUrl).toString();
  const rawReferer = requestUrl.searchParams.get("referer");
  const referer = rawReferer ? validatePublicUrl(rawReferer).toString() : undefined;
  const width = parseOptionalNumberParam("w", requestUrl.searchParams.get("w"), { min: 1, max: 4096 });
  const height = parseOptionalNumberParam("h", requestUrl.searchParams.get("h"), { min: 1, max: 4096 });
  const quality = parseNumberParam("q", requestUrl.searchParams.get("q"), {
    min: 1,
    max: 100,
    defaultValue: DEFAULT_IMAGE_QUALITY
  });
  const requestedFormat = parseImageFormat(requestUrl.searchParams.get("f"));
  const fit = parseImageFit(requestUrl.searchParams.get("fit") ?? DEFAULT_IMAGE_FIT);
  const fetchImpl = options.fetchImpl ?? fetch;
  const format = chooseImageFormat(request.headers.get("accept"), requestedFormat);
  const imageCacheTtl = getImageCacheTtl(_env);
  const imageOptions: CloudflareImageOptions = {
    fit,
    format,
    quality,
    width,
    height
  };

  const upstreamHeaders: HeadersInit = {
    accept: "image/avif,image/webp,image/jpeg,image/png,image/*;q=0.8,*/*;q=0.5",
    ...(referer ? { referer } : {})
  };

  const upstreamResponse = await fetchImpl(targetUrl, {
    headers: upstreamHeaders,
    cf: {
      cacheEverything: true,
      cacheTtl: imageCacheTtl,
      image: imageOptions
    }
  } as RequestInit & { cf: { cacheEverything: boolean; cacheTtl: number; image: CloudflareImageOptions } });

  if (!upstreamResponse.ok) {
    throw new AppError(upstreamResponse.status, "UPSTREAM_FETCH_ERROR", `Image origin returned ${upstreamResponse.status}`);
  }

  ensureImageContentType(upstreamResponse.headers.get("content-type"));

  const headers = new Headers(upstreamResponse.headers);
  headers.set("cache-control", `public, max-age=${imageCacheTtl}, immutable`);
  headers.set("vary", "Accept");
  headers.set("x-image-optimized", upstreamResponse.headers.has("cf-resized") ? "1" : "0");

  return new Response(upstreamResponse.body, {
    status: upstreamResponse.status,
    statusText: upstreamResponse.statusText,
    headers
  });
}

