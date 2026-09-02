# Security Policy

## Supported versions

OxideLink is in active development. Security fixes are applied to the latest
release only.

| Version | Supported |
|---------|-----------|
| latest  | Yes       |
| older   | No        |

## Reporting a vulnerability

If you discover a security vulnerability in OxideLink, please report it
responsibly:

1. **Do not open a public GitHub issue.**
2. Email **security@oxidelink.dev** with a description of the issue, steps to
   reproduce, and the potential impact.
3. Include the OxideLink version and Windows version if applicable.

You will receive an acknowledgment within 48 hours. If the vulnerability is
confirmed, a fix will be prepared and a GitHub Security Advisory may be
published alongside the patch release.

## Scope

- OxideLink application code (Rust backend in `src-tauri/`, frontend in
  `src-frontend/`).
- The NSIS installer and code-signing pipeline.
- Driver integration (ViGEmBus, HidHide) as it relates to OxideLink's use of
  those libraries.

Out of scope:

- Vulnerabilities in third-party drivers (ViGEmBus, HidHide) themselves —
  report those upstream.
- Vulnerabilities in the Rust standard library or Tauri framework — report
  those to the respective maintainers.

## Disclosure

We follow coordinated disclosure. Once a fix is released, we will credit the
reporter in the release notes unless they prefer to remain anonymous.
