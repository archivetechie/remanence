//! Resolve the frozen 7c formulas against healthy wire bytes. Repairs hash the
//! actual bounded payload/slots, never attacker-supplied counts. Production
//! arithmetic is observed through the parent's single dispatch table.
use super::*;

impl Editor {
    fn number(
        &mut self,
        f: usize,
        b: usize,
        offset: usize,
        old: u64,
        new: u64,
        width: usize,
    ) -> Result<(), String> {
        self.field(
            f,
            b,
            &json!({"offset":format!("0x{offset:x}..0x{:x}",offset+width),"from":old,"to":new}),
        )
    }
    pub(super) fn slot(
        &mut self,
        f: usize,
        offset: usize,
        width: usize,
        change: impl FnOnce(&mut Cbor) -> Result<(), String>,
    ) -> Result<(), String> {
        let bytes = &self.files[&f][1][offset..offset + width];
        let len = u16::from_le_bytes(bytes[..2].try_into().unwrap()) as usize;
        let mut row: Cbor = ciborium::from_reader(&bytes[2..2 + len]).map_err(|e| e.to_string())?;
        change(&mut row)?;
        let encoded = cbor_bytes(&row);
        if encoded.len() + 2 > width {
            return Err("mutated slot exceeds capacity".into());
        }
        let mut slot = vec![0; width];
        slot[..2].copy_from_slice(&(encoded.len() as u16).to_le_bytes());
        slot[2..2 + encoded.len()].copy_from_slice(&encoded);
        self.write(
            f,
            1,
            offset,
            &slot,
            "mutation: canonical CBOR slot and zero padding",
        );
        Ok(())
    }
    pub(super) fn replica_payload(&mut self, f: usize, structural: bool) -> Result<(), String> {
        if structural {
            let n = u64_at(&self.files[&f][0], 0x60) as usize;
            let mut rows = Vec::new();
            for i in 0..n {
                let slot = &self.files[&f][1][i * 64..(i + 1) * 64];
                let len = u16::from_le_bytes(slot[..2].try_into().unwrap()) as usize;
                rows.push(ciborium::from_reader(&slot[2..2 + len]).map_err(|e| e.to_string())?);
            }
            let hash = Sha256::digest(cbor_bytes(&Cbor::Array(rows)));
            for b in [0, 2] {
                self.write(f, b, 0xc8, &hash, "R-TR-PAYLOAD canonical-map SHA-256");
            }
        }
        let len = u64_at(&self.files[&f][0], 0x70) as usize;
        let mut h = Sha256::new();
        h.update(b"REM-TAPE-INDEX-REPLICA-PAYLOAD-V1\0");
        h.update(&self.files[&f][1][..len]);
        let hash = h.finalize();
        for b in [0, 2] {
            self.write(f, b, 0xa8, &hash, "R-TR-PAYLOAD payload SHA-256");
        }
        self.replica_repair(f, true);
        Ok(())
    }
    pub(super) fn layout_digest(&mut self, f: usize) {
        let b = &self.files[&f][0];
        let mut h = Sha256::new();
        h.update(b"REM-TERMINAL-TAIL-LAYOUT-V1\0");
        h.update(&b[0x3c..0x44]);
        h.update(5u16.to_le_bytes());
        h.update(&b[0x148..0x1e8]);
        h.update(&b[0xa0..0xa8]);
        let hash = h.finalize();
        for b in [0, 2] {
            self.write(f, b, 0x108, &hash, "R-TR-COMMON layout digest");
        }
    }
    pub(super) fn separation_repair(&mut self, f: usize) {
        let b = &self.files[&f][0];
        let mut h = Sha256::new();
        h.update(b"REM-INDEX-SEPARATION-DESCRIPTOR-V1\0");
        h.update(&b[0x10..0x3c]);
        h.update(&b[0x50..0x60]);
        let gap = u16::from_le_bytes(b[0x30..0x32].try_into().unwrap()) as usize;
        let component = 0xe0 + (2 * gap - 1) * 32;
        for offset in [component, component - 32, component + 32] {
            h.update(&b[offset..offset + 32]);
        }
        h.update(&b[0x98..0xb8]);
        let hash = h.finalize();
        for b in [0, 2] {
            self.write(f, b, 0xb8, &hash, "R-SEP descriptor digest");
        }
        self.crc(f, 0, 0x1f8, "R-SEP header CRC");
        let hash = Sha256::digest(&self.files[&f][0]);
        self.write(f, 2, 0x180, &hash, "R-SEP footer header SHA-256");
        self.crc(f, 2, 0x1f8, "R-SEP footer CRC");
    }
    pub(super) fn parity_payload(&mut self, f: usize, payload: &Cbor, old_len: usize) {
        let new = cbor_bytes(payload);
        let mut padded = vec![0; old_len.max(new.len())];
        padded[..new.len()].copy_from_slice(&new);
        for b in [0, 1] {
            self.write(
                f,
                b,
                0xc8,
                &padded,
                "mutation: directory payload and zero fill",
            );
        }
        for b in [0, 1, 2] {
            self.write(
                f,
                b,
                0x30,
                &(new.len() as u64).to_le_bytes(),
                "R-PM-PAYLOAD payload length",
            );
            self.write(
                f,
                b,
                0x38,
                &Sha256::digest(&new),
                "R-PM-PAYLOAD payload SHA-256",
            );
            self.crc(f, b, 0xc0, "R-PM-PAYLOAD CRC");
        }
    }
}
fn replace(v: &mut Cbor, old: u64, new: u64) -> Result<(), String> {
    if v.as_integer().map(i128::from) != Some(i128::from(old)) {
        return Err(format!("CBOR source mismatch: expected {old}, found {v:?}"));
    }
    *v = Cbor::Integer(new.into());
    Ok(())
}

