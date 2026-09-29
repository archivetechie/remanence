//! Review-only generation-2 negative vectors, groups 7a, 7b and 7c.
//! Expectations are external specification-authored data. Resolution checks the
//! original bytes, records ordered edits and integrity repairs, and never uses
//! parser outcomes to construct an expectation or repair a disagreement.
use crate::tape_image_vectors::{generate, hex, BLOCK};
use ciborium::value::Value as Cbor;
use remanence_parity::bootstrap::parse_bootstrap_block;
use remanence_parity::*;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::PathBuf,
};
pub mod erratum;
#[cfg(test)]
mod mutations;
mod overflow;
mod profiles;
pub mod supplement;
const B: usize = BLOCK as usize;

pub fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/rem-parity-terminal-index-draft")
}
/// Reject edits to the verbatim specification-authored source.
pub fn parse_source(bytes: &[u8]) -> Result<Value, String> {
    if hex(&Sha256::digest(bytes))
        != "e8336e36351a3da7c8c2c78a4aa15a40a0147892e7bbcb25ba94a3a5644cc92b"
    {
        return Err("frozen negative-cases.json SHA-256 differs".into());
    }
    serde_json::from_slice(bytes).map_err(|e| e.to_string())
}
pub fn source() -> Result<Value, String> {
    parse_source(
        &fs::read(fixture_root().join("tape-images/negatives/negative-cases.json"))
            .map_err(|e| e.to_string())?,
    )
}

/// Parse erratum set E1's negative vectors: each construction with the
/// expectation a text-only author wrote for it. Unlike the frozen sources this
/// file is not SHA-pinned; the executor fails a pending or unknown expectation.
pub fn parse_erratum_source(bytes: &[u8]) -> Result<Value, String> {
    let value: Value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    let cases = value["cases"].as_array().ok_or("erratum cases missing")?;
    let mut ids = BTreeSet::new();
    for case in cases {
        let id = case["id"].as_str().ok_or("erratum case id missing")?;
        if !ids.insert(id) {
            return Err(format!("duplicate erratum case {id}"));
        }
        if case["group"] != "e1" || !case["base"]["artifact"].is_string() {
            return Err(format!(
                "erratum case {id} lacks its group or base artifact"
            ));
        }
        if case["pinned"].as_bool().is_none() || case.get("expected").is_none() {
            return Err(format!("erratum case {id} lacks expected or pinned"));
        }
        if case["expected"] == "pending (E1)" && case["pinned"] != false {
            return Err(format!(
                "erratum case {id} is pinned without an expectation"
            ));
        }
    }
    Ok(value)
}

/// Read erratum set E1's negative constructions from the fixture tree.
pub fn erratum_source() -> Result<Value, String> {
    parse_erratum_source(
        &fs::read(fixture_root().join("tape-images/negatives/negative-cases-e1.json"))
            .map_err(|e| e.to_string())?,
    )
}

fn unhex(s: &str) -> Vec<u8> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(s.len().is_multiple_of(2));
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex bytes"))
        .collect()
}
fn u64_at(b: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(b[offset..offset + 8].try_into().unwrap())
}
fn cbor_key(v: &mut Cbor, key: i128) -> &mut Cbor {
    v.as_map_mut()
        .expect("CBOR map")
        .iter_mut()
        .find(|(k, _)| k.as_integer().map(i128::from) == Some(key))
        .map(|(_, v)| v)
        .expect("CBOR key")
}
fn cbor_bytes(v: &Cbor) -> Vec<u8> {
    let mut b = Vec::new();
    ciborium::into_writer(v, &mut b).unwrap();
    b
}

/// One resolved case contains full in-memory base files and an ordered edit log.
/// Only the descriptors and hashes are emitted; no duplicate image is stored.
pub struct Resolved {
    pub path: String,
    pub expected: Value,
    pub descriptor: Value,
    pub manifest: String,
    files: BTreeMap<usize, Vec<Vec<u8>>>,
    uuid: [u8; 16],
    roles: Vec<Role>,
    targets: Vec<usize>,
    block: usize,
    /// Whether the case's off-tape or device-report claim is injected. The
    /// healthy-base check clears it with the files, so every observation point
    /// must accept the base with no claim.
    injected: bool,
}
struct Editor {
    artifact: String,
    files: BTreeMap<usize, Vec<Vec<u8>>>,
    edits: Vec<Value>,
    changed: BTreeSet<(usize, usize)>,
}
impl Editor {
    fn write(&mut self, file: usize, block: usize, offset: usize, new: &[u8], repair: &str) {
        let old = self.files[&file][block][offset..offset + new.len()].to_vec();
        self.edits.push(json!({"artifact":self.artifact,"tape_file":file,"block_within_file":block,"byte_offset":offset,"old_bytes":hex(&old),"new_bytes":hex(new),"repair":repair}));
        self.files.get_mut(&file).unwrap()[block][offset..offset + new.len()].copy_from_slice(new);
        self.changed.insert((file, block));
    }
    fn crc(&mut self, file: usize, block: usize, offset: usize, repair: &str) {
        let crc = crc64_xz(&self.files[&file][block][..offset]);
        self.write(file, block, offset, &crc.to_le_bytes(), repair);
    }
    fn field(&mut self, file: usize, block: usize, m: &Value) -> Result<(), String> {
        let offset_text = m["offset"].as_str().ok_or("no numeric offset")?;
        let mut range = offset_text.split("..");
        let offset = usize::from_str_radix(range.next().unwrap().trim_start_matches("0x"), 16)
            .map_err(|e| e.to_string())?;
        let end = range
            .next()
            .map(|s| usize::from_str_radix(s.trim_start_matches("0x"), 16).unwrap());
        let old = &self.files[&file][block];
        let field = m["field"].as_str().unwrap_or("");
        let width = end.map(|e| e - offset).unwrap_or_else(|| {
            if let Some(s) = m["le_bytes"].as_str() {
                unhex(s).len()
            } else if field.contains("u16") {
                2
            } else if field.contains("u32") {
                4
            } else if field.contains("fill") {
                1
            } else {
                8
            }
        });
        let new = if m["change"]
            .as_str()
            .is_some_and(|s| s.starts_with("XOR 0x01"))
        {
            vec![old[offset] ^ 1]
        } else if let Some(n) = m["to"].as_u64() {
            n.to_le_bytes()[..width].to_vec()
        } else {
            match m["to"].as_str().ok_or("no resolved new value")? {
                "zero" => vec![0; width],
                "any u64 LE, e.g. 0x261BDF3D299838FC (crc64 of a zero block)" => {
                    0x261BDF3D299838FCu64.to_le_bytes().to_vec()
                }
                "L + 1" => (u64_at(old, offset) + 1).to_le_bytes().to_vec(),
                s if s
                    .split_whitespace()
                    .all(|word| word.len() == 2 && u8::from_str_radix(word, 16).is_ok()) =>
                {
                    unhex(s)
                }
                s => return Err(format!("unresolved to: {s}")),
            }
        };
        if let Some(s) = m["le_bytes"].as_str() {
            if new != unhex(s) {
                return Err("to and le_bytes disagree".into());
            }
        }
        if let Some(from) = m["from"].as_u64() {
            if old[offset..offset + new.len()] != from.to_le_bytes()[..new.len()] {
                return Err(format!(
                    "from mismatch at {offset_text}: expected {from}, bytes {}",
                    hex(&old[offset..offset + new.len()])
                ));
            }
        } else if let Some(from) = m["from"].as_str() {
            if from == "L (the header's payload_len)" {
                if u64_at(old, offset) != u64_at(&self.files[&file][0], offset) {
                    return Err("footer length differs from header".into());
                }
            } else if from != "the recorded payload length"
                && old[offset..offset + new.len()] != unhex(from)
            {
                return Err(format!("from mismatch at {offset_text}"));
            }
        }
        self.write(file, block, offset, &new, "mutation");
        Ok(())
    }
    fn sidecar_repair(&mut self, file: usize, block: usize, repairs: &str, stream_end: usize) {
        if repairs.contains("R-SC-HASHED") || repairs.contains("over the header wire bytes") {
            let b = &self.files[&file][block];
            let mut hash = Sha256::new();
            hash.update(b"remanence-sidecar-metadata-v1");
            hash.update(&b[..0x90]);
            hash.update(&b[0xc8..stream_end]);
            self.write(
                file,
                block,
                0x98,
                &hash.finalize(),
                "R-SC-HASHED canonical_metadata_hash (wire fields and actual entries)",
            );
        }
        if repairs.contains("R-SC-FOOTER") {
            self.crc(file, block, 0x80, "R-SC-FOOTER footer_crc64");
        } else if !repairs.contains("none:") {
            if !repairs.contains("R-SC-FILL") {
                self.crc(file, block, 0xc0, "header_crc64");
            }
            self.crc(file, block, B - 8, "block0_crc64");
        }
    }
    fn replica_repair(&mut self, file: usize, common: bool) {
        if common {
            let b = &self.files[&file][0];
            let mut h = Sha256::new();
            h.update(b"REM-TAPE-INDEX-EDITION-V1\0");
            for r in [0x8..0xa, 0x10..0x38, 0x3c..0x80, 0xa8..0xe8] {
                h.update(&b[r]);
            }
            for (length, offset) in [(0x1f0, 0x1f8), (0x1f2, 0x278)] {
                let n = u16::from_le_bytes(b[length..length + 2].try_into().unwrap()) as usize;
                h.update((n as u64).to_le_bytes());
                h.update(&b[offset..offset + n]);
            }
            let digest = h.finalize();
            for block in [0, 2] {
                self.write(file, block, 0xe8, &digest, "R-TR-COMMON edition digest");
            }
            let b = &self.files[&file][0];
            let ordinal = u16::from_le_bytes(b[0x38..0x3a].try_into().unwrap()) as usize;
            let component = 0x148 + (ordinal - 1) * 2 * 32;
            let mut h = Sha256::new();
            h.update(b"REM-TAPE-INDEX-REPLICA-DESCRIPTOR-V1\0");
            h.update(&b[0xe8..0x128]);
            h.update(&b[0x38..0x3c]);
            h.update(&b[component..component + 32]);
            h.update(&b[0x98..0xa0]);
            let digest = h.finalize();
            for block in [0, 2] {
                self.write(file, block, 0x128, &digest, "R-TR-COMMON descriptor digest");
            }
        }
        self.crc(file, 0, 0x3f8, "replica header CRC");
        let digest = Sha256::digest(&self.files[&file][0]);
        self.write(file, 2, 0x2b8, &digest, "footer header-record SHA-256");
        self.crc(file, 2, 0x3f8, "replica footer CRC");
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Role {
    SidecarCopy,
    SidecarFooter,
    /// Each of a sidecar's two header/index copies, on its own (Sections 9.2
    /// to 9.5).
    SidecarCopies,
    /// A Verifier's full verification of the sidecar (Sections 2.2, 9.1).
    Verifier,
    ParityMap,
    Directory,
    Bootstrap,
    Replica,
    Separation,
    Recovery,
    InverseMapping,
    ParityLocator,
    /// Terminal discovery with no off-tape state (REM-PARITY 8.4), run once.
    TerminalScanner,
    /// The Section 12.2 BOT walk, run once.
    WalkScanner,
    /// The Resumer from commit records (Section 14), run once.
    Resumer,
}
struct RoleEntry {
    role: Role,
    entry: &'static str,
    run: fn(&Resolved, usize, usize) -> Result<(), ObservedError>,
}
#[derive(Debug)]
enum ObservedError {
    Parity(ParityError),
    FormulaValue(u64),
    /// The Scanner's explicit outcome when no replica validates (REM-PARITY 8.4).
    BotStructuralRecoveryRequired,
    /// A terminal inventory read failed outside Section 15's replica names.
    TerminalInventory(String),
    Replica(TapeIndexReplicaError),
    Separation(IndexSeparationError),
    #[cfg(test)]
    ScannerReplica(remanence_parity::TerminalReplicaFailure),
}
impl std::fmt::Display for ObservedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FormulaValue(n) => write!(f, "formula returned {n} instead of rejecting"),
            Self::BotStructuralRecoveryRequired => write!(f, "BotStructuralRecoveryRequired"),
            Self::TerminalInventory(e) => write!(f, "{e}"),
            Self::Parity(e) => write!(f, "{e}"),
            Self::Replica(e) => write!(f, "{e}"),
            Self::Separation(e) => write!(f, "{e}"),
            #[cfg(test)]
            Self::ScannerReplica(e) => write!(f, "{:?}: {}", e.kind, e.detail),
        }
    }
}
/// One production observation, including a stable adjudication key and outcome.
pub struct Observation {
    pub location: String,
    pub key: &'static str,
    pub name: &'static str,
    pub detail: String,
    pub outcome: String,
}

