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

    // -------------------------------------------------------------------------
    //  uid_to_hex
    // -------------------------------------------------------------------------

    #[test]
    fn uid_to_hex_empty_is_empty_string() {
        assert_eq!(uid_to_hex(&[]), "");
    }

    #[test]
    fn uid_to_hex_single_byte() {
        assert_eq!(uid_to_hex(&[0x04]), "04");
    }

    #[test]
    fn uid_to_hex_seven_bytes() {
        assert_eq!(
            uid_to_hex(&[0x04, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]),
            "04:AA:BB:CC:DD:EE:FF"
        );
    }

    #[test]
    fn uid_to_hex_uppercase() {
        assert_eq!(uid_to_hex(&[0xab, 0xcd, 0xef]), "AB:CD:EF");
    }

    #[test]
    fn uid_to_hex_zero_bytes() {
        assert_eq!(uid_to_hex(&[0x00, 0x00]), "00:00");
    }

    // -------------------------------------------------------------------------
    //  parse_ntag215_header — 4-byte (non-NXP) UID
    // -------------------------------------------------------------------------

    fn powersaves_bin_4byte_uid() -> Vec<u8> {
        let mut data = vec![0u8; AMIIBO_BIN_SIZE_POWERSAVES];
        // First byte is NOT 0x04, so a 4-byte UID is assumed.
        data[0..4].copy_from_slice(&[0x12, 0x34, 0x56, 0x78]);
        data[10] = 0x0F;
        data[11] = 0xE0;
        data
    }

    #[test]
    fn parse_powersaves_header_4byte_uid() {
        let data = powersaves_bin_4byte_uid();
        let header = parse_ntag215_header(&data).expect("should parse 4-byte UID dump");
        assert!(!header.is_nxp);
        assert_eq!(header.uid_len, 4);
        assert_eq!(header.uid, vec![0x12, 0x34, 0x56, 0x78]);
        // BCC is 0 for 4-byte UIDs in this parser.
        assert_eq!(header.bcc, 0);
        assert!(header.is_powersaves);
        assert_eq!(header.size, AMIIBO_BIN_SIZE_POWERSAVES);
    }

    // -------------------------------------------------------------------------
    //  parse_ntag215_header — config bytes
    // -------------------------------------------------------------------------

    #[test]
    fn parse_powersaves_config_bytes() {
        let mut data = powersaves_bin();
        // Config bytes for 540-byte dump are at [532..540].
        data[532..540].copy_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x11, 0x22]);
        let header = parse_ntag215_header(&data).expect("should parse");
        assert_eq!(
            header.cfg,
            vec![0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x11, 0x22]
        );
        // No dynamic lock bytes in PowerSaves dumps.
        assert!(header.dynamic_lock.is_empty());
    }

    #[test]
    fn parse_raw_config_and_dynamic_lock() {
        let data = raw_ntag215_bin();
        let header = parse_ntag215_header(&data).expect("should parse raw dump");
        assert_eq!(
            header.dynamic_lock,
            vec![0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]
        );
        assert_eq!(
            header.cfg,
            vec![0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17]
        );
        assert_eq!(header.size, AMIIBO_BIN_SIZE_RAW);
    }

    // -------------------------------------------------------------------------
    //  parse_ntag215_header — internal / static lock bytes
    // -------------------------------------------------------------------------

    #[test]
    fn parse_internal_byte() {
        let mut data = powersaves_bin();
        // For 7-byte UID, internal byte is at index uid_len+1 = 8.
        data[8] = 0x48;
        let header = parse_ntag215_header(&data).expect("should parse");
        assert_eq!(header.internal, 0x48);
    }

    #[test]
    fn parse_static_lock_bytes() {
        let mut data = powersaves_bin();
        data[10] = 0x12;
        data[11] = 0x34;
        let header = parse_ntag215_header(&data).expect("should parse");
        assert_eq!(header.static_lock, [0x12, 0x34]);
    }

    #[test]
    fn parse_bcc_byte_for_7byte_uid() {
        let mut data = powersaves_bin();
        data[7] = 0x55; // BCC byte
        let header = parse_ntag215_header(&data).expect("should parse");
        assert_eq!(header.bcc, 0x55);
    }

    // -------------------------------------------------------------------------
    //  parse_ntag215_header — error cases
    // -------------------------------------------------------------------------

    #[test]
    fn reject_empty_dump() {
        assert!(parse_ntag215_header(&[]).is_err());
    }

    #[test]
    fn reject_too_short_dump() {
        assert!(parse_ntag215_header(&[0u8; 10]).is_err());
    }

    #[test]
    fn reject_oversized_dump() {
        assert!(parse_ntag215_header(&[0u8; 1000]).is_err());
    }

    #[test]
    fn reject_just_under_powersaves_size() {
        assert!(parse_ntag215_header(&[0u8; 539]).is_err());
    }

    #[test]
    fn reject_just_over_powersaves_under_raw() {
        assert!(parse_ntag215_header(&[0u8; 541]).is_err());
    }

    #[test]
    fn reject_just_under_raw_size() {
        assert!(parse_ntag215_header(&[0u8; 571]).is_err());
    }

    #[test]
    fn reject_just_over_raw_size() {
        assert!(parse_ntag215_header(&[0u8; 573]).is_err());
    }

    // -------------------------------------------------------------------------
    //  parse_amiibo_uid
    // -------------------------------------------------------------------------

    #[test]
    fn parse_amiibo_uid_powersaves() {
        let data = powersaves_bin();
        let uid = parse_amiibo_uid(&data).expect("should extract UID");
        assert_eq!(uid, vec![0x04, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66]);
    }

    #[test]
    fn parse_amiibo_uid_raw() {
        let data = raw_ntag215_bin();
        let uid = parse_amiibo_uid(&data).expect("should extract UID");
        assert_eq!(uid, vec![0x04, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
    }

    #[test]
    fn parse_amiibo_uid_4byte() {
        let data = powersaves_bin_4byte_uid();
        let uid = parse_amiibo_uid(&data).expect("should extract 4-byte UID");
        assert_eq!(uid, vec![0x12, 0x34, 0x56, 0x78]);
    }

    #[test]
    fn parse_amiibo_uid_rejects_invalid() {
        assert!(parse_amiibo_uid(&[0u8; 100]).is_err());
    }

    // -------------------------------------------------------------------------
    //  emulate_amiibo
    // -------------------------------------------------------------------------

    #[test]
    fn emulate_amiibo_raw_dump() {
        let data = raw_ntag215_bin();
        let state = emulate_amiibo(data).expect("should emulate raw dump");
        assert!(state.tag_present);
        assert_eq!(state.uid, Some("04:AA:BB:CC:DD:EE:FF".to_string()));
        assert!(state.amiibo_data.is_some());
        assert!(state.error.is_none());
    }

    #[test]
    fn emulate_amiibo_4byte_uid() {
        let data = powersaves_bin_4byte_uid();
        let state = emulate_amiibo(data).expect("should emulate 4-byte UID");
        assert!(state.tag_present);
        assert_eq!(state.uid, Some("12:34:56:78".to_string()));
    }

    #[test]
    fn emulate_amiibo_preserves_bytes() {
        let data = powersaves_bin();
        let state = emulate_amiibo(data.clone()).expect("should emulate");
        assert_eq!(state.amiibo_data, Some(data));
    }

    #[test]
    fn emulate_amiibo_empty_rejected() {
        assert!(emulate_amiibo(vec![]).is_err());
    }

    #[test]
    fn emulate_amiibo_default_mode_is_disabled() {
        let data = powersaves_bin();
        let state = emulate_amiibo(data).expect("should emulate");
        // NfcState default mode is Disabled.
        assert_eq!(state.mode, crate::subcmd::NfcMode::Disabled);
        assert!(!state.enabled);
    }

    // -------------------------------------------------------------------------
    //  Ntag215Header — Default, PartialEq, serialization
    // -------------------------------------------------------------------------

    #[test]
    fn ntag215_header_default() {
        let header = Ntag215Header::default();
        assert!(header.uid.is_empty());
        assert_eq!(header.uid_len, 0);
        assert_eq!(header.bcc, 0);
        assert_eq!(header.internal, 0);
        assert_eq!(header.static_lock, [0, 0]);
        assert!(header.dynamic_lock.is_empty());
        assert!(header.cfg.is_empty());
        assert_eq!(header.size, 0);
        assert!(!header.is_powersaves);
        assert!(!header.is_nxp);
    }

    #[test]
    fn ntag215_header_equality() {
        let data = powersaves_bin();
        let h1 = parse_ntag215_header(&data).unwrap();
        let h2 = parse_ntag215_header(&data).unwrap();
        assert_eq!(h1, h2);
    }

    #[test]
    fn ntag215_header_inequality_different_uid() {
        let data1 = powersaves_bin();
        let mut data2 = powersaves_bin();
        data2[1] = 0xFF; // different UID byte
        let h1 = parse_ntag215_header(&data1).unwrap();
        let h2 = parse_ntag215_header(&data2).unwrap();
        assert_ne!(h1, h2);
    }

    #[test]
    fn ntag215_header_clone_is_equal() {
        let data = powersaves_bin();
        let header = parse_ntag215_header(&data).unwrap();
        let cloned = header.clone();
        assert_eq!(header, cloned);
    }

    #[test]
    fn ntag215_header_serde_roundtrip() {
        let data = powersaves_bin();
        let header = parse_ntag215_header(&data).unwrap();
        let json = serde_json::to_string(&header).expect("serialize");
        let deserialized: Ntag215Header =
            serde_json::from_str(&json).expect("deserialize");
        assert_eq!(header, deserialized);
    }

    #[test]
    fn ntag215_header_serde_snake_case_fields() {
        let header = Ntag215Header {
            uid: vec![0x04],
            uid_len: 1,
            bcc: 0x39,
            internal: 0x48,
            static_lock: [0x0F, 0xE0],
            dynamic_lock: vec![0x01],
            cfg: vec![0x10],
            size: 540,
            is_powersaves: true,
            is_nxp: true,
        };
        let json = serde_json::to_string(&header).expect("serialize");
        assert!(json.contains("\"uid_len\""), "snake_case uid_len: {json}");
        assert!(
            json.contains("\"is_powersaves\""),
            "snake_case is_powersaves: {json}"
        );
        assert!(json.contains("\"is_nxp\""), "snake_case is_nxp: {json}");
    }

    // -------------------------------------------------------------------------
    //  apply_nfc_report — state mutation and preservation
    // -------------------------------------------------------------------------

    #[test]
    fn apply_nfc_report_preserves_enabled_and_mode() {
        let mut state = NfcState::default();
        state.enabled = true;
        state.mode = crate::subcmd::NfcMode::Nfc;

        // Apply a report with no tag (wrong report ID).
        let data = vec![0u8; 65];
        apply_nfc_report(&mut state, &data);

        assert!(state.enabled, "enabled preserved");
        assert_eq!(state.mode, crate::subcmd::NfcMode::Nfc, "mode preserved");
        assert!(!state.tag_present, "tag_present cleared");
    }

    #[test]
    fn apply_nfc_report_increments_scan_count() {
        let mut state = NfcState::default();

        let mut data = vec![0u8; 65];
        data[0] = crate::hid_parser::REPORT_ID_NFC_IR;
        data[1] = 0x42;
        data[2] = 0x80;
        data[49] = 0x01;
        data[50..57].copy_from_slice(&[0x04, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06]);
        data[57] = 0x02;

        apply_nfc_report(&mut state, &data);
        assert_eq!(state.scan_count, 1, "first scan");

        apply_nfc_report(&mut state, &data);
        assert_eq!(state.scan_count, 2, "second scan");
    }

    #[test]
    fn apply_nfc_report_clears_tag_on_no_tag() {
        let mut state = NfcState::default();
        state.tag_present = true;
        state.uid = Some("04:01:02:03:04:05:06".to_string());

        let data = vec![0u8; 65];
        apply_nfc_report(&mut state, &data);

        assert!(!state.tag_present, "tag_present cleared");
        assert!(state.uid.is_none(), "uid cleared");
    }

    #[test]
    fn apply_nfc_report_empty_data_clears_tag() {
        let mut state = NfcState::default();
        state.tag_present = true;

        apply_nfc_report(&mut state, &[]);

        assert!(!state.tag_present, "empty data clears tag");
        assert!(state.uid.is_none(), "empty data clears uid");
    }

    #[test]
    fn apply_nfc_report_sets_last_tag() {
        let mut state = NfcState::default();

        let mut data = vec![0u8; 65];
        data[0] = crate::hid_parser::REPORT_ID_NFC_IR;
        data[1] = 0x42;
        data[2] = 0x80;
        data[49] = 0x01;
        data[50..57].copy_from_slice(&[0x04, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06]);
        data[57] = 0x02;

        apply_nfc_report(&mut state, &data);

        assert!(state.last_tag.is_some(), "last_tag set");
        let tag = state.last_tag.as_ref().unwrap();
        assert_eq!(tag.uid, vec![0x04, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06]);
        assert_eq!(tag.tag_type, 0x02);
        assert!(tag.is_amiibo, "should be detected as amiibo");
    }

    #[test]
    fn apply_nfc_report_clears_error_on_tag() {
        let mut state = NfcState::default();
        state.error = Some("previous error".to_string());

        let mut data = vec![0u8; 65];
        data[0] = crate::hid_parser::REPORT_ID_NFC_IR;
        data[1] = 0x42;
        data[2] = 0x80;
        data[49] = 0x01;
        data[50..57].copy_from_slice(&[0x04, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06]);
        data[57] = 0x02;

        apply_nfc_report(&mut state, &data);

        assert!(state.error.is_none(), "error cleared on tag read");
    }

    #[test]
    fn apply_nfc_report_clears_error_on_no_tag() {
        let mut state = NfcState::default();
        state.error = Some("previous error".to_string());

        let data = vec![0u8; 65];
        apply_nfc_report(&mut state, &data);

        assert!(state.error.is_none(), "error cleared on no tag");
    }

    // -------------------------------------------------------------------------
    //  on_nfc_report — additional edge cases
    // -------------------------------------------------------------------------

    #[test]
    fn on_nfc_report_empty_data() {
        let state = on_nfc_report(&[]);
        assert!(!state.tag_present);
        assert!(state.uid.is_none());
        assert_eq!(state.scan_count, 0);
    }

    #[test]
    fn on_nfc_report_short_data() {
        let state = on_nfc_report(&[0x31]);
        assert!(!state.tag_present);
        assert!(state.uid.is_none());
    }

    #[test]
    fn on_nfc_report_default_state() {
        let state = on_nfc_report(&[0u8; 65]);
        assert_eq!(state.mode, crate::subcmd::NfcMode::Disabled);
        assert!(!state.enabled);
        assert_eq!(state.scan_count, 0);
    }

    // -------------------------------------------------------------------------
    //  NfcState serialization
    // -------------------------------------------------------------------------

    #[test]
    fn nfc_state_serde_roundtrip() {
        let state = NfcState {
            mode: crate::subcmd::NfcMode::Nfc,
            enabled: true,
            tag_present: true,
            uid: Some("04:11:22:33:44:55:66".to_string()),
            scan_count: 5,
            ..Default::default()
        };
        let json = serde_json::to_string(&state).expect("serialize");
        let deserialized: NfcState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(deserialized.mode, state.mode);
        assert_eq!(deserialized.enabled, state.enabled);
        assert_eq!(deserialized.tag_present, state.tag_present);
        assert_eq!(deserialized.uid, state.uid);
        assert_eq!(deserialized.scan_count, state.scan_count);
    }

    #[test]
    fn nfc_state_serde_snake_case_fields() {
        let state = NfcState {
            tag_present: true,
            amiibo_data: Some(vec![0x04]),
            scan_count: 1,
            ..Default::default()
        };
        let json = serde_json::to_string(&state).expect("serialize");
        assert!(
            json.contains("\"tag_present\""),
            "snake_case tag_present: {json}"
        );
        assert!(
            json.contains("\"amiibo_data\""),
            "snake_case amiibo_data: {json}"
        );
        assert!(
            json.contains("\"scan_count\""),
            "snake_case scan_count: {json}"
        );
    }

    #[test]
    fn nfc_state_default_is_clean() {
        let state = NfcState::default();
        assert!(!state.enabled);
        assert!(!state.tag_present);
        assert!(state.uid.is_none());
        assert!(state.amiibo_data.is_none());
        assert!(state.error.is_none());
        assert_eq!(state.scan_count, 0);
        assert_eq!(state.mode, crate::subcmd::NfcMode::Disabled);
    }

    // -------------------------------------------------------------------------
    //  Constants
    // -------------------------------------------------------------------------

    #[test]
    fn amiibo_bin_size_constants() {
        assert_eq!(AMIIBO_BIN_SIZE_POWERSAVES, 540);
        assert_eq!(AMIIBO_BIN_SIZE_RAW, 572);
        assert_ne!(AMIIBO_BIN_SIZE_POWERSAVES, AMIIBO_BIN_SIZE_RAW);
    }

    // -------------------------------------------------------------------------
    //  validate_path_within_base — pure logic (relative path rejection)
    // -------------------------------------------------------------------------

    #[test]
    fn validate_path_rejects_relative_path() {
        let base = std::env::temp_dir().join("oxidlink_nfc_test_relative");
        let result = validate_path_within_base("relative/path.bin", &base);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("must be absolute"),
            "error should mention absolute: {err}"
        );
    }

    #[test]
    fn validate_path_rejects_nonexistent_absolute_path() {
        let base = std::env::temp_dir().join("oxidlink_nfc_test_nonexist");
        let bogus = base.join("does_not_exist.bin");
        let result = validate_path_within_base(
            bogus.to_string_lossy().as_ref(),
            &base,
        );
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("failed to resolve"),
            "error should mention resolve: {err}"
        );
    }

    // -------------------------------------------------------------------------
    //  Tag type detection via emulate_amiibo
    // -------------------------------------------------------------------------

    #[test]
    fn emulate_amiibo_nxp_tag_detected() {
        let data = powersaves_bin();
        let state = emulate_amiibo(data).expect("should emulate");
        // NXP tags start with 0x04.
        assert!(state.uid.as_deref().unwrap_or("").starts_with("04"));
    }

    #[test]
    fn emulate_amiibo_non_nxp_tag_detected() {
        let data = powersaves_bin_4byte_uid();
        let state = emulate_amiibo(data).expect("should emulate");
        // Non-NXP tags do not start with 0x04.
        assert!(!state.uid.as_deref().unwrap_or("").starts_with("04"));
    }
}
