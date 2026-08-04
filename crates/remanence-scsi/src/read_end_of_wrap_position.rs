//! READ END OF WRAP POSITION (`A3h/1Fh/45h`) — IBM LTO SCSI Reference
//! GA32-0928-09 §5.2.20.
//!
//! Returns, for every wrap **containing user data**, the logical object
//! identifier at the end of that wrap. Two response facts are semantic,
//! not incidental, and the returned type carries both rather than
//! flattening them into a bare list:
//!
//! - Only wraps containing user data are reported. A blank region of the
//!   cartridge produces no descriptor, so the list ends at the wrap
//!   holding EOD.
//! - For the wrap holding EOD the reported value is **EOD's own logical
//!   position**, not that wrap's true physical end. Every earlier
//!   descriptor's `end_loi` is the inclusive last logical object on its
//!   wrap; the final descriptor's is an exclusive extent bound. The
//!   [`EndOfWrapPositions::completed_wraps`] / [`EndOfWrapPositions::eod_wrap`]
//!   split preserves the distinction.
//!
//! Lifecycle (for callers; this module does nothing about it): the
//! return data is a snapshot the drive takes at load. GA32-0928-09
//! states it is "valid at load" and "becomes stale on any write
//! operation" — re-issuing the command later in the same load does not
//! refresh it. Harvest timing and cache invalidation live above Layer 1
//! (design-read-ordering.md §6.5).
//!
//! Only the **long form** (`RA=1`) is implemented: one response listing
//! every reported wrap. The short form (`RA=0`, one wrap picked by the
//! WRAP NUMBER field, `WNV`) has no caller in rem and is not built.
//!
//! Support begins at LTO-7. Capability is detected by
//! `REPORT SUPPORTED OPERATION CODES` for this opcode and service action
//! ([`capability_probe_cdb`]), not by an INQUIRY-derived generation
//! table, so a drive that surprises us degrades instead of erroring.

use crate::error::ScsiError;

/// SCSI opcode — MAINTENANCE IN.
pub const OPCODE: u8 = 0xA3;

/// MAINTENANCE IN service action for READ END OF WRAP POSITION.
pub const SERVICE_ACTION: u8 = 0x1F;

/// Service action qualifier (CDB byte 2) selecting READ END OF WRAP
/// POSITION within `A3h/1Fh`.
pub const SERVICE_ACTION_QUALIFIER: u8 = 0x45;

/// CDB byte 3 for the long form: bit 1 `RA` (report all) set, bit 0
/// `WNV` (wrap number valid) clear.
pub const LONG_FORM_BYTE3: u8 = 0x02;

/// Length of one wrap descriptor in the long-form response.
pub const DESCRIPTOR_LEN: usize = 12;

/// Length of the long-form response header: a 2-byte response data
/// length followed by 2 reserved bytes.
pub const HEADER_LEN: usize = 4;

/// Recommended allocation length for the long form. The largest
/// supported geometry (LTO-10, 472 wraps) needs `4 + 472 × 12 = 5,668`
/// bytes for a single partition; 16 KiB leaves comfortable headroom for
/// multi-partition cartridges without a second round trip.
pub const ALLOC_LEN: u32 = 16_384;

/// Build the 12-byte long-form READ END OF WRAP POSITION CDB.
///
/// `RA` is set and `WNV` is clear, so the WRAP NUMBER field (bytes 4–5)
/// is unused and left zero. The response buffer is parsed by
/// [`parse_response`].
pub fn build_cdb(alloc_len: u32) -> [u8; 12] {
    let alloc = alloc_len.to_be_bytes();
    [
        OPCODE,
        SERVICE_ACTION & 0x1F,
        SERVICE_ACTION_QUALIFIER,
        LONG_FORM_BYTE3,
        0x00, // wrap number MSB — unused when WNV=0
        0x00, // wrap number LSB — unused when WNV=0
        alloc[0],
        alloc[1],
        alloc[2],
        alloc[3],
        0x00, // reserved
        0x00, // control
    ]
}