/// Run the production role for every selected copy and retain its typed name.
pub fn observe(v: &Resolved) -> Vec<Observation> {
    let mut observations = Vec::new();
    for role in &v.roles {
        let entry = ROLES.iter().find(|r| r.role == *role).unwrap();
        let once = matches!(
            role,
            Role::TerminalScanner | Role::WalkScanner | Role::Resumer
        );
        for &file in v.targets.iter().take(if once { 1 } else { usize::MAX }) {
            let blocks = if *role == Role::Directory {
                vec![0, 1]
            } else {
                vec![v.block]
            };
            for block in blocks {
                let location = format!("file={file} role={role:?} block={block}");
                let key = match role {
                    Role::Recovery => "recovery",
                    Role::Directory if block == 0 => "directory primary",
                    Role::Directory => "directory tail",
                    Role::ParityMap => "ParityMap",
                    Role::ParityLocator | Role::InverseMapping => "unit",
                    Role::SidecarCopy => "header",
                    Role::SidecarFooter => "footer",
                    Role::SidecarCopies => "copies",
                    Role::Verifier => "verifier",
                    Role::Bootstrap => "bootstrap",
                    Role::Replica => "replica",
                    Role::Separation => "separation",
                    Role::TerminalScanner => "scanner",
                    Role::WalkScanner => "walk",
                    Role::Resumer => "resumer",
                };
                let (name, detail, outcome) =
                    match std::panic::catch_unwind(|| (entry.run)(v, file, block)) {
                        Err(e) => (
                            "PANIC",
                            e.downcast_ref::<String>()
                                .cloned()
                                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                                .unwrap_or_else(|| "non-string panic".into()),
                            "PANIC".into(),
                        ),
                        Ok(Ok(())) => ("ACCEPTED", "Ok(())".into(), "ACCEPTED".into()),
                        Ok(Err(e)) => {
                            let outcome = match &e {
                                ObservedError::Parity(
                                    ParityError::SidecarMetadataUnavailable { epoch_id },
                                ) => format!("SidecarMetadataUnavailable{{epoch_id: {epoch_id}}}"),
                                _ => section15(&e).to_string(),
                            };
                            (section15(&e), e.to_string(), outcome)
                        }
                    };
                observations.push(Observation {
                    location,
                    key,
                    name,
                    detail,
                    outcome,
                });
            }
        }
    }
    observations
}

