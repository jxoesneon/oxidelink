# OxideLink Windows Installer / Release Pipeline

This document covers the NSIS installer, code signing, and release build for OxideLink.

## NSIS installer

The installer is built by Tauri as an NSIS `*-setup.exe` (and an `.msi` target is also enabled). The custom NSIS hooks live in:

- `src-tauri/bundle/nsis/installer.nsh`
- `src-tauri/bundle/nsis/HidHideInstaller.exe` (placeholder)
- `src-tauri/bundle/nsis/ViGEmBusSetup.exe` (placeholder)

### Optional driver components

`installer.nsh` adds an **optional component page** with two sections:

- HidHide driver
- ViGEmBus driver

If the user selects a section, the bundled installer is executed with `ExecWait` after the OxideLink files are copied. The installers run with `/S` (silent) flags.

> **Note:** Because Tauri includes `installer.nsh` before its built-in page macros, the component page currently appears *before* the Welcome page. For the standard page order a custom `.nsi` template must be used.

### Replacing placeholder installers

The two `.exe` files are currently 0-byte placeholders so the build config can be wired up without requiring the real drivers.

1. Obtain the real `HidHideInstaller.exe` and `ViGEmBusSetup.exe`.
2. Copy them over the placeholders in `src-tauri/bundle/nsis/`.
3. Ensure `tauri.conf.json` has the resources mapping:

```json
"bundle": {
  "resources": {
    "bundle/nsis/HidHideInstaller.exe": "resources/drivers/HidHideInstaller.exe",
    "bundle/nsis/ViGEmBusSetup.exe": "resources/drivers/ViGEmBusSetup.exe"
  }
}
```

The NSIS script installs from `$INSTDIR\resources\drivers\`.

## Code signing

A self-signed PFX has been generated for local/development signing:

- File: `src-tauri/certs/oxidelink.pfx`
- Password: `OxideLink123!` (stored in `src-tauri/certs/oxidelink.pfx.txt`)
- Thumbprint: `A9EF703B7F22C0F4593BC4E1C74BC6EC58F298C3`

`tauri.conf.json` is configured with `bundle.windows.signCommand` so Tauri signs the main `.exe`, the NSIS setup `.exe`, and the `.msi` during build using `signtool.exe`.

The `scripts/build-release.ps1` pipeline reads the PFX password from the `OXIDELINK_PFX_PASSWORD` environment variable when it is set; otherwise it falls back to the password stored in `src-tauri/certs/oxidelink.pfx.txt` for local development signing.

### Trusting the self-signed certificate

End-users will see a Windows SmartScreen / Defender warning because the certificate is not issued by a public CA. To remove the warning on a single machine:

1. Double-click `src-tauri/certs/oxidelink.pfx` and import it into **Local Machine\Trusted Root Certification Authorities** (or **Current User\Trusted Root** for just the current user).
2. Alternatively run as admin:
   ```powershell
   Import-PfxCertificate -FilePath src-tauri\certs\oxidelink.pfx -CertStoreLocation Cert:\LocalMachine\Root -Password (ConvertTo-SecureString -String 'OxideLink123!' -AsPlainText -Force)
   ```

For public distribution, replace the PFX with an EV or OV code-signing certificate from a trusted CA and update `signCommand` accordingly.

## SmartScreen

Even after the certificate is trusted locally, SmartScreen may still warn for the first few runs because the file lacks reputation. To build reputation:

- Distribute the signed installer broadly.
- Submit the signed file to Microsoft Defender SmartScreen for review.
- Consider an EV certificate for immediate reputation.

## Driver install instructions

If the bundled driver installers were not included in the setup or the user skipped them, drivers can be installed manually:

- **HidHide**: download the latest release from <https://github.com/ViGEm/HidHide> and run `HidHideInstaller.exe`.
- **ViGEmBus**: download the latest release from <https://github.com/ViGEm/ViGEmBus> and run `ViGEmBusSetup.exe`.

Both usually require a reboot before OxideLink can use the virtual controller devices.

## Release build

Run the release pipeline with:

```powershell
$env:OXIDELINK_PFX_PASSWORD = "<PFX password>"
.\scripts\build-release.ps1
```

Set `OXIDELINK_PFX_PASSWORD` before running the release script so its signing step can access the PFX without embedding the password in the command.

This:

1. Runs `npm run tauri build` (produces `src-tauri/target/release/` and `src-tauri/target/release/bundle/`).
2. Calls `scripts/sign-tauri.ps1` for any `.exe` or `.msi` artifact that is not already signed.

To sign an arbitrary file manually:

```powershell
.\scripts\sign-tauri.ps1 -Path "path\to\file.exe"
```

## Updater endpoint

The Tauri updater `endpoints` field in `src-tauri/tauri.conf.json` is configured to:

```
https://github.com/oxidelink/oxidelink/releases/latest/download/latest.json
```

> **TODO:** Replace `oxidelink/oxidelink` with the actual GitHub owner/repo once the repository is published. The `latest.json` manifest is produced by `scripts/build-release.ps1` (or CI) alongside the installer artifacts and must be uploaded to the release.

## Blockers / caveats

- The bundled `HidHideInstaller.exe` and `ViGEmBusSetup.exe` are placeholders. Replace them with real installers before shipping.
- `signtool.exe` must be available (Windows SDK) or `sign-tauri.ps1` will fail to locate it.
- Self-signed certificates trigger SmartScreen warnings. Use a real CA-issued certificate for public releases.
- The updater endpoint URL uses a placeholder GitHub owner/repo (`oxidelink/oxidelink`). Update it before public release.
