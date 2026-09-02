#!/usr/bin/env bash
# Retrieves the Tauri updater signing key from macOS Keychain.
# Usage:
#   source scripts/updater-key.sh   # exports TAURI_SIGNING_PRIVATE_KEY
#   ./scripts/updater-key.sh        # prints the key to stdout
#
# The key is stored under the "oxidelink-tauri-updater-key" keychain item.
# To re-add after a fresh machine setup:
#   security add-generic-password -a "$USER" -s "oxidelink-tauri-updater-key" \
#     -w "<key contents>" -U
set -euo pipefail

KEY="$(security find-generic-password -a "$USER" -s "oxidelink-tauri-updater-key" -w 2>/dev/null || true)"

if [ -z "$KEY" ]; then
  echo "error: Tauri updater key not found in Keychain." >&2
  echo "       Add it with:" >&2
  echo "       security add-generic-password -a \"\$USER\" -s \"oxidelink-tauri-updater-key\" -w \"<key>\" -U" >&2
  exit 1
fi

if [ "${BASH_SOURCE[0]}" = "${0}" ]; then
  # Script is being executed directly — print the key.
  printf '%s\n' "$KEY"
else
  # Script is being sourced — export it.
  export TAURI_SIGNING_PRIVATE_KEY="$KEY"
  export TAURI_SIGNING_PRIVATE_KEY_PASSWORD=""
  echo "TAURI_SIGNING_PRIVATE_KEY set from Keychain." >&2
fi
