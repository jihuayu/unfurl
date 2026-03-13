import fullOgHtml from "../fixtures/html/full-og.html?raw";
import malformedHtml from "../fixtures/html/malformed.html?raw";
import twitterOnlyHtml from "../fixtures/html/twitter-only.html?raw";
import { extractHeadMetadata } from "../../src/extractors/meta-handler";
import { mergeMetaTags } from "../../src/extractors/meta-tags";

describe("meta extraction", () => {
  it("prefers og tags over other metadata", async () => {
    const metadata = await extractHeadMetadata(
      new Response(fullOgHtml, {
        headers: {
          "content-type": "text/html; charset=utf-8"
        }
      })
    );

    const result = mergeMetaTags(metadata, "https://example.com/articles/123");

    expect(result.title).toBe("Open Graph Title");
    expect(result.description).toBe("Open Graph Description");
    expect(result.publisher).toBe("Example Publisher");
    expect(result.author).toBe("Jane Doe");
    expect(result.image).toEqual({
      url: "https://example.com/images/cover.png",
      width: 1200,
      height: 630
    });
    expect(result.logo).toEqual({
      url: "https://example.com/favicon.ico"
    });
  });

  it("falls back to twitter cards when og tags are absent", async () => {
    const metadata = await extractHeadMetadata(
      new Response(twitterOnlyHtml, {
        headers: {
          "content-type": "text/html"
        }
      })
    );

    const result = mergeMetaTags(metadata, "https://example.com/post");

    expect(result.title).toBe("Twitter Card Title");
    expect(result.description).toBe("Twitter Card Description");
    expect(result.author).toBe("@ExampleAuthor");
    expect(result.publisher).toBe("ExampleSite");
    expect(result.logo).toEqual({
      url: "https://example.com/apple-touch-icon.png"
    });
  });

  it("stops parsing once body starts", async () => {
    const metadata = await extractHeadMetadata(
      new Response(malformedHtml, {
        headers: {
          "content-type": "text/html"
        }
      })
    );

    const result = mergeMetaTags(metadata, "https://example.com/article");

    expect(result.title).toBe("Broken Title");
    expect(result.description).toBeNull();
    expect(result.image?.url).toBe("https://example.com/broken.png");
  });
});