pub(super) fn mutate(
    e: &mut Editor,
    case: &Value,
    v: &Value,
    f: usize,
    block: &mut usize,
    targets: &mut Vec<usize>,
) -> Result<Role, String> {
    let id = case["id"].as_str().unwrap();
    match id {
        "overflow-3.3-stripe-mapping-forward"
        | "overflow-9.1-total-2H-P-1"
        | "overflow-9.1-block-locator"
        | "overflow-9.1-tail-start"
        | "overflow-9.2-end-minus-start"
        | "overflow-9.4-H" => {
            if id == "overflow-9.1-tail-start" {
                *block = 6;
            }
            let b = *block;
            let stream_end = 0xc8 + u64_at(&e.files[&f][if b == 6 { 0 } else { b }], 0x68) as usize;
            match id {
                "overflow-3.3-stripe-mapping-forward" => e.number(f, b, 0x24, 2, 0, 4)?,
                "overflow-9.1-total-2H-P-1" => {
                    e.number(f, b, if b == 6 { 0x38 } else { 0x60 }, 1, 1 << 63, 8)?
                }
                "overflow-9.1-block-locator" => e.number(f, b, 0x60, 1, u64::MAX, 8)?,
                "overflow-9.1-tail-start" => {
                    e.number(f, b, 0x38, 1, 1, 8)?;
                    e.number(f, b, 0x40, 4, u64::MAX, 8)?;
                }
                "overflow-9.2-end-minus-start" => {
                    e.number(f, b, 0x30, 4, 4, 8)?;
                    e.number(f, b, 0x38, 8, 3, 8)?;
                }
                "overflow-9.4-H" => {
                    e.number(f, b, 0x48, 4, 1 << 61, 8)?;
                    e.number(f, b, 0x38, 4, 1 << 61, 8)?;
                }
                _ => unreachable!(),
            }
            e.sidecar_repair(
                f,
                b,
                if b == 6 { "R-SC-FOOTER" } else { "R-SC-HASHED" },
                stream_end,
            );
            Ok(if b == 6 {
                Role::SidecarFooter
            } else {
                Role::SidecarCopy
            })
        }
        "overflow-6.6-scheme-product" | "overflow-9.1-P" | "overflow-9.2-SxK" => {
            let len = u32::from_le_bytes(e.files[&f][0][0x2c..0x30].try_into().unwrap()) as usize;
            let mut payload: Cbor = ciborium::from_reader(&e.files[&f][0][0x38..0x38 + len])
                .map_err(|e| e.to_string())?;
            let scheme = cbor_key(&mut payload, 1);
            replace(cbor_key(scheme, 2), 2, 2)?;
            replace(cbor_key(scheme, 3), 2, 2)?;
            replace(cbor_key(scheme, 4), 2, 1 << 63)?;
            let new = cbor_bytes(&payload);
            if new.len() != len + 8 {
                return Err("bootstrap payload did not grow by eight bytes".into());
            }
            e.write(f, 0, 0x38, &new, "mutation: bootstrap S = 2^63");
            e.number(f, 0, 0x2c, len as u64, new.len() as u64, 4)?;
            e.crc(f, 0, 0x30, "bootstrap header CRC");
            e.write(
                f,
                0,
                0x38 + new.len(),
                &crc64_xz(&new).to_le_bytes(),
                "bootstrap payload CRC at new position",
            );
            Ok(Role::Bootstrap)
        }
        "overflow-10.1.2-M" | "overflow-10.1.2-2M-footer" | "overflow-10.1.2-tail-at-M" => {
            let len = u64_at(&e.files[&f][0], 0x30);
            for b in [0, 1, 2] {
                if id == "overflow-10.1.2-2M-footer" {
                    e.number(f, b, 0x28, BLOCK as u64, 2, 4)?;
                }
                e.number(
                    f,
                    b,
                    0x30,
                    len,
                    if id == "overflow-10.1.2-2M-footer" {
                        u64::MAX - 0xc8
                    } else {
                        u64::MAX
                    },
                    8,
                )?;
                e.crc(f, b, 0xc0, "ParityMap CRC");
            }
            Ok(Role::ParityMap)
        }
        "overflow-7.2-T" => {
            e.slot(f, 192, 64, |row| {
                let a = row.as_array_mut().ok_or("structural row not array")?;
                replace(&mut a[2], 3, u64::MAX - 1)
            })?;
            e.slot(f, 640, 256, |row| {
                replace(cbor_key(row, 3), 3, u64::MAX - 1)
            })?;
            e.replica_payload(f, true)?;
            Ok(Role::Replica)
        }
        "overflow-8.3-component-starts" | "overflow-8.3-eod" => {
            for b in [0, 2] {
                if id == "overflow-8.3-component-starts" {
                    e.number(f, b, 0x158, 2, u64::MAX - 1, 8)?;
                    e.number(f, b, 0x90, 2, u64::MAX - 1, 8)?;
                } else {
                    for i in 0..5 {
                        e.number(
                            f,
                            b,
                            0x158 + 32 * i,
                            2 + 4 * i as u64,
                            u64::MAX - 19 + 4 * i as u64,
                            8,
                        )?;
                    }
                    e.number(f, b, 0x90, 2, u64::MAX - 19, 8)?;
                    e.number(f, b, 0xa0, 22, 0, 8)?;
                }
            }
            e.layout_digest(f);
            e.replica_repair(f, true);
            Ok(Role::Replica)
        }
        "overflow-10.4-slot-product" | "overflow-10.4-record-geometry" => {
            let (s, o) = if id == "overflow-10.4-record-geometry" {
                ((1 << 58) - 1, 0)
            } else {
                match v["variant"].as_str().unwrap() {
                    "a-64s" => (1 << 58, 0),
                    "b-256o" => (1, 1 << 56),
                    "c-sum" => (1 << 57, 1 << 55),
                    _ => return Err("unknown counts variant".into()),
                }
            };
            for b in [0, 2] {
                e.number(f, b, 0x60, 1, s, 8)?;
                e.number(f, b, 0x68, 0, o, 8)?;
            }
            e.replica_repair(f, true);
            Ok(Role::Replica)
        }
        "overflow-10.3-manifest-range" => {
            let (start, count) = if v["variant"] == "a-range-sum" {
                (u64::MAX, 1)
            } else {
                (0, 1 << 46)
            };
            e.slot(f, 384, 256, |row| {
                replace(cbor_key(row, 10), 0, start)?;
                replace(cbor_key(row, 12), 1, count)
            })?;
            e.replica_payload(f, false)?;
            Ok(Role::Replica)
        }
        "overflow-10.4-footer-delta" => {
            match v["variant"].as_str().unwrap() {
                "a-count-zero" => e.number(f, 2, 0x2e8, 3, 0, 8)?,
                "b-start-max" => e.number(f, 2, 0x2e0, 2, u64::MAX, 8)?,
                "c-separation-count-zero" => {
                    *targets = vec![f + 1];
                    e.number(f + 1, 2, 0x1b0, 3, 0, 8)?;
                    e.crc(f + 1, 2, 0x1f8, "separation footer CRC");
                    return Ok(Role::Separation);
                }
                _ => return Err("unknown footer variant".into()),
            }
            e.crc(f, 2, 0x3f8, "R-TR-FOOTER-LOCAL CRC");
            Ok(Role::Replica)
        }
        "overflow-10.5-extent-geometry" | "overflow-10.5-footer-offset-interior" => {
            *targets = vec![f + 1];
            let extent = if id == "overflow-10.5-extent-geometry" {
                u64::MAX
            } else {
                v["E"].as_u64().ok_or("missing E")?
            };
            for b in [0, 2] {
                e.number(f + 1, b, 0x50, 3 * BLOCK as u64, extent, 8)?;
            }
            e.separation_repair(f + 1);
            Ok(Role::Separation)
        }
        "overflow-3.2-lba" => {
            // block_count(4) = 2^64 − 1 in structural slot 4 of A, B and C, so
            // the three agree; R-TR-PAYLOAD repairs each, the canonical-map
            // SHA-256 included.
            *targets = vec![f, f + 2, f + 4];
            for &file in targets.iter() {
                e.slot(file, 4 * 64, 64, |row| {
                    let a = row.as_array_mut().ok_or("structural row not array")?;
                    replace(&mut a[2], 5, u64::MAX)
                })?;
                e.replica_payload(file, true)?;
            }
            Ok(Role::Replica)
        }
        // A unit-level descriptor: no bytes change (see `inverse`).
        "overflow-3.3-stripe-mapping-inverse" => Ok(Role::InverseMapping),
        // Device and commit-record claims: no tape byte changes (see
        // `injection`, `walk_scanner` and `resumer`).
        "overflow-12.2-walk-length" => {
            *targets = vec![1];
            Ok(Role::WalkScanner)
        }
        "overflow-3.2-14-append-point" => {
            *targets = vec![3];
            Ok(Role::Resumer)
        }
        "overflow-13.3-tail-location" => {
            let len = u64_at(&e.files[&f][0], 0x30) as usize;
            if e.files[&f][0][0xc8..0xc8 + len] != e.files[&f][1][0xc8..0xc8 + len] {
                return Err("ParityMap copies differ".into());
            }
            let mut p: Cbor = ciborium::from_reader(&e.files[&f][0][0xc8..0xc8 + len])
                .map_err(|e| e.to_string())?;
            let entries = cbor_key(cbor_key(&mut p, 4), 5)
                .as_array_mut()
                .ok_or("directory entries")?;
            if entries.len() != 1 {
                return Err("expected one directory entry".into());
            }
            let row = &mut entries[0];
            replace(cbor_key(row, 1), 2, 2)?;
            match v["variant"].as_str().unwrap() {
                "a-H-seven" => replace(cbor_key(row, 6), 1, 7)?,
                "b-H-max" => replace(cbor_key(row, 6), 1, u64::MAX)?,
                "c-total-zero" => replace(cbor_key(row, 5), 7, 0)?,
                _ => return Err("unknown directory variant".into()),
            }
            e.parity_payload(f, &p, len);
            for (b, o) in [(6, 0x80), (0, 0xc0)] {
                let byte = e.files[&2][b][o] ^ 1;
                e.write(
                    2,
                    b,
                    o,
                    &[byte],
                    "precondition: corrupt sidecar CRC; no repair",
                );
            }
            Ok(Role::Recovery)
        }
        _ => Err(format!("no overflow resolver for {id}")),
    }
}

