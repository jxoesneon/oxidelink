# Contributing to OxideLink

Thank you for your interest in contributing to OxideLink. This document covers
the development setup, code style, and pull request process.

## Development prerequisites

- **Windows 10/11** (the app uses Windows-specific HID, ViGEmBus, and SendInput APIs)
- **Rust** 1.75+ (stable toolchain)
- **Node.js** 22+ and npm
- **ViGEmBus** driver installed ([download](https://github.com/ViGEm/ViGEmBus))
- Optional: **HidHide** ([download](https://github.com/ViGEm/HidHide)) for controller hiding

## Getting started

```powershell
git clone https://github.com/jxoesneon/oxidelink.git
cd oxidelink
npm install
npm run tauri dev
```

The Vite dev server starts on `http://localhost:1420` and the Tauri window
opens automatically.

## Project layout

```
src-tauri/       Rust backend (Tauri commands, HID, ViGEmBus, DSU, macros)
src-frontend/    Vite frontend (vanilla JS, HTML, CSS)
e2e-tests/       WebdriverIO + Tauri end-to-end tests
docs/            Feature guides and technical documentation
scripts/         Build and code-signing PowerShell scripts
research-pocs/   Standalone research prototypes (not part of the shipped app)
assets/          Banners and screenshots for the README
```

## Code style

### Rust

- Run `cargo fmt` before committing.
- Run `cargo clippy --lib -- -D warnings` — zero warnings are tolerated.
- Use `snake_case` for functions and variables, `PascalCase` for types.
- Prefer `?` operator over `match` for error propagation where readable.
- Keep `unsafe` blocks minimal and documented.

### Frontend (JavaScript / HTML / CSS)

- Use ES modules (`import`/`export`).
- 2-space indentation, no trailing whitespace.
- Use semantic HTML elements (`<section>`, `<nav>`, `<button>`) over `<div>`
  where appropriate.
- Keep CSS in `styles.css`; avoid inline styles.

## Testing

```powershell
# Rust library tests (490 tests)
cd src-tauri
cargo clippy --lib -- -D warnings
cargo test --lib

# Frontend unit tests (Vitest)
cd ..
npm test

# Production build
npm run build
```

E2E tests require a release build and `tauri-driver` — see
`e2e-tests/README.md` for setup.

## Pull request process

1. **Fork** the repository and create a feature branch from `main`:
   ```bash
   git checkout -b feat/your-feature
   ```
2. **Write tests** for new functionality. Unit tests go in the same file under
   `#[cfg(test)]` (Rust) or `*.test.js` (frontend).
3. **Run the full test suite** locally before pushing.
4. **Use conventional commit messages:**
   - `feat:` new feature
   - `fix:` bug fix
   - `docs:` documentation only
   - `refactor:` code restructuring without behavior change
   - `test:` test additions or corrections
   - `chore:` tooling, dependencies, CI
5. **Open a pull request** with a clear description of what changed and why.
   Link any related issues.
6. Ensure all CI checks pass before requesting review.

## Reporting issues

- Use **GitHub Issues** for bug reports and feature requests.
- Include your Windows version, OxideLink version, and controller connection
  type (USB or Bluetooth).
- Attach relevant log output from the in-app log viewer if applicable.

## License

By contributing, you agree that your contributions are licensed under the MIT
License, the same license that covers the project.
