# OxideLink E2E Tests (WebdriverIO + Tauri)

End-to-end tests for the OxideLink Tauri app using [WebdriverIO](https://webdriver.io/) and [`@wdio/tauri-service`](https://github.com/webdriverio/desktop-mobile/tree/main/packages/tauri-service).

## Prerequisites

1. **Rust toolchain** — required to build the app and `tauri-driver`.
2. **Tauri CLI** — `npm install -g @tauri-apps/cli` (already a dev dependency in the project root).
3. **Build the app** — from the project root:
   ```powershell
   npm run tauri build
   ```
   This produces `src-tauri/target/release/oxidelink.exe`.
4. **Install `tauri-driver`** (only needed for the `external` driver provider):
   ```powershell
   cargo install tauri-driver
   ```
   The `@wdio/tauri-service` can also auto-install it (`autoInstallTauriDriver: true` in `wdio.conf.js`).
5. **Edge WebDriver** — on Windows, the service auto-downloads the matching Edge WebDriver (`autoDownloadEdgeDriver: true`).

## Install E2E dependencies

```powershell
cd e2e-tests
npm install
```

## Run tests

```powershell
npm test
```

For headed mode (visible window):

```powershell
npm run test:headed
```

## Test structure

```
e2e-tests/
├── package.json       # E2E-specific dependencies
├── wdio.conf.js       # WebdriverIO + Tauri service config
├── README.md          # This file
└── test/
    └── specs/
        └── app.launch.js  # Smoke tests: window title, connection chip, battery panel
```

## Adding tests

Create new `.js` files under `test/specs/`. Use standard WebdriverIO commands (`$`, `$$`, `browser.*`) to interact with the app's DOM. For Tauri-specific operations (invoke commands, IPC mocking), see the [`@wdio/tauri-service` docs](https://github.com/webdriverio/desktop-mobile/tree/main/packages/tauri-service/docs).

## Driver providers

The config uses `driverProvider: "external"`, which requires `tauri-driver` (cargo-installed). Alternatives:

- **`embedded`** — no external driver; requires adding `tauri-plugin-wdio-webdriver` to the Rust app.
- **`crabnebula`** — cross-platform; requires `@crabnebula/tauri-driver` and a CN API key (macOS only).

## CI integration

Add to `.github/workflows/test.yml` after the Rust build step:

```yaml
- name: Install tauri-driver
  run: cargo install tauri-driver

- name: Run E2E tests
  working-directory: e2e-tests
  run: |
    npm ci
    npm test
```