pub(super) fn recover(v: &Resolved, _: usize, _: usize) -> Result<(), ObservedError> {
    let base = generate("a4-minimal").expect("base image");
    let map = ScopedFilemarkMap::from_catalog(base.written.map, 4);
    let mut raw = ImageDirectoryRawSource::from_tape_files(
        v.files.values().map(|b| b.concat()).collect(),
        BLOCK,
    )?;
    recover_ordinal_from_sidecar(
        &mut raw,
        &map,
        &base.written.inputs.scheme,
        v.uuid,
        BLOCK,
        0,
    )?;
    Ok(())
}
/// The Section 3.3 inverse at unit level, through the one helper every
/// Recoverer caller uses: start = 2^64 − 2, end = 2^64 − 1, S = k = 2. Each
/// peer position named by the case is classified; a real shard at any of them
/// would be a wrapped ordinal, and an error a rejection.
pub(super) fn inverse(_: &Resolved, _: usize, _: usize) -> Result<(), ObservedError> {
    let scheme = crate::tape_image_vectors::inputs("a4-minimal").scheme;
    for (stripe_index, index) in [(0, 1), (1, 0), (1, 1)] {
        match remanence_parity::mapping::stripe_data_shard_in_epoch(
            &StripeAddress {
                neighborhood: 0,
                stripe_index,
                position: StripePosition::Data { index },
            },
            u64::MAX - 1,
            u64::MAX,
            &scheme,
        )? {
            remanence_parity::mapping::EpochDataShard::ImplicitZero => {}
            remanence_parity::mapping::EpochDataShard::Real { ordinal } => {
                return Err(ObservedError::FormulaValue(ordinal));
            }
        }
    }
    Ok(())
}

