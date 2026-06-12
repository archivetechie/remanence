use super::*;

#[test]
fn check_condition_is_not_completion_unknown() {
    let e = TapeIoError::CheckCondition(ScsiError::CheckCondition {
        sense: Vec::new(),
        bytes_transferred: 0,
    });
    assert!(!e.is_completion_unknown());
}

#[test]
fn transport_is_completion_unknown() {
    let e = TapeIoError::Transport(ScsiError::TransportError {
        status: 0,
        host_status: 0,
        driver_status: 0x06,
        info: 1,
        sense: Vec::new(),
    });
    assert!(e.is_completion_unknown());
}

#[cfg(target_os = "linux")]
#[test]
fn unexpected_status_is_not_completion_unknown() {
    let e = TapeIoError::UnexpectedStatus(ScsiError::UnexpectedStatus { status: 0x08 });
    assert!(!e.is_completion_unknown());
}

#[test]
fn no_medium_is_not_completion_unknown() {
    let e = TapeIoError::NoMedium;
    assert!(!e.is_completion_unknown());
}

#[test]
fn write_protected_is_not_completion_unknown() {
    let e = TapeIoError::WriteProtected;
    assert!(!e.is_completion_unknown());
}

#[test]
fn block_too_large_carries_drive_reported_limit() {
    let e = TapeIoError::BlockTooLarge {
        requested: 32 * 1024 * 1024,
        limit: 16 * 1024 * 1024,
    };
    // Display includes both values for operator legibility.
    let s = e.to_string();
    assert!(s.contains("33554432"), "requested in display: {s}");
    assert!(s.contains("16777216"), "limit in display: {s}");
}

#[test]
fn read_buffer_too_small_carries_both_sizes() {
    let e = TapeIoError::ReadBufferTooSmall {
        actual: 1_048_576,
        provided: 65_536,
    };
    let s = e.to_string();
    assert!(s.contains("1048576"));
    assert!(s.contains("65536"));
}

#[test]
fn operation_failed_preserves_context_without_dirty_signal() {
    let e = TapeIoError::OperationFailed(
        "RawTapeSink operation failed: resume append error: catalog callback failed".into(),
    );
    assert!(!e.is_completion_unknown());
    let s = e.to_string();
    assert!(s.contains("RawTapeSink operation failed"));
    assert!(s.contains("catalog callback failed"));
}

// -- helper tests (Step 9.4) -----------------------------------

fn fixed_sense(key: u8, asc: u8, ascq: u8) -> Vec<u8> {
    // Fixed-format sense (SPC-5 §4.5.3, response code 0x70). 32
    // bytes total; only the fields we read are non-zero.
    let mut v = vec![0u8; 32];
    v[0] = 0x70;
    v[2] = key & 0x0F;
    v[7] = 24; // additional sense length
    v[12] = asc;
    v[13] = ascq;
    v
}

#[test]
fn map_scsi_check_condition_default_path() {
    let e = ScsiError::CheckCondition {
        sense: fixed_sense(0x05, 0x24, 0x00), // INVALID FIELD IN CDB
        bytes_transferred: 0,
    };
    assert!(matches!(map_scsi(e), TapeIoError::CheckCondition(_)));
}

#[test]
fn map_scsi_no_medium() {
    let e = ScsiError::CheckCondition {
        sense: fixed_sense(0x02, 0x3A, 0x00), // NOT READY / MEDIUM NOT PRESENT
        bytes_transferred: 0,
    };
    assert!(matches!(map_scsi(e), TapeIoError::NoMedium));
}

#[test]
fn map_scsi_write_protected() {
    let e = ScsiError::CheckCondition {
        sense: fixed_sense(0x07, 0x27, 0x00), // DATA PROTECT / WRITE PROTECTED
        bytes_transferred: 0,
    };
    assert!(matches!(map_scsi(e), TapeIoError::WriteProtected));
}