impl From<ParityError> for ObservedError {
    fn from(e: ParityError) -> Self {
        Self::Parity(e)
    }
}
impl From<TapeIndexReplicaError> for ObservedError {
    fn from(e: TapeIndexReplicaError) -> Self {
        Self::Replica(e)
    }
}
impl From<IndexSeparationError> for ObservedError {
    fn from(e: IndexSeparationError) -> Self {
        Self::Separation(e)
    }
}
struct Payload<'a>(&'a [Vec<u8>]);
impl TapeIndexReplicaPayloadBlockSource for Payload<'_> {
    fn visit_payload_blocks(
        &mut self,
        visitor: &mut dyn FnMut(&[u8]) -> Result<(), TapeIndexReplicaError>,
    ) -> Result<(), TapeIndexReplicaError> {
        for block in self.0 {
            visitor(block)?;
        }
        Ok(())
    }
}
impl IndexSeparationInteriorBlockSource for Payload<'_> {
    fn visit_interior_blocks(
        &mut self,
        visitor: &mut dyn FnMut(&[u8]) -> Result<(), IndexSeparationError>,
    ) -> Result<(), IndexSeparationError> {
        for block in self.0 {
            visitor(block)?;
        }
        Ok(())
    }
}
// The dispatch table is the single target-role to production-entry-point map.
const ROLES: &[RoleEntry] = &[
    RoleEntry {role:Role::Recovery, entry:"recover_ordinal_from_sidecar (the map from the validated terminal replicas' structural rows)", run:overflow::recover},
    RoleEntry {role:Role::InverseMapping, entry:"mapping::stripe_data_shard_in_epoch (unit level; the one inverse every Recoverer caller uses)", run:overflow::inverse},
    RoleEntry {role:Role::TerminalScanner, entry:"read_terminal_index_inventory (no off-tape state; prefix records present, their bytes unread)", run:overflow::terminal_scanner},
    RoleEntry {role:Role::WalkScanner, entry:"scan_reconstruct_filemark_map_with_report (device report injected by the raw source)", run:overflow::walk_scanner},
    RoleEntry {role:Role::Resumer, entry:"FileTapeFileJournal replay -> checked_bounded_resume_summary -> rebuild_open_epoch_from_bounded_summary", run:overflow::resumer},
    RoleEntry {role:Role::ParityLocator, entry:"parity_block_position (unit-level formula probe after header rejection)", run:overflow::locator},
    RoleEntry {role:Role::SidecarCopy, entry:"parse_sidecar_index_blocks (includes parse_sidecar_header_block)", run:|v,f,b| { parse_sidecar_index_blocks(&v.files[&f][b..b+1],&v.uuid)?; Ok(()) }},
    RoleEntry {role:Role::SidecarFooter, entry:"parse_sidecar_footer_block", run:|v,f,b| { parse_sidecar_footer_block(&v.files[&f][b],&v.uuid)?; Ok(()) }},
    RoleEntry {role:Role::SidecarCopies, entry:"parse_sidecar_index_blocks (the primary copy, then the tail copy, each on its own)", run:|v,f,_| {
        let blocks=&v.files[&f];let h=u64_at(&blocks[0],0x60) as usize;
        parse_sidecar_index_blocks(&blocks[..h],&v.uuid)?;
        parse_sidecar_index_blocks(&blocks[blocks.len()-1-h..blocks.len()-1],&v.uuid)?; Ok(())
    }},
    RoleEntry {role:Role::Verifier, entry:"verify_sidecar (a Verifier's full verification: footer, both copies, the acquired index, every data block and parity shard)", run:overflow::verifier},
    RoleEntry {role:Role::ParityMap, entry:"parse_parity_map_tape_file -> parse_copy_at -> decode_copy -> decode_parity_map_payload -> decode_sidecar_epoch_directory_cbor", run:|v,f,_| { parse_parity_map_tape_file(&v.files[&f],&v.uuid)?; Ok(()) }},
    RoleEntry {role:Role::Directory, entry:"decode_sidecar_epoch_directory_cbor (each copy's payload key 4)", run:|v,f,b| {
        let block=&v.files[&f][b]; let len=u64_at(block,0x30) as usize;
        let mut payload:Cbor=ciborium::from_reader(&block[0xc8..0xc8+len]).expect("resolved CBOR payload");
        parity_map::decode_sidecar_epoch_directory_cbor(cbor_key(&mut payload,4).clone())?; Ok(())
    }},
    RoleEntry {role:Role::Bootstrap, entry:"parse_bootstrap_block", run:|v,f,_| { parse_bootstrap_block(&v.files[&f][0])?; Ok(()) }},
    RoleEntry {role:Role::Replica, entry:"parse_tape_index_replica_header + parse_tape_index_bootstrap_footer + validate_tape_index_replica_payload", run:|v,f,_| {
        let blocks=&v.files[&f];let h=parse_tape_index_replica_header(&blocks[0],&v.uuid)?;let t=parse_tape_index_bootstrap_footer(blocks.last().expect("replica footer record"),&v.uuid)?;
        validate_tape_index_replica_payload(&h,&t,&mut Payload(&blocks[1..blocks.len()-1]), |_|Ok(()), |_|Ok(()))?;Ok(())
    }},
    RoleEntry {role:Role::Separation, entry:"parse_index_separation_header + parse_index_separation_footer + validate_index_separation_pair", run:|v,f,_| {
        let blocks=&v.files[&f];let h=parse_index_separation_header(&blocks[0],&v.uuid)?;let t=parse_index_separation_footer(blocks.last().expect("separation footer record"),&v.uuid)?;validate_index_separation_full(&h,&t,&mut Payload(&blocks[1..blocks.len()-1]))?; Ok(())
    }},
];
/// Map only actual typed errors, never the case's expectation, to §15 names.
fn section15(error: &ObservedError) -> &'static str {
    match error {
        #[cfg(test)]
        ObservedError::ScannerReplica(remanence_parity::TerminalReplicaFailure {
            kind: remanence_parity::TerminalReplicaFailureKind::TrailingFilemark,
            ..
        }) => "TerminalIndexReplicaParse",
        ObservedError::Parity(ParityError::SidecarParse(_)) => "SidecarParse",
        ObservedError::Parity(ParityError::ParityMapParse(_)) => "ParityMapParse",
        ObservedError::Parity(ParityError::DirectoryInvalid(_)) => "DirectoryInvalid",
        ObservedError::Parity(ParityError::BootstrapParse(_))
        | ObservedError::Parity(ParityError::BootstrapRefused { .. }) => "BootstrapParse",
        ObservedError::Parity(ParityError::DriveCompressionEnabled) => "DriveCompressionEnabled",
        ObservedError::Parity(ParityError::BootstrapPayloadTooLarge { .. }) => {
            "BootstrapPayloadTooLarge"
        }
        ObservedError::FormulaValue(_) => "RETURNED",
        ObservedError::BotStructuralRecoveryRequired => "BotStructuralRecoveryRequired",
        ObservedError::TerminalInventory(_) => "UNMAPPED",
        ObservedError::Parity(ParityError::TapeIo(_)) => "TapeIo",
        ObservedError::Parity(ParityError::Invariant(_)) => "Invariant",
        ObservedError::Parity(ParityError::SidecarMetadataUnavailable { .. }) => {
            "SidecarMetadataUnavailable"
        }
        ObservedError::Parity(ParityError::ReplicatedControlLayout { .. }) => {
            "ReplicatedControlLayout"
        }
        ObservedError::Replica(TapeIndexReplicaError::DigestMismatch {
            field: "canonical map SHA-256",
        }) => "FilemarkMapDigestMismatch",
        ObservedError::Replica(TapeIndexReplicaError::CompressionEnabled)
        | ObservedError::Separation(IndexSeparationError::CompressionEnabled) => {
            "DriveCompressionEnabled"
        }
        ObservedError::Replica(TapeIndexReplicaError::Emit { .. })
        | ObservedError::Separation(IndexSeparationError::Emit { .. }) => "UNMAPPED",
        ObservedError::Separation(IndexSeparationError::PhysicalSource(_)) => "UNMAPPED",
        ObservedError::Parity(ParityError::FilemarkMapReconstruct(_)) => "FilemarkMapReconstruct",
        ObservedError::Parity(ParityError::ResumeAppend(_)) => "ResumeAppend",
        ObservedError::Parity(ParityError::SchemeMismatch { .. }) => "SchemeMismatch",
        ObservedError::Replica(_) => "TerminalIndexReplicaParse",
        ObservedError::Separation(_) => "TerminalIndexSeparationParse",
        _ => "UNMAPPED",
    }
}

/// Supervisor-owned exceptions for target checks masked by reference check order.
#[derive(Debug)]
pub struct IsolationExceptions {
    entries: BTreeSet<(String, String)>,
}

/// Parse exact variant/observation matches, rejecting overlapping wildcard waivers.
pub fn parse_isolation_exceptions(bytes: &[u8]) -> Result<IsolationExceptions, String> {
    let value: Value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    let mut exceptions = IsolationExceptions {
        entries: BTreeSet::new(),
    };
    for entry in value["entries"]
        .as_array()
        .ok_or("isolation entries missing")?
    {
        let variant = entry["variant"]
            .as_str()
            .filter(|s| s.contains('/'))
            .ok_or("isolation variant missing or malformed")?
            .to_string();
        let observation = entry["observation"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("isolation observation missing or empty")?
            .to_string();
        if exceptions
            .entries
            .iter()
            .any(|(v, o)| v == &variant && (o == &observation || o == "*" || observation == "*"))
        {
            return Err(format!(
                "overlapping isolation exception: {variant} [{observation}]"
            ));
        }
        exceptions.entries.insert((variant, observation));
    }
    Ok(exceptions)
}

#[cfg(test)]
impl IsolationExceptions {
    /// Each matching observation must still miss its target, even under a wildcard.
    fn check(
        &self,
        variant: &str,
        observation: &str,
        isolation: bool,
        seen: &mut BTreeSet<(String, String)>,
    ) -> Result<Option<&'static str>, String> {
        let matched = self
            .entries
            .iter()
            .find(|(v, o)| v == variant && (o == observation || o == "*"));
        match (matched, isolation) {
            (Some(key), true) => {
                seen.insert(key.clone());
                Ok(Some("ISOLATION-EXCEPTION"))
            }
            (Some(_), false) => Err(format!(
                "STALE-ISOLATION-EXCEPTION {variant} [{observation}]"
            )),
            (None, true) => Err(format!("ISOLATION? {variant} [{observation}] (unlisted)")),
            (None, false) => Ok(None),
        }
    }

    /// Removed variants or observations cannot leave an unused exception behind.
    fn ensure_all_observed(&self, seen: &BTreeSet<(String, String)>) -> Result<(), String> {
        if let Some(key) = self.entries.difference(seen).next() {
            return Err(format!("STALE-ISOLATION-EXCEPTION unobserved {key:?}"));
        }
        Ok(())
    }
}

/// Supervisor records: open cases and corrections scoped to one observation.
#[derive(Debug)]
pub struct Adjudications {
    /// Settled by the text as a permitted set: any one of the outcomes passes.
    pub pinned_sets: BTreeMap<(String, String, String), BTreeSet<String>>,
    pub open: BTreeMap<(String, String, String), BTreeSet<String>>,
    pub settled: BTreeMap<(String, String, String), String>,
}

impl Adjudications {
    #[cfg(test)]
    fn allows_open(&self, key: &(String, String, String), outcome: &str) -> bool {
        !matches!(outcome, "PANIC" | "Invariant")
            && self
                .open
                .get(key)
                .is_some_and(|allowed| allowed.contains(outcome))
    }
}