/// What a device-report or commit-record case injects, for its descriptor.
pub(super) fn injection(id: &str) -> Result<Option<Value>, String> {
    Ok(match id {
        "overflow-12.2-walk-length" => Some(json!({
            "device_report": "the SPACE over one filemark that measures tape file 1 (issued after its head record, at LBA 3) reports one filemark spaced and a post-space position equal to the file's start, LBA 2: a position delta of 0; no tape byte changes",
            "tape_file": 1,
            "file_start_lba": WALK_STALL_LBA,
            "space_issued_at_lba": WALK_STALL_LBA + 1,
            "reported_position_after_lba": WALK_STALL_LBA,
        })),
        "overflow-3.2-14-append-point" => Some(json!({
            "commit_record_inputs": hostile_append_point_inputs()?,
            "framing": "the claimed T = 4 + (2^64 − 1) does not fit the reference journal's u64 watermark field, so the watermarks keep the last claims that fit (T = W = 4, the end of every claim before tape file 3); tape file 3's record keeps its claimed block count, and every row keeps its measured start, so the adapter does no arithmetic on the claimed count. The §14 step-2 bound T − W < S × k therefore passes, and the refusal is the commit record's own",
        })),
        _ => None,
    })
}

/// The start of tape file 1 in every finalized image.
const WALK_STALL_LBA: u64 = 2;

