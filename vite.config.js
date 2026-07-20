import { defineConfig } from "vite";

// OxideLink frontend — Vite dev server on :1420 to match tauri.conf.json devUrl.
export default defineConfig({
  root: "src-frontend",
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
  },
  envPrefix: ["VITE_", "TAURI_"],
  build: {
    outDir: "../dist",
    emptyOutDir: true,
    target: "es2021",
  },
});