/// Load structured supervisor matches and outcomes; prose is explanatory only.
/// Unknown matches fail closed when the executor checks that every entry ran.
/// Malformed outcomes, duplicate matches and open/settled overlaps are rejected.
pub fn parse_adjudications(bytes: &[u8]) -> Result<Adjudications, String> {
    let value: Value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    let mut open = BTreeMap::new();
    let mut settled = BTreeMap::new();
    let mut pinned_sets = BTreeMap::new();
    for kind in ["open", "settled"] {
        for entry in value[kind]
            .as_array()
            .ok_or_else(|| format!("{kind} array missing"))?
        {
            let id = entry["id"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or("id missing or empty")?;
            let matched = entry["match"].as_object().ok_or("match object missing")?;
            if matched.len() != 2 {
                return Err(format!("unknown or missing match fields: {id}"));
            }
            let variant = entry["match"]["variant"]
                .as_str()
                .ok_or("match variant missing")?;
            let observation = entry["match"]["observation"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or("match observation missing or empty")?;
            let key = (id.to_string(), variant.to_string(), observation.to_string());
            if open.contains_key(&key)
                || settled.contains_key(&key)
                || pinned_sets.contains_key(&key)
            {
                return Err(format!(
                    "overlapping or duplicate adjudication: {id}/{variant} [{observation}]"
                ));
            }
            if kind == "open" {
                if entry.get("corrected").is_some() {
                    return Err(format!("open entry has a corrected outcome: {id}"));
                }
                let outcomes = entry["disputed_outcomes"]
                    .as_array()
                    .filter(|a| !a.is_empty())
                    .ok_or("disputed_outcomes missing or empty")?;
                let mut allowed = BTreeSet::new();
                for outcome in outcomes {
                    let outcome = adjudication_outcome(outcome)?;
                    if !allowed.insert(outcome.to_string()) {
                        return Err(format!("duplicate disputed outcome: {id}: {outcome}"));
                    }
                }
                open.insert(key, allowed);
            } else {
                if entry.get("disputed_outcomes").is_some() {
                    return Err(format!("settled entry has disputed outcomes: {id}"));
                }
                if let Some(permitted) = entry.get("permitted_outcomes") {
                    // The text names a category and leaves the choice among its
                    // names to the Reader: the set is pinned, and any member passes.
                    if entry.get("corrected").is_some() {
                        return Err(format!("settled entry with both forms: {id}"));
                    }
                    let mut allowed = BTreeSet::new();
                    for outcome in permitted
                        .as_array()
                        .filter(|a| a.len() >= 2)
                        .ok_or("permitted_outcomes needs at least two outcomes")?
                    {
                        allowed.insert(adjudication_outcome(outcome)?.to_string());
                    }
                    pinned_sets.insert(key, allowed);
                } else {
                    settled.insert(key, adjudication_outcome(&entry["corrected"])?.to_string());
                }
            }
        }
    }
    Ok(Adjudications {
        pinned_sets,
        open,
        settled,
    })
}

/// Accept only outcomes the executor can identify, never panic/unmapped waivers.
fn adjudication_outcome(value: &Value) -> Result<&str, String> {
    let outcome = value.as_str().ok_or("outcome must be a string")?;
    let metadata_epoch = outcome
        .strip_prefix("SidecarMetadataUnavailable{epoch_id: ")
        .and_then(|s| s.strip_suffix('}'))
        .and_then(|s| s.parse::<u64>().ok())
        .is_some_and(|epoch| outcome == format!("SidecarMetadataUnavailable{{epoch_id: {epoch}}}"));
    if metadata_epoch
        || matches!(
            outcome,
            "ACCEPTED"
                | "RETURNED"
                | "SidecarParse"
                | "ParityMapParse"
                | "DirectoryInvalid"
                | "BootstrapParse"
                | "BootstrapPayloadTooLarge"
                | "SchemeMismatch"
                | "FilemarkMapReconstruct"
                | "ResumeAppend"
                | "ReplicatedControlLayout"
                | "FilemarkMapDigestMismatch"
                | "DriveCompressionEnabled"
                | "TerminalIndexReplicaParse"
                | "TerminalIndexSeparationParse"
        )
    {
        Ok(outcome)
    } else {
        Err(format!(
            "unknown or forbidden adjudication outcome: {outcome}"
        ))
    }
}

#[cfg(test)]
impl Adjudications {
    /// Apply the same scoped adjudication rules to frozen and supplement observations.
    /// Return true when handled, including every panic (which always fails).
    fn check_observation(
        &self,
        key: (String, String, String),
        path: &str,
        observation: &Observation,
        expected: &Value,
        seen: &mut BTreeSet<(String, String, String)>,
        failures: &mut Vec<String>,
    ) -> bool {
        let corrected = self.settled.get(&key);
        let disputed = self.open.get(&key);
        let pinned = self.pinned_sets.get(&key);
        if corrected.is_some() || disputed.is_some() || pinned.is_some() {
            seen.insert(key.clone());
        }
        let (status, failed, expected) = if matches!(observation.name, "PANIC" | "Invariant")
            || matches!(observation.outcome.as_str(), "PANIC" | "Invariant")
        {
            (
                "DISAGREEMENT",
                true,
                "non-PANIC, non-Invariant outcome".to_string(),
            )
        } else if let Some(corrected) = corrected {
            let failed = observation.outcome != *corrected;
            (
                if failed { "DISAGREEMENT" } else { "SETTLED" },
                failed,
                corrected.clone(),
            )
        } else if let Some(pinned) = pinned {
            let failed = !pinned.contains(&observation.outcome);
            (
                if failed { "DISAGREEMENT" } else { "PINNED-SET" },
                failed,
                format!("{pinned:?}"),
            )
        } else if let Some(disputed) = disputed {
            let failed = !self.allows_open(&key, &observation.outcome);
            (
                self.open_status(&key, observation, expected),
                failed,
                format!("{disputed:?}"),
            )
        } else {
            return false;
        };
        println!(
            "{status} {path} [{}]: expected {expected}; observed {}; {}; detail={:?}",
            observation.key, observation.outcome, observation.location, observation.detail
        );
        if failed {
            failures.push(format!("{path} [{}]: {status}", observation.key));
        }
        true
    }

    /// Signal when an open question can be closed because the source now agrees.
    fn open_status(
        &self,
        key: &(String, String, String),
        observation: &Observation,
        expected: &Value,
    ) -> &'static str {
        if !self.allows_open(key, &observation.outcome) {
            "DISAGREEMENT"
        } else if supplement::expected_matches(expected, observation) {
            "DISPUTED-NOW-AGREES"
        } else {
            "DISPUTED"
        }
    }

    /// An unknown case, variant or observation cannot become a silent waiver.
    fn ensure_all_observed(&self, seen: &BTreeSet<(String, String, String)>) -> Result<(), String> {
        for key in self
            .open
            .keys()
            .chain(self.settled.keys())
            .chain(self.pinned_sets.keys())
        {
            if !seen.contains(key) {
                return Err(format!("unknown or unexecuted adjudication match: {key:?}"));
            }
        }
        Ok(())
    }
}

/// Reasons are copied from the case, including off-tape and freeze blockers.
pub fn not_executable(case: &Value) -> Option<&str> {
    if case["group"] != "7c" {
        return None;
    }
    if case["freeze_blocker"] == true {
        Some(case["blocker_reason"].as_str().expect("blocker reason"))
    } else if case["hostile_source"] != true {
        Some(
            case["hostile_source"]
                .as_str()
                .unwrap_or_else(|| case["evaluation"].as_str().expect("non-executable reason")),
        )
    } else {
        None
    }
}

/// Enumerate only authorized groups, preserving variant objects exactly.
pub fn cases(source: &Value) -> Vec<(&Value, Option<&Value>)> {
    let mut result = Vec::new();
    for case in source["cases"]
        .as_array()
        .expect("case array")
        .iter()
        .filter(|c| matches!(c["group"].as_str(), Some("7a" | "7b" | "7c")))
    {
        if let Some(variants) = case["variants"].as_array() {
            for variant in variants {
                result.push((case, Some(variant)));
            }
        } else {
            result.push((case, None));
        }
    }
    result
}
pub fn case_path(case: &Value, variant: Option<&Value>) -> String {
    let id = case["id"].as_str().unwrap();
    variant.map_or_else(
        || id.into(),
        |v| format!("{id}/{}", v["variant"].as_str().unwrap()),
    )
}

type Files = BTreeMap<usize, Vec<Vec<u8>>>;
type Base = (Files, [u8; 16]);
fn base_artifact(artifact: &str) -> Result<Base, String> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<BTreeMap<String, Base>>> =
        std::sync::OnceLock::new();
    let mut cache = CACHE.get_or_init(Default::default).lock().unwrap();
    if let Some(base) = cache.get(artifact) {
        return Ok(base.clone());
    }
    let base = if let Some(name) = artifact.strip_prefix("tape-image ") {
        let base = generate(name)?;
        let uuid = base.image.files[0].bytes[0x10..0x20].try_into().unwrap();
        (
            base.image
                .files
                .iter()
                .enumerate()
                .map(|(i, f)| (i, f.bytes.chunks_exact(B).map(<[u8]>::to_vec).collect()))
                .collect(),
            uuid,
        )
    } else {
        (
            profiles::build(
                artifact
                    .strip_prefix("terminal profile ")
                    .ok_or("unknown artifact")?,
            )?,
            [0x11; 16],
        )
    };
    cache.insert(artifact.into(), base.clone());
    Ok(base)
}

