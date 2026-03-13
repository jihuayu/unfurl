import { chooseImageFormat, validatePublicUrl } from "../../src/utils";

describe("validatePublicUrl", () => {
  it("accepts public http and https urls", () => {
    expect(validatePublicUrl("https://example.com/a").toString()).toBe("https://example.com/a");
    expect(validatePublicUrl("http://example.com/").toString()).toBe("http://example.com/");
  });

  it("rejects localhost and private networks", () => {
    expect(() => validatePublicUrl("http://localhost:3000/test")).toThrow(/Private or local hosts/);
    expect(() => validatePublicUrl("http://127.0.0.1/test")).toThrow(/Private or loopback IPs/);
    expect(() => validatePublicUrl("http://192.168.1.5/test")).toThrow(/Private or loopback IPs/);
    expect(() => validatePublicUrl("http://service.internal/test")).toThrow(/Private or local hosts/);
    expect(() => validatePublicUrl("ftp://example.com/file")).toThrow(/Only http and https/);
  });
});

describe("chooseImageFormat", () => {
  it("negotiates avif then webp then jpeg", () => {
    expect(chooseImageFormat("image/avif,image/webp,image/*", "auto")).toBe("avif");
    expect(chooseImageFormat("image/webp,image/*", "auto")).toBe("webp");
    expect(chooseImageFormat("image/png,image/*", "auto")).toBe("jpeg");
    expect(chooseImageFormat("image/png", "png")).toBe("png");
  });
});