#[test]
fn map_scsi_data_protect_other_asc() {
    let e = ScsiError::CheckCondition {
        sense: fixed_sense(0x07, 0x74, 0x05), // DATA PROTECT / other (encryption mismatch)
        bytes_transferred: 0,
    };
    assert!(matches!(map_scsi(e), TapeIoError::DataProtect(_)));
}

#[test]
fn map_scsi_transport_error_is_completion_unknown() {
    let e = ScsiError::TransportError {
        status: 0,
        host_status: 0,
        driver_status: 0x06,
        info: 1,
        sense: Vec::new(),
    };
    let mapped = map_scsi(e);
    assert!(matches!(mapped, TapeIoError::Transport(_)));
    assert!(mapped.is_completion_unknown());
}

#[cfg(target_os = "linux")]
#[test]
fn map_scsi_unexpected_status_is_not_completion_unknown() {
    let mapped = map_scsi(ScsiError::UnexpectedStatus { status: 0x18 });
    assert!(matches!(mapped, TapeIoError::UnexpectedStatus(_)));
    assert!(!mapped.is_completion_unknown());
}

#[test]
fn map_scsi_invalid_input_is_invalid_request() {
    let mapped = map_scsi(ScsiError::InvalidInput("test request bug"));
    assert!(matches!(mapped, TapeIoError::InvalidRequest(_)));
    assert!(!mapped.is_completion_unknown());
}

#[test]
fn map_scsi_truncated_response_is_malformed_response() {
    let mapped = map_scsi(ScsiError::Truncated { got: 4, need: 32 });
    assert!(matches!(mapped, TapeIoError::MalformedResponse(_)));
    assert!(!mapped.is_completion_unknown());
}

#[test]
fn map_scsi_descriptor_sense_maps_key_asc() {
    // Response code 0x72 (descriptor) keeps key/ASC/ASCQ at
    // different offsets from fixed-format sense. The shared Layer 1
    // decoder keeps this path aligned with CHECK CONDITION mapping.
    let mut sense = vec![0u8; 16];
    sense[0] = 0x72;
    sense[1] = 0x02; // sense key in byte 1 for descriptor sense
    sense[2] = 0x3A; // ASC
    sense[3] = 0x00; // ASCQ
    let e = ScsiError::CheckCondition {
        sense,
        bytes_transferred: 0,
    };
    assert!(matches!(map_scsi(e), TapeIoError::NoMedium));
}

#[test]
fn parse_read_position_at_bot() {
    let mut buf = [0u8; 32];
    buf[0] = 0b1000_0000; // BOP=1, EOP=0, BPEW=0
    buf[1] = 0; // partition 0
                // bytes 8..16 all zero (LBA = 0)
    let pos = parse_read_position_long(&buf).expect("parse ok");
    assert_eq!(pos.lba, 0);
    assert_eq!(pos.partition, 0);
    assert!(pos.beginning_of_partition);
    assert!(!pos.end_of_partition);
    assert!(!pos.block_position_end_of_warning);
}

#[test]
fn parse_read_position_mid_tape_with_bpew() {
    let mut buf = [0u8; 32];
    buf[0] = 0b0000_0001; // BPEW=1 only
    buf[1] = 0;
    buf[8..16].copy_from_slice(&0x12345678_9ABCDEF0u64.to_be_bytes());
    let pos = parse_read_position_long(&buf).expect("parse ok");
    assert_eq!(pos.lba, 0x12345678_9ABCDEF0);
    assert!(!pos.beginning_of_partition);
    assert!(!pos.end_of_partition);
    assert!(pos.block_position_end_of_warning);
}

#[test]
fn parse_read_position_at_eop() {
    let mut buf = [0u8; 32];
    buf[0] = 0b0100_0000; // EOP=1
    buf[8..16].copy_from_slice(&u64::MAX.to_be_bytes());
    let pos = parse_read_position_long(&buf).expect("parse ok");
    assert_eq!(pos.lba, u64::MAX);
    assert!(pos.end_of_partition);
    assert!(!pos.beginning_of_partition);
}

