import type { CacheEnvelope, CacheReadResult, Env, UnfurlData } from "../types";
import { createEdgeCacheRequest } from "./cache-keys";

const memoryEdgeCache = new Map<string, CacheEnvelope>();

function createCacheResponse(envelope: CacheEnvelope): Response {
  return new Response(JSON.stringify(envelope), {
    headers: {
      "content-type": "application/json; charset=utf-8",
      "cache-control": `public, max-age=${envelope.ttl}`
    }
  });
}

async function getEdgeCache(): Promise<Cache> {
  if (typeof caches !== "undefined") {
    return caches.open("unfurl-cache");
  }

  return {
    async match(request: RequestInfo | URL): Promise<Response | undefined> {
      const key = typeof request === "string"
        ? request
        : request instanceof URL
          ? request.toString()
          : request.url;
      const hit = memoryEdgeCache.get(key);
      return hit ? createCacheResponse(hit) : undefined;
    },
    async matchAll(request?: RequestInfo | URL): Promise<readonly Response[]> {
      if (!request) {
        return [...memoryEdgeCache.values()].map((entry) => createCacheResponse(entry));
      }

      const key = typeof request === "string"
        ? request
        : request instanceof URL
          ? request.toString()
          : request.url;
      const hit = memoryEdgeCache.get(key);
      return hit ? [createCacheResponse(hit)] : [];
    },
    async put(request: RequestInfo | URL, response: Response): Promise<void> {
      const key = typeof request === "string"
        ? request
        : request instanceof URL
          ? request.toString()
          : request.url;
      const parsed = await response.clone().json<CacheEnvelope>();
      memoryEdgeCache.set(key, parsed);
    },
    async delete(request: RequestInfo | URL): Promise<boolean> {
      const key = typeof request === "string"
        ? request
        : request instanceof URL
          ? request.toString()
          : request.url;
      return memoryEdgeCache.delete(key);
    },
    async keys(): Promise<readonly Request[]> {
      return [...memoryEdgeCache.keys()].map((key) => new Request(key));
    },
    add(): Promise<void> {
      throw new Error("Not implemented in memory cache");
    },
    addAll(): Promise<void> {
      throw new Error("Not implemented in memory cache");
    }
  } as unknown as Cache;
}

export async function readUnfurlCache(env: Env, cacheKey: string, ctx: ExecutionContext): Promise<CacheReadResult> {
  const edgeRequest = createEdgeCacheRequest(cacheKey);
  const edgeCache = await getEdgeCache();
  const edgeHit = await edgeCache.match(edgeRequest);
  if (edgeHit) {
    const value = await edgeHit.json<CacheEnvelope>();
    return {
      value,
      source: "edge"
    };
  }

  const kvValue = await env.UNFURL_CACHE.get<CacheEnvelope>(cacheKey, "json");
  if (!kvValue) {
    return {
      value: null,
      source: null
    };
  }

  ctx.waitUntil(edgeCache.put(edgeRequest, createCacheResponse(kvValue)));
  return {
    value: kvValue,
    source: "kv"
  };
}

export async function writeUnfurlCache(
  env: Env,
  cacheKey: string,
  data: UnfurlData,
  ttl: number,
  ctx?: ExecutionContext
): Promise<CacheEnvelope> {
  const envelope: CacheEnvelope = {
    data,
    cachedAt: new Date().toISOString(),
    ttl
  };
  const edgeCache = await getEdgeCache();

  const tasks = [
    edgeCache.put(createEdgeCacheRequest(cacheKey), createCacheResponse(envelope)),
    env.UNFURL_CACHE.put(cacheKey, JSON.stringify(envelope), {
      expirationTtl: ttl
    })
  ];

  if (ctx) {
    ctx.waitUntil(Promise.all(tasks));
  }
  await Promise.all(tasks);
  return envelope;
}