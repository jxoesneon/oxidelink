//! NFC/amiibo read and emulation helpers (Wave 4).

use crate::hid_parser;
use crate::state::{AppCtx, NfcState};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use tauri::State;

/// Base directory for user-supplied amiibo dumps.
fn amiibo_base_dir() -> PathBuf {
    dirs_next::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("OxideLink")
        .join("amiibo")
}

/// Ensure `path` resolves to a location inside `base`, rejecting traversal
/// attempts and symlinks that escape the base directory.
fn validate_path_within_base(path: &str, base: &Path) -> Result<PathBuf, String> {
    let p = Path::new(path);
    if !p.is_absolute() {
        return Err(format!("path '{}' must be absolute", path));
    }
    let canonical = p
        .canonicalize()
        .map_err(|e| format!("failed to resolve path '{}': {}", path, e))?;
    std::fs::create_dir_all(base)
        .map_err(|e| format!("failed to create base dir '{}': {}", base.display(), e))?;
    let canonical_base = base
        .canonicalize()
        .map_err(|e| format!("failed to resolve base dir '{}': {}", base.display(), e))?;
    if !canonical.starts_with(&canonical_base) {
        return Err(format!(
            "path '{}' is outside the allowed directory '{}'",
            path,
            base.display()
        ));
    }
    Ok(canonical)
}

/// PowerSaves-style amiibo .bin dump size (135 NTAG215 pages).
pub const AMIIBO_BIN_SIZE_POWERSAVES: usize = 540;
/// Raw, full NTAG215 dump size (143 pages).
pub const AMIIBO_BIN_SIZE_RAW: usize = 572;

/// Parsed NTAG215 / amiibo dump header.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Ntag215Header {
    /// UID in raw bytes (4 or 7 bytes depending on manufacturer prefix).
    pub uid: Vec<u8>,
    /// Number of UID bytes detected (4 or 7).
    pub uid_len: usize,
    /// Block count check byte (byte 7 for 7-byte UIDs).
    pub bcc: u8,
    /// Internal byte (byte 8).
    pub internal: u8,
    /// Static lock bytes (bytes 10-11 of page 2).
    pub static_lock: [u8; 2],
    /// Dynamic lock bytes (raw 572-byte dumps only).
    pub dynamic_lock: Vec<u8>,
    /// Configuration / password bytes at the end of the dump.
    pub cfg: Vec<u8>,
    /// Total file size in bytes.
    pub size: usize,
    /// `true` for the 540-byte PowerSaves-style trimmed dump.
    pub is_powersaves: bool,
    /// `true` when the first byte is the NXP manufacturer ID (0x04).
    pub is_nxp: bool,
}

/// Convert a UID byte slice to a colon-separated hex string.
pub fn uid_to_hex(uid: &[u8]) -> String {
    uid.iter()
        .map(|b| format!("{:02X}", b))
        .collect::<Vec<_>>()
        .join(":")
}

/// Parse the header of an NTAG215 / amiibo .bin dump.
///
/// Supports the two common file sizes:
/// * 540 bytes – PowerSaves-style trimmed dump.
/// * 572 bytes – raw full NTAG215 dump.
///
/// The UID starts at offset 0. If the manufacturer byte is `0x04` (NXP) a
/// 7-byte UID is assumed, otherwise a 4-byte UID is returned.
pub fn parse_ntag215_header(data: &[u8]) -> Result<Ntag215Header, String> {
    let size = data.len();
    if size != AMIIBO_BIN_SIZE_POWERSAVES && size != AMIIBO_BIN_SIZE_RAW {
        return Err(format!(
            "Unsupported amiibo/NTAG215 dump size: {} bytes (expected {} or {})",
            size, AMIIBO_BIN_SIZE_POWERSAVES, AMIIBO_BIN_SIZE_RAW
        ));
    }
    if size < 12 {
        return Err("NTAG215 dump too short to contain a header".into());
    }

    let is_powersaves = size == AMIIBO_BIN_SIZE_POWERSAVES;
    let is_nxp = data[0] == 0x04;
    let uid_len = if is_nxp { 7 } else { 4 };
    if !matches!(uid_len, 4 | 7) {
        return Err(format!("Unsupported NTAG215 UID length: {}", uid_len));
    }

    if size < uid_len {
        return Err("NTAG215 dump too short for UID".into());
    }
    let uid = data[0..uid_len].to_vec();

    let bcc = if uid_len == 7 && size > uid_len {
        data[uid_len]
    } else {
        0
    };

    let internal = data.get(uid_len + 1).copied().unwrap_or(0);
    let static_lock = if size >= 12 {
        [data[10], data[11]]
    } else {
        [0, 0]
    };

    let (dynamic_lock, cfg) = if size == AMIIBO_BIN_SIZE_RAW {
        let dynamic_lock = if size >= 528 {
            data[520..528].to_vec()
        } else {
            Vec::new()
        };
        let cfg = if size >= 536 {
            data[528..536].to_vec()
        } else {
            Vec::new()
        };
        (dynamic_lock, cfg)
    } else {
        let cfg = if size >= 540 {
            data[532..540].to_vec()
        } else {
            Vec::new()
        };
        (Vec::new(), cfg)
    };

    Ok(Ntag215Header {
        uid,
        uid_len,
        bcc,
        internal,
        static_lock,
        dynamic_lock,
        cfg,
        size,
        is_powersaves,
        is_nxp,
    })
}

