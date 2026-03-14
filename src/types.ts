export interface Env {
  UNFURL_CACHE: KVNamespace;
  API_RESPONSE_CACHE_TTL?: string;
  IMAGE_CACHE_TTL?: string;
  OG_CACHE_TTL?: string;
}

export type CacheStatus = "HIT" | "MISS";
export type CacheSource = "edge" | "kv" | null;
export type ImageFormat = "avif" | "webp" | "jpeg" | "png" | "auto";
export type ImageFit = "scale-down" | "contain" | "cover" | "crop" | "pad";

export interface ResponseHeadersShape {
  "x-cache-status": CacheStatus;
  "x-response-time": string;
}

export interface MediaAsset {
  url: string;
  width?: number;
  height?: number;
  proxy?: string;
}

export interface LogoAsset {
  url: string;
  proxy?: string;
}

export interface UnfurlData {
  title: string | null;
  description: string | null;
  image: MediaAsset | null;
  url: string;
  author: string | null;
  publisher: string | null;
  date: string | null;
  lang: string | null;
  logo: LogoAsset | null;
  video: MediaAsset | null;
  audio: MediaAsset | null;
}

export interface ApiSuccessResponse {
  status: "success";
  data: UnfurlData;
  headers: ResponseHeadersShape;
}

export interface ApiErrorResponse {
  status: "error";
  error: {
    code: string;
    message: string;
  };
  headers?: Partial<ResponseHeadersShape>;
}

export interface RawHeadMetadata {
  lang?: string;
  titleChunks: string[];
  meta: Map<string, string[]>;
  icons: string[];
  canonical?: string;
}

export interface CacheEnvelope {
  data: UnfurlData;
  cachedAt: string;
  ttl: number;
}

export interface CacheReadResult {
  value: CacheEnvelope | null;
  source: CacheSource;
}

export interface FetchPageOptions {
  timeoutMs?: number;
  maxAttempts?: number;
  fetchImpl?: typeof fetch;
}

export interface ImageProxyOptions {
  fetchImpl?: typeof fetch;
}
