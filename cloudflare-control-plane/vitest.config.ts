import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    environment: "node",
    include: ["test/**/*.test.ts"]
  },
  resolve: {
    alias: {
      // Stub the Workerd-only `cloudflare:workers` module so unit tests can
      // import Workflow classes without a real Workers runtime. The stub
      // mirrors only the shapes our code references.
      "cloudflare:workers": new URL(
        "./test/workflows/cloudflare-workers-stub.ts",
        import.meta.url
      ).pathname
    }
  }
});
