import { defineConfig } from "vitest/config";

// OxideLink frontend test config — Vitest with jsdom for DOM-dependent code.
export default defineConfig({
  root: "src-frontend",
  test: {
    environment: "jsdom",
    globals: true,
    include: ["**/*.test.js", "**/*.spec.js"],
  },
});