#[test]
fn parse_read_position_decodes_nonzero_partition() {
    // IBM Table 99: PARTITION NUMBER is a 4-byte field at bytes
    // 4..8 (codex 19:57 caught the earlier byte-1 parse).
    let mut buf = [0u8; 32];
    buf[0] = 0;
    buf[4..8].copy_from_slice(&0x0000_0001u32.to_be_bytes());
    buf[8..16].copy_from_slice(&42u64.to_be_bytes());
    let pos = parse_read_position_long(&buf).expect("parse ok");
    assert_eq!(pos.partition, 1);
    assert_eq!(pos.lba, 42);
}

#[test]
fn parse_read_position_decodes_large_partition_number() {
    // Sanity-check u32 width — production rem only operates in
    // partition 0, but the on-wire field is 4 bytes.
    let mut buf = [0u8; 32];
    buf[4..8].copy_from_slice(&0xCAFE_BABEu32.to_be_bytes());
    let pos = parse_read_position_long(&buf).expect("parse ok");
    assert_eq!(pos.partition, 0xCAFE_BABE);
}

#[test]
fn parse_read_position_byte_1_is_reserved_not_partition() {
    // Codex regression: a 1 in byte 1 (the old parse site) must
    // NOT affect partition any more.
    let mut buf = [0u8; 32];
    buf[1] = 0xFF; // reserved garbage
    buf[4..8].copy_from_slice(&7u32.to_be_bytes());
    let pos = parse_read_position_long(&buf).expect("parse ok");
    assert_eq!(pos.partition, 7);
}

#[test]
fn parse_read_position_rejects_short_buffer() {
    let buf = [0u8; 20];
    let err = parse_read_position_long(&buf).expect_err("short buffer rejected");
    match err {
        TapeIoError::MalformedResponse(ScsiError::Truncated { got, need }) => {
            assert_eq!(got, 20);
            assert_eq!(need, 32);
        }
        other => panic!("expected Truncated, got {other:?}"),
    }
}

#[test]
fn tape_outcome_extracts_sense_from_check_condition() {
    let e = TapeIoError::CheckCondition(ScsiError::CheckCondition {
        sense: vec![0x70, 0, 0x05, 0, 0, 0, 0, 24],
        bytes_transferred: 0,
    });
    let outcome = tape_outcome(&e, false);
    match outcome {
        AuditOutcome::ScsiError { sense, dirty, .. } => {
            assert_eq!(sense.unwrap()[0], 0x70);
            assert!(!dirty);
        }
        other => panic!("expected ScsiError outcome, got {other:?}"),
    }
}

#[test]
fn tape_outcome_marks_dirty_when_caller_says() {
    let e = TapeIoError::Transport(ScsiError::TransportError {
        status: 0,
        host_status: 0,
        driver_status: 0x06,
        info: 1,
        sense: Vec::new(),
    });
    let outcome = tape_outcome(&e, true);
    match outcome {
        AuditOutcome::ScsiError { dirty, .. } => assert!(dirty),
        other => panic!("expected ScsiError outcome, got {other:?}"),
    }
}

#[test]
fn tape_outcome_no_medium_has_no_sense() {
    let e = TapeIoError::NoMedium;
    let outcome = tape_outcome(&e, false);
    match outcome {
        AuditOutcome::ScsiError { sense, .. } => assert!(sense.is_none()),
        other => panic!("expected ScsiError outcome, got {other:?}"),
    }
}

// -- space-residual helper (Step 9.5) --------------------------

/// Build a fixed-format sense buffer with VALID bit set + signed
/// 32-bit INFORMATION + sense key.
fn sense_with_info(key: u8, info: i32) -> Vec<u8> {
    let mut v = vec![0u8; 32];
    v[0] = 0x80 | 0x70; // VALID + fixed-format current
    v[2] = key & 0x0F;
    v[3..7].copy_from_slice(&(info as u32).to_be_bytes());
    v[7] = 24;
    v
}

