import type { ApiErrorResponse, ApiSuccessResponse, Env, ImageFit, ImageFormat, ResponseHeadersShape, UnfurlData } from "./types";

const LOCAL_SUFFIXES = [".local", ".internal", ".localhost"];
const PRIVATE_IPV4_PATTERNS = [
  /^10\./,
  /^127\./,
  /^169\.254\./,
  /^192\.168\./,
  /^172\.(1[6-9]|2\d|3[0-1])\./
];

export const DEFAULT_API_RESPONSE_CACHE_TTL = 3600;
export const DEFAULT_IMAGE_CACHE_TTL = 86400;
export const DEFAULT_OG_CACHE_TTL = 43200;
export const DEFAULT_FETCH_TIMEOUT_MS = 8000;
export const DEFAULT_IMAGE_QUALITY = 80;
export const DEFAULT_IMAGE_FIT: ImageFit = "scale-down";

export class AppError extends Error {
  constructor(
    public readonly status: number,
    public readonly code: string,
    message: string
  ) {
    super(message);
    this.name = "AppError";
  }
}

export class HeadParsedSignal extends Error {
  constructor() {
    super("Head parsed successfully");
    this.name = "HeadParsedSignal";
  }
}

export function jsonResponse(body: ApiSuccessResponse | ApiErrorResponse, status = 200, headers?: HeadersInit): Response {
  return new Response(JSON.stringify(body, null, 2), {
    status,
    headers: {
      "content-type": "application/json; charset=utf-8",
      ...headers
    }
  });
}

export function createSuccessResponse(
  data: UnfurlData,
  cacheStatus: "HIT" | "MISS",
  startedAt: number,
  responseCacheTtl: number,
  extraHeaders?: HeadersInit
): Response {
  const responseHeaders: ResponseHeadersShape = {
    "x-cache-status": cacheStatus,
    "x-response-time": `${Date.now() - startedAt}ms`
  };

  const payload: ApiSuccessResponse = {
    status: "success",
    data,
    headers: responseHeaders
  };

  return jsonResponse(payload, 200, {
    "cache-control": `public, max-age=${responseCacheTtl}`,
    ...responseHeaders,
    ...extraHeaders
  });
}

export function createErrorResponse(error: unknown, startedAt: number): Response {
  const appError = error instanceof AppError ? error : new AppError(500, "INTERNAL_ERROR", "Unexpected internal error");
  const responseTime = `${Date.now() - startedAt}ms`;
  const payload: ApiErrorResponse = {
    status: "error",
    error: {
      code: appError.code,
      message: appError.message
    },
    headers: {
      "x-response-time": responseTime
    }
  };

  return jsonResponse(payload, appError.status, {
    "cache-control": "no-store",
    "x-response-time": responseTime
  });
}

export function createCorsHeaders(): HeadersInit {
  return {
    "access-control-allow-origin": "*",
    "access-control-allow-methods": "GET,HEAD,OPTIONS",
    "access-control-allow-headers": "Content-Type,Accept"
  };
}

export function withCors(response: Response): Response {
  const headers = new Headers(response.headers);
  const corsHeaders = createCorsHeaders();
  Object.entries(corsHeaders).forEach(([key, value]) => headers.set(key, value));

  return new Response(response.body, {
    status: response.status,
    statusText: response.statusText,
    headers
  });
}

export function parseBooleanParam(value: string | null, defaultValue = false): boolean {
  if (value === null || value.length === 0) {
    return defaultValue;
  }

  const normalized = value.trim().toLowerCase();
  if (normalized === "true" || normalized === "1") {
    return true;
  }
  if (normalized === "false" || normalized === "0") {
    return false;
  }

  throw new AppError(400, "INVALID_BOOLEAN", `Invalid boolean value: ${value}`);
}

export function parseNumberParam(
  name: string,
  value: string | null,
  options: { min?: number; max?: number; defaultValue?: number } = {}
): number {
  if (value === null || value.length === 0) {
    if (options.defaultValue !== undefined) {
      return options.defaultValue;
    }
    throw new AppError(400, "MISSING_QUERY_PARAM", `Missing required query parameter: ${name}`);
  }

  const parsed = Number.parseInt(value, 10);
  if (!Number.isFinite(parsed)) {
    throw new AppError(400, "INVALID_NUMBER", `Invalid numeric value for ${name}`);
  }
  if (options.min !== undefined && parsed < options.min) {
    throw new AppError(400, "INVALID_NUMBER", `${name} must be >= ${options.min}`);
  }
  if (options.max !== undefined && parsed > options.max) {
    throw new AppError(400, "INVALID_NUMBER", `${name} must be <= ${options.max}`);
  }
  return parsed;
}

