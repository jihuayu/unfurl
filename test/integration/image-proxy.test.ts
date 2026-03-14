import worker from "../../src/index";
import { createEnv, createExecutionContext } from "../helpers/worker";

describe("GET /proxy/image", () => {
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("passes through resized images and negotiates avif", async () => {
    const env = createEnv();
    const fetchSpy = vi.fn<typeof fetch>().mockResolvedValue(
      new Response("image-bytes", {
        headers: {
          "content-type": "image/avif",
          "cf-resized": "ok"
        }
      })
    );
    vi.stubGlobal("fetch", fetchSpy as unknown as typeof fetch);

    const response = await worker.fetch(
      new Request("https://service.example/proxy/image?url=https://cdn.example.com/cover.png&w=200&referer=https%3A%2F%2Fexample.com%2Fpost%3Fcase%3Dfirst", {
        headers: {
          accept: "image/avif,image/webp,image/*",
          referer: "https://attacker.example/fake"
        }
      }),
      env,
      createExecutionContext()
    );

    expect(response.status).toBe(200);
    expect(response.headers.get("content-type")).toBe("image/avif");
    expect(response.headers.get("x-image-optimized")).toBe("1");
    expect(response.headers.get("cache-control")).toContain("immutable");
    const requestInit = fetchSpy.mock.calls[0]?.[1] as RequestInit & {
      cf?: { image?: { format?: string; width?: number } };
    };
    expect(requestInit.cf?.image?.format).toBe("avif");
    expect(requestInit.cf?.image?.width).toBe(200);
    const requestHeaders = requestInit.headers as HeadersInit | undefined;
    const forwardedReferer = requestHeaders instanceof Headers
      ? requestHeaders.get("referer")
      : (requestHeaders as Record<string, string> | undefined)?.referer;
    expect(forwardedReferer).toBe("https://example.com/post?case=first");
  });

  it("falls back to simple proxy when cf-resized header is absent", async () => {
    const env = createEnv();
    const fetchSpy = vi.fn<typeof fetch>().mockResolvedValue(
      new Response("raw-image", {
        headers: {
          "content-type": "image/png"
        }
      })
    );
    vi.stubGlobal("fetch", fetchSpy as unknown as typeof fetch);

    const response = await worker.fetch(
      new Request("https://service.example/proxy/image?url=https://cdn.example.com/raw.png"),
      env,
      createExecutionContext()
    );

    expect(response.status).toBe(200);
    expect(response.headers.get("content-type")).toBe("image/png");
    expect(response.headers.get("x-image-optimized")).toBe("0");
  });

  it("rejects non-image upstream responses", async () => {
    const env = createEnv();
    const fetchSpy = vi.fn<typeof fetch>().mockResolvedValue(
      new Response("<html>bad</html>", {
        headers: {
          "content-type": "text/html"
        }
      })
    );
    vi.stubGlobal("fetch", fetchSpy as unknown as typeof fetch);

    const response = await worker.fetch(
      new Request("https://service.example/proxy/image?url=https://cdn.example.com/not-image"),
      env,
      createExecutionContext()
    );

    expect(response.status).toBe(415);
    const json = await response.json<{ error: { code: string } }>();
    expect(json.error.code).toBe("UNSUPPORTED_MEDIA_TYPE");
  });
});