#[test]
fn space_residual_returns_signed_information_for_no_sense() {
    // SPACE(Filemarks, +10) stopped at EOD after moving 7 — drive
    // reports residual = 3 with sense key 0 (NO SENSE) + FM bit
    // (the FM bit itself is informational; we key off VALID + key).
    let sense = sense_with_info(0x00, 3);
    assert_eq!(space_residual_if_early_stop(&sense), Some(3));
}

#[test]
fn space_residual_returns_negative_for_backward_short_stop() {
    // SPACE(Blocks, -10) hit BOP after moving -4 — residual = -6.
    let sense = sense_with_info(0x00, -6);
    assert_eq!(space_residual_if_early_stop(&sense), Some(-6));
}

#[test]
fn space_residual_traversal_clamps_impossible_positive_residual() {
    assert_eq!(units_traversed_from_space_residual(5, 10), 0);
    assert_eq!(units_traversed_from_space_residual(5, -10), 5);
}

#[test]
fn space_residual_traversal_clamps_impossible_negative_residual() {
    assert_eq!(units_traversed_from_space_residual(-5, -10), 0);
    assert_eq!(units_traversed_from_space_residual(-5, 10), -5);
}

#[test]
fn space_residual_blank_check_for_eod_crossing() {
    // SPACE(Blocks) past EOD raises BLANK CHECK (key=8) with
    // residual.
    let sense = sense_with_info(0x08, 2);
    assert_eq!(space_residual_if_early_stop(&sense), Some(2));
}

#[test]
fn space_residual_returns_none_when_valid_bit_clear() {
    let mut sense = sense_with_info(0x00, 5);
    sense[0] = 0x70; // VALID cleared
    assert_eq!(space_residual_if_early_stop(&sense), None);
}

#[test]
fn space_residual_returns_none_for_real_error_sense_keys() {
    // MEDIUM ERROR (3) is a hard error even with VALID set.
    let sense = sense_with_info(0x03, 5);
    assert_eq!(space_residual_if_early_stop(&sense), None);
    // DATA PROTECT (7) similarly.
    let sense = sense_with_info(0x07, 5);
    assert_eq!(space_residual_if_early_stop(&sense), None);
}

#[test]
fn space_residual_returns_none_for_descriptor_sense() {
    let mut sense = vec![0u8; 16];
    sense[0] = 0xF2; // 0x72 with VALID — descriptor format
    sense[1] = 0x00;
    assert_eq!(space_residual_if_early_stop(&sense), None);
}

#[test]
fn space_residual_handles_empty_sense() {
    assert_eq!(space_residual_if_early_stop(&[]), None);
}

// -- ili_signed_information helper (Step 9.6) ------------------

fn ili_sense(info: i32) -> Vec<u8> {
    // Fixed-format sense with VALID + ILI + signed INFORMATION.
    let mut v = vec![0u8; 32];
    v[0] = 0x80 | 0x70; // VALID + 0x70 current fixed
    v[2] = 0x20; // ILI bit set, key = 0
    v[3..7].copy_from_slice(&(info as u32).to_be_bytes());
    v[7] = 24;
    v
}

#[test]
fn ili_information_positive_means_block_smaller_than_buffer() {
    // requested 1024, actual 768 → INFORMATION = +256.
    let sense = ili_sense(256);
    assert_eq!(ili_signed_information(&sense), Some(256));
}

#[test]
fn ili_information_negative_means_block_larger_than_buffer() {
    // requested 1024, actual 65536 → INFORMATION = -64512.
    let sense = ili_sense(-64_512);
    assert_eq!(ili_signed_information(&sense), Some(-64_512));
}

#[test]
fn ili_returns_none_without_ili_bit() {
    let mut sense = ili_sense(100);
    sense[2] = 0x00; // ILI cleared
    assert_eq!(ili_signed_information(&sense), None);
}

