import path from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));

// WebdriverIO config for OxideLink E2E tests via @wdio/tauri-service.
//
// Prerequisites:
//   1. Build the app:  cd .. && npm run tauri build  (or cargo build --release in src-tauri)
//   2. Install tauri-driver:  cargo install tauri-driver
//   3. On Windows, the service auto-manages the Edge WebDriver.
//
// Run:  npm test
export const config = {
  runner: "local",
  specs: ["./test/specs/**/*.js"],
  maxInstances: 1,
  capabilities: [
    {
      browserName: "tauri",
      "tauri:options": {
        // Path to the built OxideLink binary (relative to project root).
        application: path.resolve(__dirname, "../src-tauri/target/release/oxidelink.exe"),
        driverProvider: "external",
        autoInstallTauriDriver: true,
        autoDownloadEdgeDriver: true,
      },
    },
  ],
  logLevel: "info",
  waitforTimeout: 10000,
  connectionRetryTimeout: 120000,
  connectionRetryCount: 3,
  services: [
    [
      "@wdio/tauri-service",
      {
        driverProvider: "external",
        autoInstallTauriDriver: true,
        autoDownloadEdgeDriver: true,
      },
    ],
  ],
  framework: "mocha",
  reporters: ["spec"],
  mochaOpts: {
    ui: "bdd",
    timeout: 60000,
  },
};