/// The committed prefix of unfinalized-open (files 0..3) whose record for
/// tape file 3, the second Object, claims block_count 2^64 − 1.
fn hostile_append_point_inputs() -> Result<Value, String> {
    let base = generate("unfinalized-open")?;
    let mut input = crate::resume_vectors::portable_input("resume-open", &base);
    for row in input["committed_prefix"].as_array_mut().unwrap() {
        let f = row["tape_file_number"].as_u64().unwrap() as usize;
        row["physical_start_override"] = json!(base.image.files[f].start_record);
    }
    if input["committed_prefix"][3]["kind"] != "Object" {
        return Err("tape file 3 of unfinalized-open is not an Object".into());
    }
    input["committed_prefix"][3]["block_count"] = json!(u64::MAX);
    input["T"] = json!(4);
    input.as_object_mut().unwrap().remove("append_object");
    Ok(input)
}

/// Terminal discovery with no off-tape state over the three mutated replicas
/// at their planned positions. The prefix files are present with their
/// planned record counts, as zero-filled records: discovery reads none of them.
pub(super) fn terminal_scanner(v: &Resolved, _: usize, _: usize) -> Result<(), ObservedError> {
    let profile = v.descriptor["artifact"]
        .as_str()
        .and_then(|a| a.strip_prefix("terminal profile "))
        .expect("a terminal profile case");
    let prefix = super::profiles::prefix_block_counts(profile).expect("profile prefix");
    let mut tape_files: Vec<Vec<u8>> = prefix.iter().map(|&n| vec![0; n as usize * B]).collect();
    for (&file, blocks) in &v.files {
        assert_eq!(
            file,
            tape_files.len(),
            "terminal files follow the prefix densely"
        );
        tape_files.push(blocks.concat());
    }
    let mut raw = ImageDirectoryRawSource::from_tape_files(tape_files, BLOCK)?;
    match read_terminal_index_inventory(&mut raw, &v.uuid, BLOCK, |_| Ok(()), |_| Ok(())) {
        Ok(TerminalInventoryOutcome::Inventory(_)) => Ok(()),
        Ok(TerminalInventoryOutcome::BotStructuralRecoveryRequired(_)) => {
            Err(ObservedError::BotStructuralRecoveryRequired)
        }
        Err(TerminalInventoryReadError::TerminalIndexReplicaConflict { .. }) => Err(
            ObservedError::TerminalInventory("TerminalIndexReplicaConflict".into()),
        ),
        Err(error) => Err(ObservedError::TerminalInventory(format!("{error:?}"))),
    }
}