function parseEnvTtl(value: string | undefined, fallback: number): number {
  if (!value) {
    return fallback;
  }

  const parsed = Number.parseInt(value, 10);
  return Number.isFinite(parsed) && parsed > 0 ? parsed : fallback;
}

export function getApiResponseCacheTtl(env: Env): number {
  return parseEnvTtl(env.API_RESPONSE_CACHE_TTL, DEFAULT_API_RESPONSE_CACHE_TTL);
}

export function getImageCacheTtl(env: Env): number {
  return parseEnvTtl(env.IMAGE_CACHE_TTL, DEFAULT_IMAGE_CACHE_TTL);
}

export function getOgCacheTtl(env: Env): number {
  return parseEnvTtl(env.OG_CACHE_TTL, DEFAULT_OG_CACHE_TTL);
}

export function parseOptionalNumberParam(
  name: string,
  value: string | null,
  options: { min?: number; max?: number } = {}
): number | undefined {
  if (value === null || value.length === 0) {
    return undefined;
  }

  return parseNumberParam(name, value, options);
}

export function parseImageFormat(value: string | null): ImageFormat {
  if (value === null || value.length === 0) {
    return "auto";
  }

  const normalized = value.trim().toLowerCase();
  if (["auto", "avif", "webp", "jpeg", "png"].includes(normalized)) {
    return normalized as ImageFormat;
  }

  throw new AppError(400, "INVALID_IMAGE_FORMAT", `Unsupported image format: ${value}`);
}

export function parseImageFit(value: string | null): ImageFit {
  if (value === null || value.length === 0) {
    return DEFAULT_IMAGE_FIT;
  }

  const normalized = value.trim().toLowerCase();
  if (["scale-down", "contain", "cover", "crop", "pad"].includes(normalized)) {
    return normalized as ImageFit;
  }

  throw new AppError(400, "INVALID_IMAGE_FIT", `Unsupported image fit: ${value}`);
}

export function sanitizeText(value: string | null | undefined): string | undefined {
  if (!value) {
    return undefined;
  }

  const normalized = value.replace(/\s+/g, " ").trim();
  return normalized.length > 0 ? normalized : undefined;
}

export function toAbsoluteUrl(value: string | null | undefined, baseUrl: string): string | undefined {
  const sanitized = sanitizeText(value);
  if (!sanitized) {
    return undefined;
  }

  try {
    return new URL(sanitized, baseUrl).toString();
  } catch {
    return undefined;
  }
}

export function validatePublicUrl(rawUrl: string): URL {
  let url: URL;
  try {
    url = new URL(rawUrl);
  } catch {
    throw new AppError(400, "INVALID_URL", "Invalid URL provided");
  }

  if (!["http:", "https:"].includes(url.protocol)) {
    throw new AppError(400, "INVALID_URL_PROTOCOL", "Only http and https URLs are supported");
  }

  const hostname = url.hostname.toLowerCase();
  if (hostname === "localhost" || LOCAL_SUFFIXES.some((suffix) => hostname.endsWith(suffix))) {
    throw new AppError(400, "PRIVATE_HOST", "Private or local hosts are not allowed");
  }

  const strippedHostname = hostname.replace(/^\[/, "").replace(/\]$/, "");
  if (isBlockedIpLiteral(strippedHostname)) {
    throw new AppError(400, "PRIVATE_IP", "Private or loopback IPs are not allowed");
  }

  return url;
}

function isBlockedIpLiteral(hostname: string): boolean {
  if (hostname === "::1" || hostname === "0:0:0:0:0:0:0:1") {
    return true;
  }

  if (hostname.includes(":")) {
    const normalized = hostname.toLowerCase();
    return normalized.startsWith("fc") || normalized.startsWith("fd") || normalized.startsWith("fe80:");
  }

  return PRIVATE_IPV4_PATTERNS.some((pattern) => pattern.test(hostname));
}

export function chooseImageFormat(acceptHeader: string | null, requestedFormat: ImageFormat): Exclude<ImageFormat, "auto"> {
  if (requestedFormat !== "auto") {
    return requestedFormat;
  }

  const normalized = (acceptHeader ?? "").toLowerCase();
  if (normalized.includes("image/avif")) {
    return "avif";
  }
  if (normalized.includes("image/webp")) {
    return "webp";
  }
  return "jpeg";
}

export function buildImageProxyUrl(requestUrl: string, assetUrl: string, refererUrl: string): string {
  const baseUrl = new URL(requestUrl);
  return `${baseUrl.origin}/proxy/image?url=${encodeURIComponent(assetUrl)}&referer=${encodeURIComponent(refererUrl)}`;
}

export function ensureImageContentType(contentType: string | null): void {
  if (!contentType || !contentType.toLowerCase().startsWith("image/")) {
    throw new AppError(415, "UNSUPPORTED_MEDIA_TYPE", "Origin did not return an image payload");
  }
}