#[test]
fn ili_returns_none_without_valid_bit() {
    let mut sense = ili_sense(100);
    sense[0] = 0x70; // VALID cleared
    assert_eq!(ili_signed_information(&sense), None);
}

#[test]
fn ili_returns_none_for_descriptor_sense() {
    let mut sense = vec![0u8; 16];
    sense[0] = 0xF2; // descriptor + VALID
    assert_eq!(ili_signed_information(&sense), None);
}

// -- write_eom_signal helper (Step 9.7) ------------------------

fn eom_sense(key: u8) -> Vec<u8> {
    let mut v = vec![0u8; 32];
    v[0] = 0x70; // fixed-format current (VALID optional for EOM)
    v[2] = 0x40 | (key & 0x0F); // EOM + key
    v[7] = 24;
    v
}

#[test]
fn write_eom_signal_early_warning_on_no_sense_plus_eom() {
    let s = eom_sense(0x00);
    let sig = write_eom_signal(&s).expect("EW signal present");
    assert!(sig.early_warning);
    assert!(!sig.end_of_medium);
}

#[test]
fn write_eom_signal_end_of_medium_on_volume_overflow() {
    let s = eom_sense(0x0D);
    let sig = write_eom_signal(&s).expect("EOM signal present");
    assert!(sig.early_warning);
    assert!(sig.end_of_medium);
}

#[test]
fn write_eom_signal_returns_none_without_eom_bit() {
    let mut s = eom_sense(0x00);
    s[2] &= !0x40; // clear EOM
    assert!(write_eom_signal(&s).is_none());
}

#[test]
fn write_eom_signal_returns_none_for_hard_error_keys() {
    // MEDIUM ERROR (3) with EOM bit set is a real failure, not
    // an informational EW signal.
    let s = eom_sense(0x03);
    assert!(write_eom_signal(&s).is_none());
    // DATA PROTECT (7) similarly.
    let s = eom_sense(0x07);
    assert!(write_eom_signal(&s).is_none());
}

#[test]
fn write_eom_signal_returns_none_for_descriptor_sense() {
    let mut s = vec![0u8; 16];
    s[0] = 0x72;
    assert!(write_eom_signal(&s).is_none());
}

// -- read_filemark_signal helper (Step 9.6) --------------------

fn filemark_sense() -> Vec<u8> {
    let mut v = vec![0u8; 32];
    v[0] = 0x80 | 0x70; // VALID + fixed-format current
    v[2] = 0x80; // FILEMARK + NO SENSE
    v[7] = 24;
    v[12] = 0x00;
    v[13] = 0x01;
    v
}

#[test]
fn read_filemark_signal_detects_valid_fixed_filemark() {
    assert!(read_filemark_signal(&filemark_sense()));
}

#[test]
fn read_filemark_signal_requires_valid_bit() {
    let mut sense = filemark_sense();
    sense[0] = 0x70;
    assert!(!read_filemark_signal(&sense));
}

#[test]
fn read_filemark_signal_returns_none_for_descriptor_sense() {
    let mut sense = vec![0u8; 16];
    sense[0] = 0x72;
    sense[1] = 0x00;
    sense[2] = 0x00;
    sense[3] = 0x01;
    assert!(!read_filemark_signal(&sense));
}

// -- MODE SENSE / SELECT helpers (Step 9.7b) -------------------