/// A raw source whose SPACE over one filemark, issued just after the head
/// record of the file starting at `stall_lba`, reports the file's start as the
/// post-space position: a position delta of 0 (REM-PARITY 12.2).
struct StalledSpace {
    inner: ImageDirectoryRawSource,
    stall_lba: Option<u64>,
}
impl RawTapeSource for StalledSpace {
    fn configure_fixed_block_size(&mut self, size: u32) -> Result<(), ParityError> {
        self.inner.configure_fixed_block_size(size)
    }
    fn locate_physical(&mut self, hint: PhysicalPositionHint) -> Result<(), ParityError> {
        self.inner.locate_physical(hint)
    }
    fn locate_end_of_data(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        self.inner.locate_end_of_data()
    }
    fn space_filemarks(&mut self, count: i64) -> Result<SpaceFilemarksOutcome, ParityError> {
        let before = self.inner.position()?;
        let outcome = self.inner.space_filemarks(count)?;
        if let Some(start) = self
            .stall_lba
            .filter(|start| count == 1 && before.lba == start + 1)
        {
            return Ok(SpaceFilemarksOutcome {
                filemarks_spaced: 1,
                position_after: PhysicalPositionHint {
                    lba: start,
                    partition: before.partition,
                },
                hit_end_of_data: outcome.hit_end_of_data,
            });
        }
        Ok(outcome)
    }
    fn read_record(&mut self, buf: &mut [u8]) -> Result<RawReadOutcome, ParityError> {
        self.inner.read_record(buf)
    }
    fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        self.inner.position()
    }
}

/// The BOT walk of Section 12.2 over the unchanged image, with the device
/// report injected when the case's claim is present.
pub(super) fn walk_scanner(v: &Resolved, _: usize, _: usize) -> Result<(), ObservedError> {
    let mut raw = StalledSpace {
        inner: ImageDirectoryRawSource::from_tape_files(
            v.files.values().map(|b| b.concat()).collect(),
            BLOCK,
        )?,
        stall_lba: v.injected.then_some(WALK_STALL_LBA),
    };
    scan_reconstruct_filemark_map_with_report(&mut raw, &v.uuid, BLOCK)?;
    Ok(())
}

/// The Resumer from commit records: journal replay, the bounded summary and
/// the open-epoch re-read over the unchanged tape. With the claim present the
/// Resumer must refuse before any read; the healthy prefix resumes.
pub(super) fn resumer(v: &Resolved, _: usize, _: usize) -> Result<(), ObservedError> {
    let base = generate("unfinalized-open").expect("base image");
    let input = if v.injected {
        v.descriptor["injection"]["commit_record_inputs"].clone()
    } else {
        let mut healthy = crate::resume_vectors::portable_input("resume-open", &base);
        healthy.as_object_mut().unwrap().remove("append_object");
        healthy
    };
    let temp = tempfile::tempdir().expect("journal temp directory");
    let (mut drive, world, _) = crate::resume_vectors::model(&base, &input);
    let result = (|| -> Result<(), ParityError> {
        let journal = crate::resume_vectors::adapt(&input, &base, &temp.path().join("claims"))?;
        let snapshot = resume_record_result(journal.committed_snapshot_bounded())?;
        let summary = checked_bounded_resume_summary(&snapshot)?;
        rebuild_open_epoch_from_bounded_summary(
            &mut DriveHandleRawSource::new(&mut drive),
            &summary,
            &base.written.inputs.scheme,
            base.written.inputs.tape_uuid,
            BLOCK,
        )?;
        Ok(())
    })();
    if result.is_err() && v.injected {
        assert!(
            world.lock().unwrap().command_log.is_empty(),
            "a refused commit-record claim positioned, read or wrote tape"
        );
    }
    result?;
    Ok(())
}

pub(super) fn locator(v: &Resolved, f: usize, b: usize) -> Result<(), ObservedError> {
    let result = parity_block_position(0, 1, 2, 2, u64_at(&v.files[&f][b], 0x60))?;
    if u64_at(&v.files[&f][b], 0x60) == u64::MAX {
        return Err(ObservedError::FormulaValue(result));
    }
    Ok(())
}
