import { buildUnfurlCacheKey, normalizeTargetUrl } from "../../src/cache/cache-keys";

describe("cache key normalization", () => {
  it("removes tracking params, trims trailing slash and sorts query params", () => {
    const normalized = normalizeTargetUrl("HTTPS://Example.COM/Path/?b=2&utm_source=x&a=1&fbclid=123");

    expect(normalized).toBe("https://example.com/Path?a=1&b=2");
    expect(buildUnfurlCacheKey("https://example.com/Path/?b=2&a=1")).toBe("unfurl:v1:https://example.com/Path?a=1&b=2");
  });

  it("preserves case-sensitive pathname and query keys", () => {
    expect(buildUnfurlCacheKey("https://example.com/Post?ID=1")).not.toBe(
      buildUnfurlCacheKey("https://example.com/post?id=1")
    );
  });
});
