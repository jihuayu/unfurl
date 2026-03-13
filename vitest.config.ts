import { cloudflareTest } from "@cloudflare/vitest-pool-workers";
import { defineConfig } from "vitest/config";

const useCloudflarePool = process.env.CF_POOL === "1";

export default defineConfig({
  plugins: useCloudflarePool
    ? [
      cloudflareTest({
        main: "./src/index.ts",
        wrangler: {
          configPath: "./wrangler.toml"
        },
        miniflare: {
          kvNamespaces: ["UNFURL_CACHE"]
        }
      })
    ]
    : [],
  test: {
    include: ["test/**/*.test.ts"],
    globals: true,
    environment: "node"
  }
});