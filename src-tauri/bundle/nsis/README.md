# NSIS bundle resources

This directory contains NSIS installer hooks and optional driver installers
bundled into the OxideLink setup executable.

## Driver installers

`HidHideInstaller.exe` and `ViGEmBusSetup.exe` are **0-byte placeholders**
committed to the repository so the NSIS installer script can reference them
without failing the build. The installer (`installer.nsh`) checks the file
size before executing and skips any 0-byte placeholder automatically.

### Replacing with real installers

Before publishing a public release, replace the placeholder files with the
real driver installers:

1. Download the latest **ViGEmBus** setup from
   <https://github.com/ViGEm/ViGEmBus/releases>.
2. Download the latest **HidHide** setup from
   <https://github.com/ViGEm/HidHide/releases>.
3. Replace the 0-byte files in this directory with the downloaded installers.
4. Do **not** commit the real installers to the repository — they are large
   binaries and should be injected at build time or stored in release
   artifacts only.

The `.gitignore` file excludes any non-placeholder (non-zero-byte) versions
of these files from being committed accidentally.

## installer.nsh

The NSIS hook script that adds optional HidHide and ViGEmBus component pages
to the Tauri-generated installer, installs the drivers when selected, and
creates fallback shortcuts for silent/passive installs.