/// Resolve the frozen text to bytes; an inconsistent source value is an error,
/// never a license to substitute the bytes generated by the reference.
pub fn resolve(case: &Value, variant: Option<&Value>) -> Result<Resolved, String> {
    let id = case["id"].as_str().unwrap();
    let path = case_path(case, variant);
    let null = Value::Null;
    let v = variant.unwrap_or(&null);
    let artifact = if v["base_override"].is_string() {
        "terminal profile minimal-256k"
    } else {
        match id {
            // A unit-level descriptor: no image holds it (see the case's note).
            "overflow-3.3-stripe-mapping-inverse" => "unit level",
            // "Any image, under a fault-injecting transport": the device report
            // is injected over the smallest finalized image.
            "overflow-12.2-walk-length" => "tape-image a4-minimal",
            _ => case["base"]["artifact"].as_str().unwrap(),
        }
    };
    let (files, uuid) = if artifact == "unit level" {
        (BTreeMap::new(), [0; 16])
    } else {
        base_artifact(artifact)?
    };
    let mut editor = Editor {
        artifact: artifact.into(),
        files,
        edits: Vec::new(),
        changed: BTreeSet::new(),
    };
    let terminal_start = if artifact.ends_with("multi-256k") {
        6
    } else {
        1
    };
    let file = case["base"]["tape_file"]
        .as_u64()
        .map(|n| n as usize)
        .unwrap_or(terminal_start);
    let mut block = v["block_within_file"]
        .as_u64()
        .or(case["base"]["block_within_file"].as_u64())
        .unwrap_or(0) as usize;
    let mut targets = vec![file];
    let mut mutation = case["mutation"].clone();
    if let Some(obj) = v.as_object() {
        for (k, value) in obj {
            if ["field", "offset", "from", "to", "le_bytes", "change"].contains(&k.as_str()) {
                mutation[k] = value.clone();
            }
        }
    }
    let repairs = if v["repair"].is_array() {
        v["repair"].to_string()
    } else {
        case["integrity_repair"].to_string()
    };
    let role = if case["group"] == "7c" {
        overflow::mutate(&mut editor, case, v, file, &mut block, &mut targets)?
    } else if id.starts_with("sidecar-") {
        // Stream membership comes from the original copy and explicit entry
        // additions/removals, not hostile count fields that may contradict it.
        let mut stream_end = 0xc8
            + u64_at(
                &editor.files[&file][if block == 6 { 0 } else { block }],
                0x68,
            ) as usize;
        if let Some(mutations) = v["mutation"].as_array() {
            for m in mutations {
                editor.field(file, block, m)?;
            }
            stream_end = match v["variant"].as_str().unwrap() {
                "a-not-end-minus-start" => 0x120,
                "b-above-logical" => 0x130,
                _ => return Err("unknown stream change".into()),
            };
        } else {
            editor.field(file, block, &mutation)?;
        }
        editor.sidecar_repair(file, block, &repairs, stream_end);
        if id == "sidecar-primary-tail-disagreement" {
            Role::SidecarCopies
        } else if block == 6 {
            Role::SidecarFooter
        } else {
            Role::SidecarCopy
        }
    } else if id == "paritymap-directory-not-ascending" {
        let len = u64_at(&editor.files[&file][0], 0x30) as usize;
        let original = editor.files[&file][0][0xc8..0xc8 + len].to_vec();
        if editor.files[&file][1][0xc8..0xc8 + len] != original {
            return Err("ParityMap payload copies differ before mutation".into());
        }
        let mut payload: Cbor =
            ciborium::from_reader(original.as_slice()).map_err(|e| e.to_string())?;
        let entries = cbor_key(cbor_key(&mut payload, 4), 5)
            .as_array_mut()
            .ok_or("directory entries not array")?;
        if entries.len() != 2 {
            return Err("directory must have two entries".into());
        }
        for (i, entry) in entries.iter_mut().enumerate() {
            // Directory key 1 is tape_file_number, key 2 is epoch_id.
            if cbor_key(entry, 2).as_integer().map(i128::from) != Some(i as i128)
                || cbor_key(entry, 1).as_integer().map(i128::from) != Some((i + 2) as i128)
            {
                return Err("directory entry order differs from [e0,e1]".into());
            }
        }
        entries.swap(0, 1);
        let new = cbor_bytes(&payload);
        if new.len() != len {
            return Err("directory swap changes payload length".into());
        }
        for block in [0, 1] {
            editor.write(
                file,
                block,
                0xc8,
                &new,
                "mutation: swap complete CBOR directory entries",
            );
        }
        for block in [0, 1, 2] {
            editor.write(
                file,
                block,
                0x38,
                &Sha256::digest(&new),
                "R-PM-PAYLOAD payload_sha256",
            );
            editor.crc(file, block, 0xc0, "R-PM-PAYLOAD CRC");
        }
        Role::ParityMap
    } else if id == "paritymap-footer-header-disagreement" {
        editor.field(file, block, &mutation)?;
        editor.crc(file, block, 0xc0, "R-PM-FOOTER CRC");
        Role::ParityMap
    } else if id == "bootstrap-payload-past-block" {
        editor.field(file, 0, &mutation)?;
        editor.crc(file, 0, 0x30, "R-BOOT-HDR CRC");
        Role::Bootstrap
    } else if id == "e1-16" {
        // A conformant no-parity bootstrap recording drive compression: flags
        // bit 0 set, the payload re-encoded without key 1 and with key 5 true,
        // then its length, zero fill and both CRCs.
        let original = &editor.files[&file][0];
        let len = u32::from_le_bytes(original[0x2c..0x30].try_into().unwrap()) as usize;
        let mut payload: Cbor =
            ciborium::from_reader(&original[0x38..0x38 + len]).map_err(|e| e.to_string())?;
        let entries = payload
            .as_map_mut()
            .ok_or("bootstrap payload is not a map")?;
        let before = entries.len();
        entries.retain(|(k, _)| k.as_integer().map(i128::from) != Some(1));
        if entries.len() + 1 != before {
            return Err("bootstrap payload has no key 1".into());
        }
        if *cbor_key(&mut payload, 5) != Cbor::Bool(false) {
            return Err("bootstrap payload key 5 is not false".into());
        }
        *cbor_key(&mut payload, 5) = Cbor::Bool(true);
        let new = cbor_bytes(&payload);
        if new.len() >= len {
            return Err("no-parity payload is not shorter than the original".into());
        }
        if original[0x0c..0x10] != [0, 0, 0, 0] {
            return Err("bootstrap flags are not zero".into());
        }
        editor.write(
            file,
            0,
            0x0c,
            &1u32.to_be_bytes(),
            "mutation: flags bit 0 (no-parity)",
        );
        editor.write(
            file,
            0,
            0x38,
            &new,
            "mutation: payload without key 1 (scheme), key 5 (drive_compression) true",
        );
        editor.write(
            file,
            0,
            0x38 + new.len() + 8,
            &vec![0; len - new.len()],
            "zero fill after the shorter payload",
        );
        editor.write(
            file,
            0,
            0x2c,
            &(new.len() as u32).to_le_bytes(),
            "cbor_payload_len",
        );
        editor.write(
            file,
            0,
            0x38 + new.len(),
            &crc64_xz(&new).to_le_bytes(),
            "payload CRC",
        );
        editor.crc(file, 0, 0x30, "header CRC");
        Role::Bootstrap
    } else if id == "terminal-object-id-over-64" {
        targets = v["replicas"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                terminal_start
                    + match r.as_str().unwrap() {
                        "A" => 0,
                        "B" => 2,
                        "C" => 4,
                        _ => unreachable!(),
                    }
            })
            .collect();
        for &file in &targets {
            let slot = &editor.files[&file][1][384..640];
            let len = u16::from_le_bytes(slot[..2].try_into().unwrap()) as usize;
            let mut row: Cbor =
                ciborium::from_reader(&slot[2..2 + len]).map_err(|e| e.to_string())?;
            let object_id = cbor_key(&mut row, 4);
            if object_id.as_bytes().map(Vec::as_slice)
                != Some(b"minimal-plaintext-object".as_slice())
                || object_id.as_bytes().unwrap().len() != 24
            {
                return Err("Object-row key 4 is not the specified 24-byte object_id".into());
            }
            *object_id = Cbor::Bytes(vec![0x61; 65]);
            let encoded = cbor_bytes(&row);
            if encoded.len() > 254 {
                return Err("mutated Object row exceeds slot".into());
            }
            let mut slot = vec![0; 256];
            slot[..2].copy_from_slice(&(encoded.len() as u16).to_le_bytes());
            slot[2..2 + encoded.len()].copy_from_slice(&encoded);
            editor.write(
                file,
                1,
                384,
                &slot,
                "mutation: object_id slot, encoded_len and padding",
            );
            let len = u64_at(&editor.files[&file][0], 0x70) as usize;
            let mut h = Sha256::new();
            h.update(b"REM-TAPE-INDEX-REPLICA-PAYLOAD-V1\0");
            h.update(&editor.files[&file][1][..len]);
            let digest = h.finalize();
            for block in [0, 2] {
                editor.write(file, block, 0xa8, &digest, "R-TR-PAYLOAD payload SHA-256");
            }
            editor.replica_repair(file, true);
        }
        Role::Replica
    } else if id == "terminal-scope-scalars" {
        for block in [0, 2] {
            editor.field(file, block, &mutation)?;
        }
        editor.replica_repair(file, true);
        Role::Replica
    } else if id == "terminal-replica-role-magic" {
        if v["variant"] == "b-header-carries-footer-magic" {
            editor.write(
                file,
                0,
                0,
                &derive_tape_index_replica_footer_magic(&uuid),
                "mutation: footer role magic",
            );
        } else {
            editor.field(file, 0, &mutation)?;
        }
        editor.replica_repair(file, false);
        Role::Replica
    } else if id == "separation-extent-role-magic" {
        targets = vec![file + 1];
        editor.field(file + 1, 0, &mutation)?;
        editor.crc(file + 1, 0, 0x1f8, "R-SEP header CRC");
        let digest = Sha256::digest(&editor.files[&(file + 1)][0]);
        editor.write(
            file + 1,
            2,
            0x180,
            &digest,
            "R-SEP footer header-record SHA-256",
        );
        editor.crc(file + 1, 2, 0x1f8, "R-SEP footer CRC");
        Role::Separation
    } else {
        return Err(format!("no resolver for {id}"));
    };
    let mut manifest = String::new();
    for &(file, block) in &editor.changed {
        let bytes = &editor.files[&file][block];
        manifest.push_str(&format!(
            "{path}\t{artifact}\t{file}\t{block}\t{}\t{}\n",
            bytes.len(),
            hex(&Sha256::digest(bytes))
        ));
    }
    let roles = if id == "paritymap-directory-not-ascending" {
        vec![role, Role::Directory]
    } else if id == "overflow-13.3-tail-location" {
        vec![Role::ParityMap, Role::Directory, role]
    } else if id == "overflow-9.1-block-locator" {
        vec![role, Role::ParityLocator]
    } else if id == "overflow-3.2-lba" {
        vec![role, Role::TerminalScanner]
    } else if matches!(
        id,
        "sidecar-total-block-count" | "sidecar-primary-tail-disagreement"
    ) {
        // The observations of the sidecar-acquisition cases: the component
        // parser, the Recoverer, a Verifier, the Scanner with intact replicas,
        // and the BOT walk (had the replicas failed).
        let mut roles = vec![role, Role::Recovery, Role::Verifier];
        // The footer variant's author states no observation of the Scanner
        // with intact replicas.
        if !(id == "sidecar-total-block-count" && block == 6) {
            roles.push(Role::TerminalScanner);
        }
        roles.push(Role::WalkScanner);
        roles
    } else {
        vec![role]
    };
    let mut descriptor = json!({"case":path,"artifact":artifact,"target_role":format!("{role:?}"),"entry_points":roles.iter().map(|role|ROLES.iter().find(|r|r.role==*role).unwrap().entry).collect::<Vec<_>>(),"edits":editor.edits});
    if let Some(injection) = overflow::injection(id)? {
        descriptor["injection"] = injection;
    }
    Ok(Resolved {
        path,
        expected: variant.unwrap_or(case).clone(),
        descriptor,
        manifest,
        files: editor.files,
        uuid,
        roles,
        targets,
        block,
        injected: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Structured matches are authoritative; malformed and unused waivers fail closed.
    #[test]
    fn structured_adjudications_fail_closed() {
        let original = json!({
            "open": [{"id": "generic-case", "match": {"variant": "isolated", "observation": "reader"}, "disputed_outcomes": ["ACCEPTED", "SidecarParse"]}],
            "settled": [{"id": "generic-case", "match": {"variant": "isolated", "observation": "decoder"}, "corrected": "SidecarParse"}]
        });
        let parse = |value: &Value| parse_adjudications(&serde_json::to_vec(value).unwrap());
        let parsed = parse(&original).unwrap();
        let open_key = ("generic-case".into(), "isolated".into(), "reader".into());
        let settled_key = ("generic-case".into(), "isolated".into(), "decoder".into());
        assert!(parsed.allows_open(&open_key, "SidecarParse"));
        assert!(!parsed.allows_open(&open_key, "PANIC"));
        assert!(!parsed.allows_open(&open_key, "Invariant"));
        assert_eq!(parsed.settled[&settled_key], "SidecarParse");
        assert!(parsed.ensure_all_observed(&BTreeSet::new()).is_err());
        assert!(parsed
            .ensure_all_observed(&BTreeSet::from([open_key.clone()]))
            .is_err());
        parsed
            .ensure_all_observed(&BTreeSet::from([open_key, settled_key]))
            .unwrap();
        for (pointer, malformed) in [
            ("/open/0/id", json!("")),
            ("/open/0/match", Value::Null),
            ("/open/0/match/variant", json!(1)),
            ("/open/0/match/observation", json!("")),
            (
                "/open/0/match",
                json!({"variant": "", "observation": "reader", "extra": "wildcard"}),
            ),
            ("/open/0/disputed_outcomes", json!([])),
            ("/open/0/disputed_outcomes", json!(["ACCEPTED", "ACCEPTED"])),
            ("/open/0/disputed_outcomes", json!(["PANIC"])),
            ("/open/0/disputed_outcomes", json!(["UNKNOWN"])),
            ("/open/0/disputed_outcomes", json!([1])),
            ("/settled/0/corrected", json!("PANIC")),
            (
                "/settled/0/corrected",
                json!("SidecarMetadataUnavailable{epoch_id: -1}"),
            ),
            ("/settled/0/match/observation", json!("reader")),
        ] {
            let mut value = original.clone();
            *value.pointer_mut(pointer).unwrap() = malformed;
            assert!(parse(&value).is_err(), "accepted malformed {value}");
        }
        for kind in ["open", "settled"] {
            let mut value = original.clone();
            let entries = value[kind].as_array_mut().unwrap();
            entries.push(entries[0].clone());
            assert!(parse(&value).is_err(), "accepted duplicate {kind} entry");
        }
    }

    /// Exact and wildcard exceptions fail closed on new, moved, resolved or removed misses.
    #[test]
    fn isolation_exceptions_are_scoped_and_stale_checked() {
        let parse = |v: Value| parse_isolation_exceptions(&serde_json::to_vec(&v).unwrap());
        let exceptions = parse(json!({"entries":[
            {"variant":"case/isolated", "observation":"reader"},
            {"variant":"wild/isolated", "observation":"*"}
        ]}))
        .unwrap();
        let mut seen = BTreeSet::new();
        assert!(exceptions.ensure_all_observed(&seen).is_err());
        assert_eq!(
            exceptions
                .check("case/isolated", "reader", true, &mut seen)
                .unwrap(),
            Some("ISOLATION-EXCEPTION")
        );
        assert!(exceptions
            .check("case/isolated", "other", true, &mut seen)
            .is_err());
        assert!(exceptions
            .check("other/isolated", "reader", true, &mut seen)
            .is_err());
        assert!(exceptions
            .check("case/isolated", "reader", false, &mut seen)
            .is_err());
        for observation in ["reader", "replica"] {
            assert_eq!(
                exceptions
                    .check("wild/isolated", observation, true, &mut seen)
                    .unwrap(),
                Some("ISOLATION-EXCEPTION")
            );
        }
        assert!(exceptions
            .check("wild/isolated", "reader", false, &mut seen)
            .is_err());
        exceptions.ensure_all_observed(&seen).unwrap();
        assert_eq!(
            exceptions
                .check("other/isolated", "reader", false, &mut seen)
                .unwrap(),
            None
        );
        for entries in [
            json!([{"variant":"case/isolated","observation":"*"},{"variant":"case/isolated","observation":"reader"}]),
            json!([{"variant":"case/isolated","observation":"reader"},{"variant":"case/isolated","observation":"reader"}]),
            json!([{"variant":"case/isolated","observation":""}]),
        ] {
            assert!(parse(json!({"entries":entries})).is_err());
        }
    }

    /// Open adjudications signal agreement; Invariant cannot pass even informatively.
    /// A permitted set is pinned: either member passes, anything else fails,
    /// and a set of fewer than two outcomes or a mixed entry is refused.
    #[test]
    fn a_pinned_permitted_set_passes_either_member_only() {
        let entry = |extra: Value| {
            let mut e = json!({"id": "c", "match": {"variant": "v", "observation": "o"},
                "permitted_outcomes": ["DirectoryInvalid", "ParityMapParse"]});
            for (k, v) in extra.as_object().unwrap() {
                e[k] = v.clone();
            }
            serde_json::to_vec(&json!({"open": [], "settled": [e]})).unwrap()
        };
        let adjudications = parse_adjudications(&entry(json!({}))).unwrap();
        let key = ("c".to_string(), "v".to_string(), "o".to_string());
        assert!(adjudications.pinned_sets[&key].contains("ParityMapParse"));
        for bad in [
            json!({"permitted_outcomes": ["DirectoryInvalid"]}),
            json!({"corrected": "ACCEPTED"}),
            json!({"permitted_outcomes": ["DirectoryInvalid", "PANIC"]}),
        ] {
            assert!(parse_adjudications(&entry(bad)).is_err());
        }
        let observation = |outcome: &str| Observation {
            location: "test".into(),
            key: "o",
            name: "x",
            detail: String::new(),
            outcome: outcome.into(),
        };
        for (outcome, fails) in [
            ("DirectoryInvalid", false),
            ("ParityMapParse", false),
            ("ACCEPTED", true),
        ] {
            let (mut seen, mut failures) = (BTreeSet::new(), Vec::new());
            adjudications.check_observation(
                key.clone(),
                "c/v",
                &observation(outcome),
                &Value::Null,
                &mut seen,
                &mut failures,
            );
            assert_eq!(!failures.is_empty(), fails, "{outcome}");
        }
    }

    #[test]
    fn adjudication_reports_now_agrees_and_rejects_invariant() {
        let adjudications = parse_adjudications(&serde_json::to_vec(&json!({
            "open":[{"id":"policy-probe","match":{"variant":"isolated","observation":"reader"},"disputed_outcomes":["ACCEPTED","SidecarParse"]}],
            "settled":[]
        })).unwrap()).unwrap();
        let key = ("policy-probe".into(), "isolated".into(), "reader".into());
        let mut o = Observation {
            key: "reader",
            name: "SidecarParse",
            outcome: "SidecarParse".into(),
            location: "policy unit test".into(),
            detail: "now agrees".into(),
        };
        let expected = json!("SidecarParse");
        assert_eq!(
            adjudications.open_status(&key, &o, &expected),
            "DISPUTED-NOW-AGREES"
        );
        let mut failures = Vec::new();
        assert!(adjudications.check_observation(
            key.clone(),
            "policy-probe/isolated",
            &o,
            &expected,
            &mut BTreeSet::new(),
            &mut failures
        ));
        assert!(failures.is_empty());
        o.name = "Invariant";
        o.outcome = "Invariant".into();
        assert!(!supplement::expected_matches(
            &json!({"outcome":"rejection; no Section 15 name"}),
            &o
        ));
        assert_eq!(
            adjudications.open_status(&key, &o, &expected),
            "DISAGREEMENT"
        );
        assert!(adjudication_outcome(&json!("Invariant")).is_err());
    }

    #[test]
    fn adjudication_scope_and_section15_mapping() {
        let adjudications = parse_adjudications(
            &fs::read(fixture_root().join("tape-images/negatives/adjudications.json")).unwrap(),
        )
        .unwrap();
        // The two entries that were open are settled: the Recoverer's outcome
        // for each is ACCEPTED, observed through recover_ordinal_from_sidecar,
        // and nothing else is open.
        // Nothing is open; the c-total-zero parser name is a pinned set.
        assert!(adjudications.open.is_empty());
        assert_eq!(adjudications.pinned_sets.len(), 1);
        let parent = (
            "sidecar-primary-tail-disagreement".to_string(),
            String::new(),
            "recovery".to_string(),
        );
        let isolated = (
            "sidecar-primary-tail-disagreement".to_string(),
            "isolated".to_string(),
            "recoverer".to_string(),
        );
        assert_eq!(
            adjudications.settled.get(&parent).map(String::as_str),
            Some("ACCEPTED")
        );
        assert_eq!(
            adjudications.settled.get(&isolated).map(String::as_str),
            Some("ACCEPTED")
        );
        for outcome in ["PANIC", "Invariant", "TapeIo", "ACCEPTED", "SidecarParse"] {
            assert!(!adjudications.allows_open(&parent, outcome));
        }
        assert!(!adjudications.settled.contains_key(&(
            parent.0.clone(),
            String::new(),
            "agreement".to_string()
        )));
        assert_eq!(
            section15(&ObservedError::Replica(
                TapeIndexReplicaError::DigestMismatch {
                    field: "canonical map SHA-256"
                }
            )),
            "FilemarkMapDigestMismatch"
        );
        assert_eq!(
            section15(&ObservedError::Replica(
                TapeIndexReplicaError::DigestMismatch {
                    field: "payload SHA-256"
                }
            )),
            "TerminalIndexReplicaParse"
        );
        assert_eq!(
            section15(&ObservedError::Replica(
                TapeIndexReplicaError::CompressionEnabled
            )),
            "DriveCompressionEnabled"
        );
        assert_eq!(
            section15(&ObservedError::Separation(
                IndexSeparationError::CompressionEnabled
            )),
            "DriveCompressionEnabled"
        );
        assert_eq!(
            section15(&ObservedError::Separation(
                IndexSeparationError::PhysicalSource("opaque source failure".into())
            )),
            "UNMAPPED"
        );
        // Typed refusals keep their Section 15 names once every continuation
        // decision has been made.
        assert_eq!(
            section15(&ObservedError::Parity(ParityError::BootstrapRefused {
                field: BootstrapRefusedField::TapeUuid,
                detail: "tape identity mismatch".into(),
            })),
            "BootstrapParse"
        );
        assert_eq!(
            section15(&ObservedError::Parity(ParityError::DriveCompressionEnabled)),
            "DriveCompressionEnabled"
        );
    }
    #[test]
    fn healthy_roles_and_repairs() {
        let source = source().expect("frozen source");
        let errata = erratum::load().expect("erratum set E2");
        for (case, variant) in cases(&source) {
            if errata.not_executable(case).is_some() {
                continue;
            }
            let mut v = resolve(case, variant).expect("case resolution");
            let artifact = v.descriptor["artifact"].as_str().unwrap();
            let files = if artifact == "unit level" {
                BTreeMap::new()
            } else {
                base_artifact(artifact).unwrap().0
            };
            // Each observation point must accept the exact unmutated base, with
            // no device report or commit-record claim injected.
            v.files = files;
            v.injected = false;
            for observation in observe(&v) {
                assert_eq!(
                    observation.name, "ACCEPTED",
                    "{} healthy base: {}",
                    v.path, observation.detail
                );
            }
        }
        // Re-signing healthy copies must be byte-identical. This independently
        // exercises repair preimages against the production writer's digests.
        for artifact in [
            "tape-image a4-minimal",
            "terminal profile minimal-256k",
            "terminal profile multi-256k",
        ] {
            let (files, _) = base_artifact(artifact).unwrap();
            let original = files.clone();
            let mut e = Editor {
                artifact: artifact.into(),
                files,
                edits: Vec::new(),
                changed: BTreeSet::new(),
            };
            if artifact.starts_with("tape-image") {
                for block in [0, 5] {
                    e.sidecar_repair(2, block, "R-SC-HASHED", 0x128);
                }
                e.sidecar_repair(2, 6, "R-SC-FOOTER", 0);
            } else {
                let start = if artifact.ends_with("multi-256k") {
                    6
                } else {
                    1
                };
                for file in [start, start + 2, start + 4] {
                    e.layout_digest(file);
                    e.replica_payload(file, true)
                        .expect("healthy payload repair");
                }
                for file in [start + 1, start + 3] {
                    e.separation_repair(file);
                }
            }
            for (file, blocks) in &original {
                for (block, bytes) in blocks.iter().enumerate() {
                    assert!(
                        e.files[file][block] == *bytes,
                        "healthy repair changed {artifact} file={file} block={block}"
                    );
                }
            }
        }
    }
    /// Run erratum set E1's negative constructions. Their expectations are not
    /// merged yet, so each observation is informative; a panic still fails.
    fn execute_erratum(root: &std::path::Path, manifest: &mut String, failures: &mut Vec<String>) {
        let source = erratum_source().expect("erratum source");
        for case in source["cases"].as_array().unwrap() {
            let path = case_path(case, None);
            let mut resolved = match resolve(case, None) {
                Ok(v) => v,
                Err(e) => {
                    println!("UNRESOLVED {path}: {e}");
                    failures.push(path);
                    continue;
                }
            };
            for (name, wanted) in [
                ("expected.json", &resolved.expected),
                ("mutation.json", &resolved.descriptor),
            ] {
                let actual: Value = serde_json::from_slice(
                    &fs::read(root.join(&path).join(name)).expect("generated vector file"),
                )
                .expect("vector JSON");
                assert_eq!(&actual, wanted, "{path}/{name}");
            }
            manifest.push_str(&resolved.manifest);
            let pinned = case["pinned"].as_bool().expect("pinned flag");
            let expected = case["expected"]
                .as_object()
                .unwrap_or_else(|| panic!("{path}: expectation missing or pending"));
            let mut compared = BTreeSet::new();
            for observation in observe(&resolved) {
                // The author's role names the observation point.
                let role = match observation.key {
                    "bootstrap" => "bootstrap role",
                    other => panic!("{path}: no expectation vocabulary for role {other}"),
                };
                let wanted = &expected[role];
                let keys = wanted
                    .as_object()
                    .unwrap_or_else(|| panic!("{path} [{role}]: expectation missing"));
                for key in keys.keys() {
                    assert!(
                        matches!(key.as_str(), "accepts" | "error" | "sections" | "quotes"),
                        "{path} [{role}]: unknown expectation key {key}"
                    );
                }
                assert!(
                    wanted["accepts"].is_boolean() && wanted["error"].is_string(),
                    "{path} [{role}]: accepts must be a bool and error a Section 15 name or none"
                );
                let wanted = json!({"accepts": wanted["accepts"], "error": wanted["error"]});
                let accepted = observation.name == "ACCEPTED";
                let observed = json!({"accepts": accepted,
                    "error": if accepted { "none" } else { observation.outcome.as_str() }});
                let status = if !pinned {
                    "INFORMATIVE"
                } else if wanted == observed {
                    "PASS"
                } else {
                    "DISAGREEMENT"
                };
                println!(
                    "{status} {path} [{role}]: expected {wanted}; observed {observed}; {}; detail={:?}",
                    observation.location, observation.detail
                );
                if (pinned && wanted != observed)
                    || matches!(observation.name, "PANIC" | "Invariant")
                {
                    failures.push(format!("{path} {}", observation.location));
                }
                for quote in expected[role]["quotes"].as_array().into_iter().flatten() {
                    let quote = quote.as_str().expect("quote text");
                    assert!(
                        crate::tape_image_vectors::specification_quote_holds(quote),
                        "{path} [{role}]: quote not in the specification: {quote}"
                    );
                }
                compared.insert(role);
            }
            assert_eq!(
                compared,
                expected.keys().map(String::as_str).collect::<BTreeSet<_>>(),
                "{path}: every expectation must name an observed role"
            );
            // The observation point accepts the exact unmutated base.
            let (files, _) = base_artifact(resolved.descriptor["artifact"].as_str().unwrap())
                .expect("erratum base");
            resolved.files = files;
            for observation in observe(&resolved) {
                assert_eq!(
                    observation.name, "ACCEPTED",
                    "{path} healthy base: {}",
                    observation.detail
                );
            }
        }
    }

    #[test]
    fn negative_vectors() {
        let source = source().expect("frozen negative source");
        let root = fixture_root().join("tape-images/negatives");
        let errata = erratum::load().expect("erratum set E2");
        errata
            .check_against_sources(&source, &supplement::source().expect("supplement source"))
            .expect("every erratum entry replaces the frozen expectation it names");
        let mut seen_errata = BTreeSet::new();
        for entry in &errata.entries {
            if entry.clear_freeze_blocker {
                println!(
                    "ERRATUM {}: freeze_blocker cleared (was true); {}",
                    entry.case,
                    json!({"replaces": entry.replaces})
                );
            }
            if let Some(reason) = &entry.executes {
                println!("ERRATUM {}: executes; {reason}", entry.case);
            }
            for unobserved in &entry.unobserved {
                println!(
                    "ERRATUM-UNOBSERVED {} [{}]: {}; author outcome {}",
                    entry.case,
                    unobserved["author_observation"],
                    unobserved["reason"],
                    unobserved["expected"]
                );
            }
        }
        for entry in ROLES {
            println!("ROLE {:?}: {}", entry.role, entry.entry);
        }
        let adjudications = parse_adjudications(
            &fs::read(root.join("adjudications.json")).expect("supervisor adjudication record"),
        )
        .expect("valid adjudication record");
        let mut seen_adjudications = BTreeSet::new();
        let mut failures = Vec::new();
        let mut manifest =
            String::from("case\tartifact\ttape_file\tblock_within_file\tbytes\tsha256\n");
        for (case, variant) in cases(&source) {
            let path = case_path(case, variant);
            if let Some(reason) = errata.not_executable(case) {
                let actual: Value = serde_json::from_slice(
                    &fs::read(root.join(&path).join("expected.json"))
                        .expect("generated expectation"),
                )
                .unwrap();
                assert_eq!(&actual, variant.unwrap_or(case));
                println!("NOT-EXECUTABLE {path}: {reason}");
                continue;
            }
            let resolved = match resolve(case, variant) {
                Ok(v) => v,
                Err(e) => {
                    println!("UNRESOLVED {path}: {e}");
                    failures.push(path);
                    continue;
                }
            };
            for (name, wanted) in [
                ("expected.json", &resolved.expected),
                ("mutation.json", &resolved.descriptor),
            ] {
                let actual: Value = serde_json::from_slice(
                    &fs::read(root.join(&path).join(name)).expect("generated vector file"),
                )
                .expect("vector JSON");
                assert_eq!(&actual, wanted, "{path}/{name}");
            }
            manifest.push_str(&resolved.manifest);
            let expected = variant
                .and_then(|v| v.get("expected"))
                .unwrap_or(&case["expected"]);
            let pinned = variant
                .and_then(|v| v["pinned"].as_bool())
                .or(case["pinned"].as_bool())
                .expect("pinned flag");
            for observation in observe(&resolved) {
                let variant_name = variant
                    .map(|v| v["variant"].as_str().expect("variant name"))
                    .unwrap_or("");
                if let Some((entry, override_)) = errata.observation(
                    "negative-cases.json",
                    case["id"].as_str().unwrap(),
                    variant_name,
                    observation.key,
                ) {
                    seen_errata.insert((
                        entry.source.clone(),
                        entry.case.clone(),
                        entry.variant.clone(),
                        observation.key.to_string(),
                    ));
                    if !erratum::report(&path, entry, override_, &observation)
                        || matches!(observation.name, "PANIC" | "Invariant")
                    {
                        failures.push(format!("{path} {} (erratum)", observation.location));
                    }
                    continue;
                }
                let adjudication_key = (
                    case["id"].as_str().unwrap().to_string(),
                    variant_name.to_string(),
                    observation.key.to_string(),
                );
                if adjudications.check_observation(
                    adjudication_key,
                    &path,
                    &observation,
                    expected,
                    &mut seen_adjudications,
                    &mut failures,
                ) {
                    continue;
                }
                let Observation {
                    location,
                    name: observed,
                    detail: result,
                    ..
                } = observation;
                let matches = expected
                    .as_str()
                    .is_some_and(|s| s.split([' ', '{']).next() == Some(observed))
                    || expected["error"]
                        .as_str()
                        .is_some_and(|s| s.split('{').next() == Some(observed))
                    || expected["one_of"]
                        .as_array()
                        .is_some_and(|names| names.iter().any(|name| name == observed));
                let status = if !pinned {
                    "INFORMATIVE"
                } else if matches {
                    "PASS"
                } else {
                    "DISAGREEMENT"
                };
                let site = if location.contains("ParityLocator") {
                    "; site=sidecar.rs::parity_block_position (checked H + parity_index*S + stripe)"
                } else if path == "overflow-13.3-tail-location/c-total-zero"
                    && location.contains("Recovery")
                {
                    "; site=parity_map.rs::read_final_parity_map -> recovery.rs::rescue_tail_sidecar_index_with_directory"
                } else {
                    ""
                };
                println!("{status} {path}: expected {expected}; observed {observed}; {location}; detail={result:?}{site}");
                if pinned && !matches {
                    failures.push(format!("{path} {location}"));
                }
            }
            for field in ["tape_level", "concurrent_checks"] {
                let value = variant.and_then(|v| v.get(field)).unwrap_or(&case[field]);
                if !value.is_null() {
                    println!("INFORMATIVE {path} {field}: {value}");
                }
            }
        }
        supplement::execute(
            &root,
            &mut manifest,
            &mut failures,
            &adjudications,
            &mut seen_adjudications,
            &errata,
            &mut seen_errata,
        );
        execute_erratum(&root, &mut manifest, &mut failures);
        adjudications
            .ensure_all_observed(&seen_adjudications)
            .expect("every adjudication observation must execute");
        errata
            .ensure_all_observed(&seen_errata)
            .expect("every erratum override must execute");
        assert_eq!(
            fs::read_to_string(root.join("MANIFEST.tsv")).expect("negative manifest"),
            manifest
        );
        assert!(
            failures.is_empty(),
            "negative vector failures: {failures:?}"
        );
    }
}