/// Build the `REPORT SUPPORTED OPERATION CODES` CDB that probes for
/// this command — the capability check the design requires instead of
/// an INQUIRY-derived generation table.
///
/// The probe keys on `(A3h, 001Fh)`: RSOC's REQUESTED SERVICE ACTION
/// field has no room for the `45h` service action qualifier, which is
/// an IBM extension inside the CDB body rather than part of the SPC
/// service-action key. A drive that reports `A3h/1Fh` supported but
/// still rejects qualifier `45h` at runtime is handled by the runtime
/// rejection path (design §6.5's defensive
/// `UNAVAILABLE_UNSUPPORTED_FORMAT` mapping), not by this probe.
///
/// Parse the response with
/// [`crate::report_supported_opcodes::parse_one_command_response`].
pub fn capability_probe_cdb(alloc_len: u32) -> [u8; 12] {
    crate::report_supported_opcodes::build_cdb(OPCODE, u16::from(SERVICE_ACTION), alloc_len)
}

/// One long-form wrap descriptor, exactly as reported.
///
/// Field widths are the wire widths: 2-byte wrap number, 2-byte
/// partition, 6-byte logical object identifier widened to `u64`. The
/// semantics match `remanence-order`'s harvested-descriptor shape —
/// `end_loi` is the inclusive last logical object of a completed wrap
/// and EOD's exclusive position for the wrap holding EOD — so the
/// conversion at the harvest call site is a plain field mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WrapDescriptor {
    /// Wrap the descriptor reports on.
    pub wrap_number: u16,
    /// Tape partition the descriptor reports on. Always 0 here:
    /// descriptors for other partitions are counted and skipped during
    /// parsing (design §6.4 — ignored, not a failure).
    pub partition: u16,
    /// Logical object identifier at the end of the wrap. Inclusive for
    /// completed wraps; EOD's own (exclusive) position for the wrap
    /// holding EOD.
    pub end_loi: u64,
}

/// Parsed long-form response: the partition-0 wrap descriptors,
/// validated contiguous ascending from wrap 0.
///
/// The last descriptor is the wrap holding EOD, whose `end_loi` is
/// EOD's position rather than a true wrap end — use
/// [`completed_wraps`](Self::completed_wraps) and
/// [`eod_wrap`](Self::eod_wrap) rather than treating the list as
/// uniform.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EndOfWrapPositions {
    wraps: Vec<WrapDescriptor>,
    ignored_nonzero_partition_descriptors: usize,
}

impl EndOfWrapPositions {
    /// All accepted (partition-0) descriptors, in reported order —
    /// contiguous ascending from wrap 0.
    pub fn descriptors(&self) -> &[WrapDescriptor] {
        &self.wraps
    }

    /// The completed wraps: every descriptor except the last. For these,
    /// `end_loi` is the exact, inclusive last logical object on the
    /// wrap.
    pub fn completed_wraps(&self) -> &[WrapDescriptor] {
        match self.wraps.len() {
            0 => &[],
            n => &self.wraps[..n - 1],
        }
    }

    /// The wrap holding EOD: the last reported descriptor. Its
    /// `end_loi` is EOD's own logical position (an exclusive extent
    /// bound), **not** the wrap's true physical end. `None` when the
    /// response reported no partition-0 wraps at all.
    pub fn eod_wrap(&self) -> Option<&WrapDescriptor> {
        self.wraps.last()
    }

    /// How many descriptors were skipped because they reported a
    /// partition other than 0. Skipping is not a failure (design §6.4);
    /// the count is kept for observability.
    pub fn ignored_nonzero_partition_descriptors(&self) -> usize {
        self.ignored_nonzero_partition_descriptors
    }
}