/// Extract just the UID from an amiibo / NTAG215 dump.
pub fn parse_amiibo_uid(data: &[u8]) -> Result<Vec<u8>, String> {
    Ok(parse_ntag215_header(data)?.uid)
}

/// Validate that `bytes` are a supported amiibo/NTAG215 dump and build an
/// `NfcState` ready for emulation.
pub fn emulate_amiibo(bytes: Vec<u8>) -> Result<NfcState, String> {
    let header = parse_ntag215_header(&bytes)?;
    let uid_hex = uid_to_hex(&header.uid);

    Ok(NfcState {
        tag_present: true,
        uid: Some(uid_hex),
        amiibo_data: Some(bytes),
        error: None,
        ..Default::default()
    })
}

/// Update an existing `NfcState` from a raw 0x31 NFC/IR input report.
///
/// This preserves existing fields such as `enabled` and `mode` while
/// updating tag presence and UID.
pub fn apply_nfc_report(state: &mut NfcState, data: &[u8]) {
    if let Some(parsed) = hid_parser::parse_nfc_ir_report(data) {
        if let Some(tag) = parsed.nfc_tag {
            state.tag_present = true;
            state.uid = Some(uid_to_hex(&tag.uid));
            state.last_tag = Some(tag);
            state.scan_count = state.scan_count.wrapping_add(1);
            state.error = None;
            return;
        }
    }

    state.tag_present = false;
    state.uid = None;
    state.error = None;
}

/// Parse a raw 0x31 NFC/IR input report and return a fresh `NfcState`.
pub fn on_nfc_report(data: &[u8]) -> NfcState {
    let mut state = NfcState::default();
    apply_nfc_report(&mut state, data);
    state
}

// ===========================================================================
//  Tauri commands
// ===========================================================================

/// Return the current NFC runtime state for slot 0.
// Not registered as a Tauri command here because main.rs already exposes
// `get_nfc_state`; keeping this helper avoids a macro-name collision.
pub fn get_nfc_state(ctx: State<'_, AppCtx>) -> NfcState {
    ctx.shared.active_controller().nfc.clone()
}

/// Enable or disable NFC/amiibo support.
#[tauri::command]
pub fn set_nfc_enabled(ctx: State<'_, AppCtx>, enabled: bool) -> Result<NfcState, String> {
    {
        let mut cfg = ctx.shared.config.write();
        cfg.nfc.enabled = enabled;
    }
    let mut ctrl = ctx.shared.active_controller_mut();
    ctrl.nfc.enabled = enabled;
    Ok(ctrl.nfc.clone())
}

/// Load an amiibo .bin from disk, validate it, and store it for emulation.
#[tauri::command]
pub fn load_amiibo_bin(ctx: State<'_, AppCtx>, path: String) -> Result<NfcState, String> {
    let base = amiibo_base_dir();
    let canonical = validate_path_within_base(&path, &base)?;
    let bytes =
        fs::read(&canonical).map_err(|e| format!("Failed to read amiibo bin '{}': {}", path, e))?;
    let mut state = emulate_amiibo(bytes)?;
    state.enabled = true;

    {
        let mut cfg = ctx.shared.config.write();
        cfg.nfc.enabled = true;
        cfg.nfc.emulate_bin = Some(canonical.to_string_lossy().to_string());
        cfg.nfc.last_uid = state.uid.clone();
    }

    let mut ctrl = ctx.shared.active_controller_mut();
    ctrl.nfc = state.clone();
    Ok(state)
}