/// Build a 28-byte MODE SENSE(6) response with page 0x0F.
/// `block_length` goes into the block descriptor's BLOCK LENGTH
/// field (3-byte BE at bytes 9..12). `dce` becomes byte 0 bit 7
/// of the page payload.
fn mode_sense_compression_response(
    block_length: u32,
    dce: bool,
    medium_type: u8,
    device_specific: u8,
) -> Vec<u8> {
    let mut buf = vec![0u8; 28];
    // Header
    buf[0] = 27; // Mode Data Length (n-1)
    buf[1] = medium_type; // Medium Type
    buf[2] = device_specific; // Device-Specific Parameter
    buf[3] = 8; // BDL
                // Block descriptor (bytes 4..12)
    let bl = block_length.to_be_bytes();
    buf[9] = bl[1];
    buf[10] = bl[2];
    buf[11] = bl[3];
    // Page 0x0F (bytes 12..28)
    buf[12] = 0x0F;
    buf[13] = 14;
    buf[14] = if dce { 0x80 } else { 0x00 };
    // bytes 15..28 reserved/zero
    buf
}

#[test]
fn parse_mode_sense_variable_block_no_compression() {
    let buf = mode_sense_compression_response(0, false, 0x98, 0x10);
    let parsed = parse_mode_sense_data_compression(&buf).expect("parse ok");
    assert_eq!(parsed.block_size, BlockSize::Variable);
    assert!(!parsed.compression);
    assert_eq!(parsed.medium_type, 0x98);
    assert!(!parsed.write_protected);
    assert_eq!(parsed.worm, WormMediaState::NotWorm);
}

#[test]
fn parse_mode_sense_fixed_block_with_compression() {
    let buf = mode_sense_compression_response(65_536, true, 0x98, 0x10);
    let parsed = parse_mode_sense_data_compression(&buf).expect("parse ok");
    assert_eq!(parsed.block_size, BlockSize::Fixed { size_bytes: 65_536 });
    assert!(parsed.compression);
}

#[test]
fn parse_mode_sense_surfaces_write_protect_bit() {
    let buf = mode_sense_compression_response(0, false, 0x78, 0x90);
    let parsed = parse_mode_sense_data_compression(&buf).expect("parse ok");
    assert!(parsed.write_protected);
    assert_eq!(parsed.worm, WormMediaState::NotWorm);
}

#[test]
fn parse_mode_sense_detects_worm_medium_type() {
    let lto9 = mode_sense_compression_response(0, false, 0x9C, 0x10);
    let parsed = parse_mode_sense_data_compression(&lto9).expect("parse lto9 worm");
    assert_eq!(parsed.worm, WormMediaState::Worm);

    let legacy = mode_sense_compression_response(0, false, 0x01, 0x10);
    let parsed = parse_mode_sense_data_compression(&legacy).expect("parse legacy worm");
    assert_eq!(parsed.worm, WormMediaState::Worm);
}

#[test]
fn parse_mode_sense_rejects_short_header() {
    let buf = [0u8; 3];
    assert!(matches!(
        parse_mode_sense_data_compression(&buf),
        Err(TapeIoError::MalformedModeResponse(_))
    ));
}

#[test]
fn parse_mode_sense_rejects_unexpected_bdl() {
    let mut buf = mode_sense_compression_response(0, false, 0x98, 0x10);
    buf[3] = 0; // no block descriptor
    let err = parse_mode_sense_data_compression(&buf).unwrap_err();
    match err {
        TapeIoError::MalformedModeResponse(msg) => assert!(msg.contains("block descriptor")),
        other => panic!("expected MalformedModeResponse, got {other:?}"),
    }
}

#[test]
fn parse_mode_sense_rejects_wrong_page_code() {
    let mut buf = mode_sense_compression_response(0, false, 0x98, 0x10);
    buf[12] = 0x10; // device-config page instead of compression
    let err = parse_mode_sense_data_compression(&buf).unwrap_err();
    match err {
        TapeIoError::MalformedModeResponse(msg) => assert!(msg.contains("0x10")),
        other => panic!("expected MalformedModeResponse, got {other:?}"),
    }
}