/// Parse a long-form READ END OF WRAP POSITION response.
///
/// Wire validation, all failing closed:
///
/// - the buffer must cover the 4-byte header and the declared length —
///   a truncated response is rejected, never silently shortened;
/// - the declared response data length must cover its 2 reserved bytes
///   and then describe a whole number of 12-byte descriptors;
/// - partition-0 wrap numbers must be contiguous ascending from wrap 0
///   (design §6.4) — out-of-order, gapped or duplicated wrap numbers
///   are rejected rather than re-sorted or accepted;
/// - descriptors with `partition != 0` are counted and skipped, not
///   treated as failure.
///
/// Bytes beyond the declared length are allocation slack from the
/// caller's buffer and are ignored, matching this crate's other
/// length-driven parsers. An empty descriptor list is wire-valid (a
/// blank cartridge has no wrap containing user data); whether it is an
/// acceptable *harvest* is decided above Layer 1.
pub fn parse_response(buf: &[u8]) -> Result<EndOfWrapPositions, ScsiError> {
    if buf.len() < HEADER_LEN {
        return Err(ScsiError::Truncated {
            got: buf.len(),
            need: HEADER_LEN,
        });
    }
    let declared = usize::from(u16::from_be_bytes([buf[0], buf[1]]));
    if declared < 2 {
        return Err(ScsiError::InvalidResponse {
            offset: 0,
            detail: "REOWP response data length cannot cover its two reserved header bytes",
        });
    }
    let descriptor_bytes = declared - 2;
    if descriptor_bytes % DESCRIPTOR_LEN != 0 {
        return Err(ScsiError::InvalidResponse {
            offset: 0,
            detail: "REOWP response data length does not describe whole 12-byte wrap descriptors",
        });
    }
    let total = 2 + declared;
    if buf.len() < total {
        return Err(ScsiError::Truncated {
            got: buf.len(),
            need: total,
        });
    }

    let mut wraps = Vec::with_capacity(descriptor_bytes / DESCRIPTOR_LEN);
    let mut ignored = 0usize;
    let mut offset = HEADER_LEN;
    while offset < total {
        let wrap_number = u16::from_be_bytes([buf[offset], buf[offset + 1]]);
        let partition = u16::from_be_bytes([buf[offset + 2], buf[offset + 3]]);
        // Bytes 4–5 of the descriptor are reserved and not validated.
        let mut end_loi = 0u64;
        for byte in &buf[offset + 6..offset + DESCRIPTOR_LEN] {
            end_loi = (end_loi << 8) | u64::from(*byte);
        }
        if partition != 0 {
            ignored += 1;
        } else {
            if usize::from(wrap_number) != wraps.len() {
                return Err(ScsiError::InvalidResponse {
                    offset,
                    detail: if wraps.is_empty() {
                        "first partition-0 wrap descriptor does not report wrap 0"
                    } else {
                        "partition-0 wrap numbers are not contiguous ascending"
                    },
                });
            }
            wraps.push(WrapDescriptor {
                wrap_number,
                partition,
                end_loi,
            });
        }
        offset += DESCRIPTOR_LEN;
    }

    Ok(EndOfWrapPositions {
        wraps,
        ignored_nonzero_partition_descriptors: ignored,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The six wrap-end LOIs documented from the real LTO-8 capture
    /// (prompt-ro-p2-reowp.md; response data length `0x07CA`, 166
    /// descriptors).
    const DOCUMENTED_END_LOIS: [u64; 6] =
        [207_516, 415_522, 624_102, 832_574, 1_040_610, 1_249_095];

    fn push_descriptor(buf: &mut Vec<u8>, wrap: u16, partition: u16, end_loi: u64) {
        buf.extend_from_slice(&wrap.to_be_bytes());
        buf.extend_from_slice(&partition.to_be_bytes());
        buf.extend_from_slice(&[0x00, 0x00]); // reserved
        buf.extend_from_slice(&end_loi.to_be_bytes()[2..8]); // low 6 bytes, BE
    }

    /// Build a response around explicit descriptors, with a consistent
    /// declared length. Used by the failure-path tests.
    fn response(descriptors: &[(u16, u16, u64)]) -> Vec<u8> {
        let data_len = 2 + descriptors.len() * DESCRIPTOR_LEN;
        let mut buf = Vec::with_capacity(2 + data_len);
        buf.extend_from_slice(&(data_len as u16).to_be_bytes());
        buf.extend_from_slice(&[0x00, 0x00]);
        for &(wrap, partition, end_loi) in descriptors {
            push_descriptor(&mut buf, wrap, partition, end_loi);
        }
        buf
    }

    /// Reconstruction of the real captured LTO-8 response.
    ///
    /// The raw capture bytes were not committed to the panel sources;
    /// what is documented is the response data length `0x07CA` (1,994
    /// bytes → 166 descriptors) and the end LOIs of wraps 0–5. The
    /// byte sequence is rebuilt from those documented values: header
    /// and wraps 0–5 are the real figures, wraps 6–164 continue with a
    /// fixed +208,250 stride (inside the observed 208,006–208,580
    /// delta band of the real values), and wrap 165 — the EOD wrap —
    /// gets a partial stride, since its reported value is EOD's
    /// position rather than a wrap end. Assertions against the
    /// synthesized tail would be circular, so the tests assert only
    /// the documented facts: the count, the header arithmetic, and
    /// the six real values.
    fn golden_capture() -> Vec<u8> {
        let mut buf = vec![0x07, 0xCA, 0x00, 0x00];
        let mut end_lois = DOCUMENTED_END_LOIS.to_vec();
        let mut last = *end_lois.last().expect("six documented values");
        for _ in 6..165 {
            last += 208_250;
            end_lois.push(last);
        }
        end_lois.push(last + 104_000); // wrap 165: EOD partway into the wrap
        assert_eq!(end_lois.len(), 166);
        for (wrap, end_loi) in end_lois.iter().enumerate() {
            push_descriptor(&mut buf, wrap as u16, 0, *end_loi);
        }
        buf
    }

    #[test]
    fn golden_capture_header_arithmetic_is_the_documented_length() {
        // 0x07CA = 1,994 = 2 reserved header bytes + 166 × 12; the
        // full transfer is the 2-byte length field plus that.
        assert_eq!(0x07CA, 2 + 166 * DESCRIPTOR_LEN);
        assert_eq!(golden_capture().len(), 2 + 0x07CA);
    }

    #[test]
    fn golden_capture_parses_to_166_descriptors_with_documented_values() {
        let parsed = parse_response(&golden_capture()).expect("golden capture parses");
        assert_eq!(parsed.descriptors().len(), 166);
        assert_eq!(parsed.ignored_nonzero_partition_descriptors(), 0);
        for (wrap, expected) in DOCUMENTED_END_LOIS.iter().enumerate() {
            let d = parsed.descriptors()[wrap];
            assert_eq!(d.wrap_number, wrap as u16);
            assert_eq!(d.partition, 0);
            assert_eq!(d.end_loi, *expected, "wrap {wrap} end LOI");
        }
        // The EOD split: 165 completed wraps, the 166th holds EOD.
        assert_eq!(parsed.completed_wraps().len(), 165);
        let eod = parsed
            .eod_wrap()
            .expect("non-empty capture has an EOD wrap");
        assert_eq!(eod.wrap_number, 165);
    }

    #[test]
    fn cdb_bytes_exactly_as_tabulated() {
        let cdb = build_cdb(0x07CA + 2);
        assert_eq!(
            cdb,
            [
                0xA3, // opcode
                0x1F, // service action
                0x45, // service action qualifier
                0x02, // RA=1, WNV=0
                0x00, 0x00, // wrap number, unused in long form
                0x00, 0x00, 0x07, 0xCC, // allocation length, big-endian
                0x00, // reserved
                0x00, // control
            ]
        );
        assert_eq!(cdb[3] & 0x02, 0x02, "RA must be set for the long form");
        assert_eq!(cdb[3] & 0x01, 0x00, "WNV must be clear");
    }

    #[test]
    fn cdb_alloc_length_is_four_byte_big_endian() {
        let cdb = build_cdb(0x0102_0304);
        assert_eq!(&cdb[6..10], &[0x01, 0x02, 0x03, 0x04]);
    }

    #[test]
    fn capability_probe_targets_a3_1f_via_rsoc() {
        let cdb = capability_probe_cdb(64);
        assert_eq!(cdb[0], 0xA3, "RSOC opcode (MAINTENANCE IN)");
        assert_eq!(cdb[1] & 0x1F, 0x0C, "RSOC service action");
        assert_eq!(cdb[3], OPCODE, "requested operation code is REOWP's A3h");
        assert_eq!(&cdb[4..6], &[0x00, 0x1F], "requested service action 001Fh");
    }

    #[test]
    fn buffer_shorter_than_header_is_rejected() {
        let err = parse_response(&[0x07, 0xCA, 0x00]).expect_err("short header");
        assert!(matches!(err, ScsiError::Truncated { got: 3, need: 4 }));
    }

    #[test]
    fn truncated_response_is_rejected_not_silently_shortened() {
        // Cut the golden capture mid-descriptor without touching the
        // declared length. A parser that walked whole descriptors and
        // stopped early would yield 165 wraps and no error — the
        // failure this test guards against.
        let mut buf = golden_capture();
        buf.truncate(buf.len() - 5);
        let err = parse_response(&buf).expect_err("truncated capture");
        assert!(matches!(err, ScsiError::Truncated { need, .. } if need == 2 + 0x07CA));
    }

    #[test]
    fn declared_length_beyond_actual_buffer_is_rejected() {
        // One whole descriptor present, but the header declares two.
        let mut buf = response(&[(0, 0, 207_516)]);
        let inflated = (2 + 2 * DESCRIPTOR_LEN) as u16;
        buf[0..2].copy_from_slice(&inflated.to_be_bytes());
        let err = parse_response(&buf).expect_err("declared length disagrees");
        assert!(matches!(err, ScsiError::Truncated { .. }));
    }

    #[test]
    fn declared_length_not_a_whole_descriptor_multiple_is_rejected() {
        // Declared 2 + 11: covers the reserved bytes but not a whole
        // 12-byte descriptor.
        let mut buf = response(&[(0, 0, 207_516)]);
        buf[0..2].copy_from_slice(&13u16.to_be_bytes());
        let err = parse_response(&buf).expect_err("ragged declared length");
        assert!(matches!(err, ScsiError::InvalidResponse { offset: 0, .. }));
    }

    #[test]
    fn declared_length_smaller_than_reserved_bytes_is_rejected() {
        let err = parse_response(&[0x00, 0x00, 0x00, 0x00]).expect_err("declared 0");
        assert!(matches!(err, ScsiError::InvalidResponse { offset: 0, .. }));
    }

    #[test]
    fn nonzero_partition_descriptors_are_ignored_not_a_failure() {
        // Partition-1 descriptors interleaved and leading; the
        // partition-0 sequence 0..=2 must survive intact.
        let buf = response(&[
            (0, 1, 999_999),
            (0, 0, 207_516),
            (0, 1, 100),
            (1, 0, 415_522),
            (2, 0, 624_102),
            (7, 2, 55),
        ]);
        let parsed = parse_response(&buf).expect("nonzero partitions are skipped");
        assert_eq!(parsed.descriptors().len(), 3);
        assert_eq!(parsed.ignored_nonzero_partition_descriptors(), 3);
        assert_eq!(parsed.descriptors()[0].end_loi, 207_516);
        assert_eq!(parsed.descriptors()[2].end_loi, 624_102);
        assert_eq!(parsed.eod_wrap().expect("eod").wrap_number, 2);
    }

    #[test]
    fn out_of_order_wrap_numbers_are_rejected() {
        let buf = response(&[(0, 0, 100), (2, 0, 300), (1, 0, 200)]);
        let err = parse_response(&buf).expect_err("out of order");
        assert!(
            matches!(err, ScsiError::InvalidResponse { offset, .. } if offset == 4 + DESCRIPTOR_LEN)
        );
    }

    #[test]
    fn gapped_wrap_numbers_are_rejected() {
        let buf = response(&[(0, 0, 100), (1, 0, 200), (3, 0, 400)]);
        let err = parse_response(&buf).expect_err("gap");
        assert!(
            matches!(err, ScsiError::InvalidResponse { offset, .. } if offset == 4 + 2 * DESCRIPTOR_LEN)
        );
    }

    #[test]
    fn first_wrap_not_zero_is_rejected() {
        let buf = response(&[(1, 0, 200), (2, 0, 300)]);
        let err = parse_response(&buf).expect_err("first wrap not zero");
        assert!(matches!(err, ScsiError::InvalidResponse { offset: 4, .. }));
    }

    #[test]
    fn duplicate_wrap_numbers_are_rejected() {
        let buf = response(&[(0, 0, 100), (1, 0, 200), (1, 0, 201)]);
        let err = parse_response(&buf).expect_err("duplicate");
        assert!(
            matches!(err, ScsiError::InvalidResponse { offset, .. } if offset == 4 + 2 * DESCRIPTOR_LEN)
        );
    }

    #[test]
    fn empty_descriptor_list_is_wire_valid() {
        // A blank cartridge has no wrap containing user data. The wire
        // response is valid; whether the harvest accepts it is decided
        // above Layer 1.
        let parsed = parse_response(&response(&[])).expect("empty list parses");
        assert!(parsed.descriptors().is_empty());
        assert!(parsed.completed_wraps().is_empty());
        assert!(parsed.eod_wrap().is_none());
    }

    #[test]
    fn trailing_allocation_slack_is_ignored() {
        let mut buf = golden_capture();
        let canonical = parse_response(&buf).expect("golden parses");
        buf.extend_from_slice(&[0u8; 64]);
        let padded = parse_response(&buf).expect("slack tolerated");
        assert_eq!(canonical, padded);
    }

    #[test]
    fn end_loi_decodes_all_six_big_endian_bytes() {
        let buf = response(&[(0, 0, 0x0000_FFEE_DDCC_BBAA)]);
        let parsed = parse_response(&buf).expect("parses");
        assert_eq!(parsed.descriptors()[0].end_loi, 0x0000_FFEE_DDCC_BBAA);
    }
}