/// Read and validate an amiibo .bin from disk without updating config state.
#[tauri::command]
pub fn emulate_amiibo_from_path(path: String) -> Result<NfcState, String> {
    let base = amiibo_base_dir();
    let canonical = validate_path_within_base(&path, &base)?;
    let bytes =
        fs::read(&canonical).map_err(|e| format!("Failed to read amiibo bin '{}': {}", path, e))?;
    emulate_amiibo(bytes)
}

// ===========================================================================
//  Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn powersaves_bin() -> Vec<u8> {
        let mut data = vec![0u8; AMIIBO_BIN_SIZE_POWERSAVES];
        data[0..7].copy_from_slice(&[0x04, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66]);
        // Static lock bytes
        data[10] = 0x0F;
        data[11] = 0xE0;
        data
    }

    fn raw_ntag215_bin() -> Vec<u8> {
        let mut data = vec![0u8; AMIIBO_BIN_SIZE_RAW];
        data[0..7].copy_from_slice(&[0x04, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
        data[7] = 0x39; // BCC
        data[10] = 0x0F;
        data[11] = 0xE0;
        // Dynamic lock bytes (raw dump only)
        data[520..528].copy_from_slice(&[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]);
        // Config pages
        data[528..536].copy_from_slice(&[0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17]);
        data
    }

    #[test]
    fn parse_powersaves_header() {
        let data = powersaves_bin();
        let header = parse_ntag215_header(&data).expect("should parse 540-byte dump");
        assert!(header.is_powersaves);
        assert!(header.is_nxp);
        assert_eq!(header.uid_len, 7);
        assert_eq!(header.uid, vec![0x04, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66]);
        assert_eq!(header.static_lock, [0x0F, 0xE0]);
    }

    #[test]
    fn parse_raw_ntag215_header() {
        let data = raw_ntag215_bin();
        let header = parse_ntag215_header(&data).expect("should parse 572-byte dump");
        assert!(!header.is_powersaves);
        assert!(header.is_nxp);
        assert_eq!(header.uid_len, 7);
        assert_eq!(header.uid, vec![0x04, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
        assert_eq!(header.bcc, 0x39);
        assert_eq!(
            header.dynamic_lock,
            vec![0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]
        );
        assert_eq!(
            header.cfg,
            vec![0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17]
        );
    }

    #[test]
    fn reject_bad_amiibo_size() {
        assert!(parse_ntag215_header(&[0u8; 100]).is_err());
    }

    #[test]
    fn uid_to_hex_colon_format() {
        assert_eq!(uid_to_hex(&[0x04, 0x01, 0xAB]), "04:01:AB");
    }

    #[test]
    fn emulate_amiibo_valid() {
        let data = powersaves_bin();
        let state = emulate_amiibo(data).expect("should emulate");
        assert!(state.tag_present);
        assert_eq!(state.uid, Some("04:11:22:33:44:55:66".to_string()));
        assert!(state.amiibo_data.is_some());
        assert!(state.error.is_none());
    }

    #[test]
    fn emulate_amiibo_rejects_invalid_size() {
        let result = emulate_amiibo(vec![0u8; 100]);
        assert!(result.is_err());
    }

    #[test]
    fn on_nfc_report_extracts_uid() {
        let mut data = vec![0u8; 65];
        data[0] = crate::hid_parser::REPORT_ID_NFC_IR;
        data[1] = 0x42; // timer
        data[2] = 0x80; // battery full
                        // NFC payload at byte 49+
        data[49] = 0x01; // tag present
        data[50..57].copy_from_slice(&[0x04, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06]);
        data[57] = 0x02; // amiibo tag type

        let state = on_nfc_report(&data);
        assert!(state.tag_present);
        assert_eq!(state.uid, Some("04:01:02:03:04:05:06".to_string()));
    }

    #[test]
    fn on_nfc_report_no_tag() {
        let mut data = vec![0u8; 65];
        data[0] = crate::hid_parser::REPORT_ID_NFC_IR;
        data[1] = 0x42;
        data[2] = 0x80;
        // data[49] is 0 -> no tag present

        let state = on_nfc_report(&data);
        assert!(!state.tag_present);
        assert!(state.uid.is_none());
    }

    #[test]
    fn on_nfc_report_wrong_id() {
        let data = vec![0u8; 65];
        // data[0] is 0x00, not 0x31
        let state = on_nfc_report(&data);
        assert!(!state.tag_present);
        assert!(state.uid.is_none());
    }
}