#[test]
fn build_compression_param_list_variable_no_compression() {
    let cfg = TapeConfig {
        block_size: BlockSize::Variable,
        compression: false,
        max_block_size_bytes: 0x80_0000, // ignored
        write_protected: false,
        worm: WormMediaState::Unknown,
    };
    let buf = build_compression_param_list(&cfg);
    assert_eq!(buf.len(), 28);
    // Mode parameter header
    assert_eq!(buf[0], 0, "MODE SELECT MDL byte is reserved 0");
    assert_eq!(buf[1], 0, "Medium Type = 0");
    // Device-Specific Parameter byte: BUFFERED MODE 001b in
    // bits 4..6 → 0x10 (codex 20:38 idref=7714c4dc Medium).
    assert_eq!(
        buf[2], 0x10,
        "Device-Specific Parameter: BUFFERED MODE=001, SPEED=0"
    );
    assert_eq!(buf[3], 8, "BDL");
    // Block descriptor block length bytes — variable means 0
    assert_eq!(&buf[9..12], &[0, 0, 0]);
    // Page header
    assert_eq!(buf[12], 0x0F);
    assert_eq!(buf[13], 14);
    // DCE bit clear, but DCC = 1 (non-changeable per IBM
    // Table 345; codex 20:22).
    assert_eq!(buf[14] & 0x80, 0, "DCE clear when compression off");
    assert_eq!(buf[14] & 0x40, 0x40, "DCC must be 1");
    // DDE bit always 1 (non-changeable).
    assert_eq!(buf[15] & 0x80, 0x80, "DDE must be 1");
    // Compression / decompression algorithms = 1.
    assert_eq!(&buf[16..20], &[0, 0, 0, 1], "compression algorithm = 1");
    assert_eq!(&buf[20..24], &[0, 0, 0, 1], "decompression algorithm = 1");
}

#[test]
fn build_compression_param_list_fixed_with_compression() {
    let cfg = TapeConfig {
        block_size: BlockSize::Fixed {
            size_bytes: 0x010000,
        }, // 64 KiB
        compression: true,
        max_block_size_bytes: 0,
        write_protected: false,
        worm: WormMediaState::Unknown,
    };
    let buf = build_compression_param_list(&cfg);
    // Device-Specific Parameter byte stays at 0x10 regardless
    // of compression / block size.
    assert_eq!(buf[2], 0x10);
    // Block length 24-bit BE
    assert_eq!(&buf[9..12], &[0x01, 0x00, 0x00]);
    // DCE bit set; DCC still 1 (non-changeable).
    assert_eq!(buf[14] & 0x80, 0x80, "DCE set when compression on");
    assert_eq!(buf[14] & 0x40, 0x40, "DCC must still be 1");
    // DDE still 1; algorithms still 1.
    assert_eq!(buf[15] & 0x80, 0x80, "DDE must be 1");
    assert_eq!(&buf[16..20], &[0, 0, 0, 1]);
    assert_eq!(&buf[20..24], &[0, 0, 0, 1]);
}

#[test]
fn parse_mode_sense_round_trips_with_builder() {
    // Sanity: build_compression_param_list followed by a parse
    // (with reformatted header for SENSE shape) recovers the
    // original config. The MODE SELECT header byte 0 is
    // reserved (0); MODE SENSE byte 0 is Mode Data Length.
    // For a 28-byte response, MDL = 27.
    let cfg_in = TapeConfig {
        block_size: BlockSize::Fixed {
            size_bytes: 0x40_0000,
        },
        compression: false,
        max_block_size_bytes: 0,
        write_protected: false,
        worm: WormMediaState::Unknown,
    };
    let mut buf = build_compression_param_list(&cfg_in);
    buf[0] = (buf.len() as u8) - 1; // give it a SENSE-shaped header
    let parsed = parse_mode_sense_data_compression(&buf).expect("round-trips");
    assert_eq!(
        parsed.block_size,
        BlockSize::Fixed {
            size_bytes: 0x40_0000
        }
    );
    assert!(!parsed.compression);
    assert!(!parsed.write_protected);
    assert_eq!(parsed.worm, WormMediaState::Unknown);
}
