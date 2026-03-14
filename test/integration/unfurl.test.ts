import fullOgHtml from "../fixtures/html/full-og.html?raw";
import { buildUnfurlCacheKey } from "../../src/cache/cache-keys";
import worker from "../../src/index";
import { createEnv, createExecutionContext } from "../helpers/worker";

describe("GET /api", () => {
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("returns unfurled metadata and then serves cache hits", async () => {
    const env = createEnv();
    const ctx = createExecutionContext();
    const originFetch = vi.fn<typeof fetch>().mockImplementation(async () =>
      new Response(fullOgHtml, {
        headers: {
          "content-type": "text/html; charset=utf-8"
        }
      })
    );
    vi.stubGlobal("fetch", originFetch as unknown as typeof fetch);

    const firstResponse = await worker.fetch(
      new Request("https://service.example/api?url=https://example.com/post?case=first"),
      env,
      ctx
    );
    await ctx.drain();

    expect(firstResponse.status).toBe(200);
    const firstJson = await firstResponse.json<{ status: string; data: { image: { proxy: string } }; headers: { "x-cache-status": string } }>();
    expect(firstJson.status).toBe("success");
    expect(firstJson.headers["x-cache-status"]).toBe("MISS");
    expect(firstResponse.headers.get("cache-control")).toBe("public, max-age=3600");
    expect(firstJson.data.image.proxy).toContain("/proxy/image?url=");
    expect(firstJson.data.image.proxy).toContain("referer=");
    expect(firstJson.data.image.proxy).toContain(encodeURIComponent("https://example.com/post?case=first"));
    const cachedEnvelope = await env.UNFURL_CACHE.get<{ ttl: number }>(
      buildUnfurlCacheKey("https://example.com/post?case=first"),
      "json"
    );
    expect(cachedEnvelope?.ttl).toBe(43200);
    expect(originFetch).toHaveBeenCalledTimes(1);

    const secondResponse = await worker.fetch(
      new Request("https://service.example/api?url=https://example.com/post?case=first"),
      env,
      createExecutionContext()
    );
    const secondJson = await secondResponse.json<{ headers: { "x-cache-status": string } }>();
    expect(secondJson.headers["x-cache-status"]).toBe("HIT");
    expect(originFetch).toHaveBeenCalledTimes(1);
  });

  it("bypasses cache when force=true", async () => {
    const env = createEnv();
    const firstCtx = createExecutionContext();
    const secondCtx = createExecutionContext();
    const originFetch = vi.fn<typeof fetch>().mockImplementation(async () =>
      new Response(fullOgHtml, {
        headers: {
          "content-type": "text/html"
        }
      })
    );
    vi.stubGlobal("fetch", originFetch as unknown as typeof fetch);

    await worker.fetch(
      new Request("https://service.example/api?url=https://example.com/post?case=force"),
      env,
      firstCtx
    );
    await firstCtx.drain();

    const response = await worker.fetch(
      new Request("https://service.example/api?url=https://example.com/post?case=force&force=true"),
      env,
      secondCtx
    );
    await secondCtx.drain();

    const json = await response.json<{ headers: { "x-cache-status": string } }>();
    expect(json.headers["x-cache-status"]).toBe("MISS");
    expect(originFetch).toHaveBeenCalledTimes(2);
  });

  it("uses environment overrides for api response cache and og data cache", async () => {
    const env = createEnv({
      API_RESPONSE_CACHE_TTL: "7200",
      OG_CACHE_TTL: "86400"
    });
    const ctx = createExecutionContext();
    const originFetch = vi.fn<typeof fetch>().mockImplementation(async () =>
      new Response(fullOgHtml, {
        headers: {
          "content-type": "text/html"
        }
      })
    );
    vi.stubGlobal("fetch", originFetch as unknown as typeof fetch);

    const response = await worker.fetch(
      new Request("https://service.example/api?url=https://example.com/post?case=env"),
      env,
      ctx
    );
    await ctx.drain();

    expect(response.headers.get("cache-control")).toBe("public, max-age=7200");
    const cachedEnvelope = await env.UNFURL_CACHE.get<{ ttl: number }>(
      buildUnfurlCacheKey("https://example.com/post?case=env"),
      "json"
    );
    expect(cachedEnvelope?.ttl).toBe(86400);
  });

  it("rejects private targets", async () => {
    const env = createEnv();
    const response = await worker.fetch(
      new Request("https://service.example/api?url=http://127.0.0.1/admin"),
      env,
      createExecutionContext()
    );

    expect(response.status).toBe(400);
    const json = await response.json<{ error: { code: string } }>();
    expect(json.error.code).toBe("PRIVATE_IP");
  });
});

