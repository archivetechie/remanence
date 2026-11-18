//! Read-only history over captured library authority prefixes; no fold or catalogue reads.

/// Authority standing captured with the byte limits, before any visitor runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HistoryStanding {
    Clean,
    Fenced,
    Refused { refusal: String },
}

/// Byte traversal distinguishes visitor termination even on the last frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FramesEnd {
    Complete,
    Stopped,
}

use super::*;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::{self, Write},
    ops::ControlFlow,
};

/// Searchable envelope and subject of one verified library record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryHead {
    /// Sequence in the library authority log.
    pub sequence: u64,
    /// Envelope timestamp in nanoseconds since the Unix epoch.
    pub timestamp_unix_ns: u64,
    /// Actor attributed by the original writer.
    pub actor: AuditActor,
    /// Layer attributed by the original writer.
    pub source_layer: SourceLayer,
    /// Optional enclosing operation identity.
    pub operation_id: Option<Id>,
    /// Optional enclosing session identity.
    pub session_id: Option<Id>,
    /// Build string recorded in the envelope.
    pub software_build: String,
    /// Record kind, with the schema variant for event-bearing kinds.
    pub event_kind: String,
    /// Kind of the resource this record describes.
    pub subject_kind: String,
    /// Resolved resource identity, when present.
    pub subject_id: Option<String>,
}

/// Borrowed record detail, valid for the duration of its visitor call.
pub struct HistoryEntry<'a> {
    head: HistoryHead,
    record: &'a Record,
    hash: Hash,
}

/// Completion of a history walk, including authority standing and byte damage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistoryEnd {
    /// Every captured record was visited; authority standing was clean.
    Clean,
    /// Every captured record was visited; publication ambiguity was present.
    Fenced,
    /// Every captured record was visited; authority had latched a refusal.
    Refused {
        /// Exact debug text of the captured authority refusal.
        refusal: String,
    },
    /// The visitor requested a stop, including at the last record.
    Stopped,
    /// Byte validation or I/O failed inside a captured prefix.
    Damaged {
        /// Segment basename at the first failure.
        segment: String,
        /// 0 for a file-open, header or segment-chain fault; the failed frame's start for a frame fault; for an early end, an empty or interior-torn segment or a decoder I/O error, the end of the last verified frame (the header's end when none was verified).
        offset: u64,
        /// Replay fault or I/O error text.
        fault: String,
    },
}

const DETAIL_LIMIT: usize = 1_048_576;
const BLOB_LIMIT: usize = 4_096;

/// Count the complete rendering while retaining at most the detail byte budget.
#[derive(Default)]
struct CountedBody {
    bytes: Vec<u8>,
    rendered_bytes: usize,
}
impl Write for CountedBody {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.rendered_bytes += bytes.len();
        let keep = bytes.len().min(DETAIL_LIMIT - self.bytes.len());
        self.bytes.extend_from_slice(&bytes[..keep]);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl HistoryEntry<'_> {
    /// Envelope values and the subject resolved from earlier history records.
    pub fn head(&self) -> &HistoryHead {
        &self.head
    }

    /// Render wire-named detail, omitting an oversized body with its exact size.
    pub fn detail_json(&self) -> String {
        let mut body = CountedBody::default();
        rendered_body(&self.record.body)
            .render(&mut body)
            .expect("memory writer");
        let mut detail = Vec::new();
        self.prefix(&mut detail).expect("memory writer");
        if detail.len() + body.rendered_bytes + 1 > DETAIL_LIMIT {
            write!(
                detail,
                "{{\"omitted\":{{\"rendered_bytes\":{}}}}}",
                body.rendered_bytes
            )
            .expect("memory writer");
        } else {
            detail.extend_from_slice(&body.bytes);
        }
        detail.push(b'}');
        String::from_utf8(detail).expect("JSON is UTF-8")
    }

    fn prefix(&self, out: &mut dyn Write) -> io::Result<()> {
        out.write_all(b"{\"record_kind\":")?;
        json_string(out, &format!("{:?}", self.record.body.tag()))?;
        out.write_all(b",\"record_uuid\":")?;
        self.record.envelope.record_uuid.render(out)?;
        out.write_all(b",\"record_hash\":")?;
        self.hash.render(out)?;
        out.write_all(b",\"process_id\":")?;
        self.record.envelope.process_id.render(out)?;
        out.write_all(b",\"idempotency_key\":")?;
        self.record.envelope.idempotency_key.render(out)?;
        out.write_all(b",\"subject\":{\"kind\":")?;
        json_string(out, &self.head.subject_kind)?;
        out.write_all(b",\"id\":")?;
        match &self.head.subject_id {
            Some(id) => json_string(out, id)?,
            None => out.write_all(b"null")?,
        }
        out.write_all(b"},\"body\":")
    }
}

/// Schema-generated rendering shares declaration order with the wire codec.
pub(super) trait Render {
    fn render(&self, out: &mut dyn Write) -> io::Result<()>;
    fn variant_name(&self) -> Option<&'static str> {
        None
    }
    #[cfg(test)]
    fn check_fields(&self) {}
}

pub(super) fn json_string(out: &mut dyn Write, value: &str) -> io::Result<()> {
    serde_json::to_writer(out, value).map_err(io::Error::other)
}
fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(DIGITS[(byte >> 4) as usize] as char);
        text.push(DIGITS[(byte & 15) as usize] as char);
    }
    text
}
fn uuid(id: Id) -> String {
    uuid::Uuid::from_bytes(id).to_string()
}
macro_rules! number_render {
    ($($ty:ty),+) => {$(impl Render for $ty {
        fn render(&self, out: &mut dyn Write) -> io::Result<()> { write!(out, "{self}") }
    })+};
}
number_render!(u8, u16, u32, u64, bool);
impl Render for Negative {
    fn render(&self, out: &mut dyn Write) -> io::Result<()> {
        write!(out, "{}", self.0)
    }
}
impl<T: Render> Render for Option<T> {
    fn render(&self, out: &mut dyn Write) -> io::Result<()> {
        match self {
            Some(value) => value.render(out),
            None => out.write_all(b"null"),
        }
    }
    #[cfg(test)]
    fn check_fields(&self) {
        if let Some(value) = self {
            value.check_fields();
        }
    }
}
impl<T: Render, const N: usize> Render for Bounded<T, N> {
    fn render(&self, out: &mut dyn Write) -> io::Result<()> {
        out.write_all(b"[")?;
        for (index, value) in self.0.iter().enumerate() {
            if index > 0 {
                out.write_all(b",")?;
            }
            value.render(out)?;
        }
        out.write_all(b"]")
    }
    #[cfg(test)]
    fn check_fields(&self) {
        for value in &self.0 {
            value.check_fields();
        }
    }
}
impl Render for Unit {
    fn render(&self, out: &mut dyn Write) -> io::Result<()> {
        out.write_all(b"null")
    }
}
impl Render for [u8; 16] {
    fn render(&self, out: &mut dyn Write) -> io::Result<()> {
        json_string(out, &uuid(*self))
    }
}
impl Render for [u8; 32] {
    fn render(&self, out: &mut dyn Write) -> io::Result<()> {
        json_string(out, &hex(self))
    }
}
impl<const N: usize> Render for TextValue<N> {
    fn render(&self, out: &mut dyn Write) -> io::Result<()> {
        json_string(out, self.as_str())
    }
}
fn large_blob(out: &mut dyn Write, bytes: &[u8]) -> io::Result<()> {
    write!(
        out,
        "{{\"len\":{},\"sha256\":\"{}\"}}",
        bytes.len(),
        hex(&Sha256::digest(bytes))
    )
}
impl<const N: usize> Render for Blob<N> {
    fn render(&self, out: &mut dyn Write) -> io::Result<()> {
        if self.0.len() > BLOB_LIMIT {
            large_blob(out, &self.0)
        } else {
            json_string(out, &hex(&self.0))
        }
    }
}
impl Render for AuditActor {
    fn render(&self, out: &mut dyn Write) -> io::Result<()> {
        match self {
            Self::System => json_string(out, "system"),
            Self::User(id) => json_string(out, &format!("user:{id}")),
            Self::Service(id) => json_string(out, &format!("service:{id}")),
        }
    }
}
impl Render for DiagnosticEntry {
    fn render(&self, out: &mut dyn Write) -> io::Result<()> {
        out.write_all(b"[")?;
        self.0.render(out)?;
        out.write_all(b",")?;
        match &self.1 {
            DiagnosticValue::Text(value) => value.render(out)?,
            DiagnosticValue::Unsigned(value) => value.render(out)?,
            DiagnosticValue::Negative(value) => value.render(out)?,
            DiagnosticValue::Bool(value) => value.render(out)?,
            DiagnosticValue::Null(value) => value.render(out)?,
            DiagnosticValue::Bytes(value) => {
                if value.0.len() > BLOB_LIMIT {
                    large_blob(out, &value.0)?;
                } else {
                    out.write_all(b"{\"hex\":")?;
                    json_string(out, &hex(&value.0))?;
                    out.write_all(b"}")?;
                }
            }
        }
        out.write_all(b"]")
    }
}
impl Render for MediumFamily {
    fn render(&self, out: &mut dyn Write) -> io::Result<()> {
        match self {
            Self::Lto(value) => {
                out.write_all(b"{\"Lto\":")?;
                value.render(out)?;
                out.write_all(b"}")
            }
            Self::Disk => out.write_all(b"{\"Disk\":null}"),
        }
    }
    fn variant_name(&self) -> Option<&'static str> {
        Some(match self {
            Self::Lto(_) => "Lto",
            Self::Disk => "Disk",
        })
    }
}
impl Render for FenceScope {
    fn render(&self, out: &mut dyn Write) -> io::Result<()> {
        match self {
            Self::Tape(value) => {
                out.write_all(b"{\"Tape\":")?;
                value.render(out)?;
            }
            Self::Drive(value) => {
                out.write_all(b"{\"Drive\":")?;
                value.render(out)?;
            }
            Self::Model { model, firmware } => {
                out.write_all(b"{\"Model\":{\"model\":")?;
                model.render(out)?;
                out.write_all(b",\"firmware\":")?;
                firmware.render(out)?;
                out.write_all(b"}")?;
            }
        }
        out.write_all(b"}")
    }
    fn variant_name(&self) -> Option<&'static str> {
        Some(match self {
            Self::Tape(_) => "Tape",
            Self::Drive(_) => "Drive",
            Self::Model { .. } => "Model",
        })
    }
    #[cfg(test)]
    fn check_fields(&self) {
        match self {
            Self::Tape(value) => value.check_fields(),
            Self::Drive(value) => value.check_fields(),
            Self::Model { .. } => (),
        }
    }
}

fn rendered_body(body: &Body) -> &dyn Render {
    match body {
        Body::AssignPool(v) => v,
        Body::Label(v) => v,
        Body::Retire(v) => v,
        Body::Reprovision(v) => v,
        Body::MediumSpecSet(v) => v,
        Body::SupersedeTail(v) => v,
        Body::AcceptUnprotectedFinalization(v) => v,
        Body::FenceRaised(v) => v,
        Body::FenceReleased(v) => v,
        Body::MotionIntent(v) => v,
        Body::MotionOutcome(v) => v,
        Body::InventoryReconciled(v) => v,
        Body::VerificationProgress(v) => v,
        Body::VerificationVerdict(v) => v,
        Body::ManualFinalizationAccepted(v) => v,
        Body::ConfigurationAccepted(v) => v,
        Body::ReadinessObserved(v) => v,
        Body::DriveObserved(v) => v,
        Body::DriveDecision(v) => v,
        Body::DriveHealthObserved(v) => v,
        Body::CleaningRecorded(v) => v,
        Body::AlarmChanged(v) => v,
        Body::OperationRecorded(v) => v,
        Body::SessionObserved(v) => v,
        Body::WrapMapCached(v) => v,
        Body::RecoveryRecorded(v) => v,
        Body::Diagnostic(v) => v,
    }
}

fn event_kind(body: &Body) -> String {
    let kind = format!("{:?}", body.tag());
    let variant = match body {
        Body::OperationRecorded(v) => v.event.variant_name(),
        Body::SessionObserved(v) => v.event.variant_name(),
        Body::AlarmChanged(v) => v.action.variant_name(),
        Body::CleaningRecorded(v) => v.action.variant_name(),
        Body::DriveDecision(v) => v.decision.variant_name(),
        Body::MotionIntent(v) => v.motion.variant_name(),
        Body::WrapMapCached(v) => v.action.variant_name(),
        Body::RecoveryRecorded(v) => v.action.variant_name(),
        Body::Diagnostic(v) => v.event.variant_name(),
        Body::AssignPool(_)
        | Body::Label(_)
        | Body::Retire(_)
        | Body::Reprovision(_)
        | Body::MediumSpecSet(_)
        | Body::SupersedeTail(_)
        | Body::AcceptUnprotectedFinalization(_)
        | Body::FenceRaised(_)
        | Body::FenceReleased(_)
        | Body::MotionOutcome(_)
        | Body::InventoryReconciled(_)
        | Body::VerificationProgress(_)
        | Body::VerificationVerdict(_)
        | Body::ManualFinalizationAccepted(_)
        | Body::ConfigurationAccepted(_)
        | Body::ReadinessObserved(_)
        | Body::DriveObserved(_)
        | Body::DriveHealthObserved(_) => None,
    };
    match variant {
        Some(variant) => format!("{kind}.{variant}"),
        None => kind,
    }
}

type Subject = (String, Option<String>);
fn subject(kind: &str, id: impl Into<String>) -> Subject {
    (kind.into(), Some(id.into()))
}
fn tape_subject(tape: TapeRef) -> Subject {
    subject("tape", uuid(tape.uuid))
}
fn endpoint_id(endpoint: &DriveKey) -> String {
    match endpoint {
        DriveKey::Bay(bay) => format!("bay:{}:0x{:04x}", bay.library_serial.as_str(), bay.bay),
        DriveKey::Standalone(drive) => format!("standalone:{}", drive.drive_serial.as_str()),
    }
}
#[derive(Default)]
struct Walker {
    subjects: BTreeMap<u64, Subject>,
}
impl Walker {
    fn subject(&mut self, body: &Body, sequence: u64) -> Subject {
        match body {
            Body::AssignPool(v) => tape_subject(v.tape),
            Body::Label(v) => tape_subject(v.tape),
            Body::Retire(v) => tape_subject(v.tape),
            Body::Reprovision(v) => tape_subject(v.tape),
            Body::MediumSpecSet(v) => tape_subject(v.tape),
            Body::WrapMapCached(v) => tape_subject(v.tape),
            Body::VerificationVerdict(v) => tape_subject(v.tape),
            Body::SupersedeTail(v) => tape_subject(v.at.tape),
            Body::AcceptUnprotectedFinalization(v) => tape_subject(v.at.tape),
            Body::VerificationProgress(v) => tape_subject(v.at.tape),
            Body::ManualFinalizationAccepted(v) => tape_subject(v.at.tape),
            Body::RecoveryRecorded(v) => tape_subject(v.at.tape),
            Body::FenceRaised(v) => {
                let value = match &v.scope {
                    FenceScope::Tape(tape) => tape_subject(*tape),
                    FenceScope::Drive(endpoint) => subject("endpoint", endpoint_id(endpoint)),
                    FenceScope::Model { model, firmware } => {
                        subject("model", format!("{}/{}", model.as_str(), firmware.as_str()))
                    }
                };
                self.subjects.insert(sequence, value.clone());
                value
            }
            Body::FenceReleased(v) => self
                .subjects
                .remove(&v.fence)
                .unwrap_or_else(|| subject("fence", v.fence.to_string())),
            Body::MotionOutcome(v) => self
                .subjects
                .remove(&v.intent.sequence)
                .unwrap_or_else(|| subject("motion", v.intent.sequence.to_string())),
            Body::MotionIntent(v) => {
                let value = match &v.motion {
                    Motion::Load(v) => subject("endpoint", endpoint_id(&v.endpoint)),
                    Motion::Unload(v) => subject("endpoint", endpoint_id(&v.endpoint)),
                    Motion::Move(v) => subject("library", v.library_serial.as_str()),
                };
                self.subjects.insert(sequence, value.clone());
                value
            }
            Body::InventoryReconciled(v) => {
                for intent in &v.resolves.0 {
                    self.subjects.remove(&intent.sequence);
                }
                match &v.subject {
                    InventorySubject::Library(serial) => subject("library", serial.as_str()),
                    InventorySubject::Standalone(drive) => subject(
                        "endpoint",
                        format!("standalone:{}", drive.drive_serial.as_str()),
                    ),
                }
            }
            Body::ConfigurationAccepted(v) => subject("configuration", v.generation.to_string()),
            Body::ReadinessObserved(v) => match (&v.scope, &v.endpoint) {
                (ReadinessScope::Library, DriveKey::Bay(bay)) => {
                    subject("library", bay.library_serial.as_str())
                }
                _ => subject("endpoint", endpoint_id(&v.endpoint)),
            },
            Body::DriveObserved(v) => subject("drive", uuid(v.drive_uuid)),
            Body::DriveDecision(v) => subject("drive", uuid(v.drive_uuid)),
            Body::DriveHealthObserved(v) => subject("drive", uuid(v.drive_uuid)),
            Body::CleaningRecorded(v) => match (&v.action, v.cartridge) {
                (
                    CleaningAction::Register(_)
                    | CleaningAction::Expire(_)
                    | CleaningAction::Reject(_),
                    Some(tape),
                ) => tape_subject(tape),
                _ => subject("cleaning_run", v.run_id.as_str()),
            },
            Body::AlarmChanged(v) => subject("alarm", v.condition_key.as_str()),
            Body::OperationRecorded(v) => subject("operation", uuid(v.operation)),
            Body::SessionObserved(v) => subject("session", uuid(v.session_id)),
            Body::Diagnostic(v) => (
                v.subject_kind.as_str().into(),
                v.subject_id.as_ref().map(|id| id.as_str().into()),
            ),
        }
    }
    fn entry<'a>(&mut self, record: &'a Record, sequence: u64, hash: Hash) -> HistoryEntry<'a> {
        let envelope = &record.envelope;
        let (subject_kind, subject_id) = self.subject(&record.body, sequence);
        HistoryEntry {
            head: HistoryHead {
                sequence,
                timestamp_unix_ns: envelope.timestamp_unix_ns,
                actor: envelope.actor.clone(),
                source_layer: envelope.source_layer.clone(),
                operation_id: envelope.operation_id,
                session_id: envelope.session_id,
                software_build: envelope.software_build.as_str().into(),
                event_kind: event_kind(&record.body),
                subject_kind,
                subject_id,
            },
            record,
            hash,
        }
    }
}

/// Walk captured bytes with only snapshot leases retained during visitor calls.
pub(crate) fn walk(
    snapshot: &LibrarySnapshot,
    standing: HistoryStanding,
    visit: &mut dyn FnMut(&HistoryEntry<'_>) -> ControlFlow<()>,
) -> HistoryEnd {
    let mut walker = Walker::default();
    match snapshot
        .frames(&mut |record, sequence, hash| visit(&walker.entry(record, sequence, hash)))
    {
        Ok(FramesEnd::Stopped) => HistoryEnd::Stopped,
        Ok(FramesEnd::Complete) => match standing {
            HistoryStanding::Clean => HistoryEnd::Clean,
            HistoryStanding::Fenced => HistoryEnd::Fenced,
            HistoryStanding::Refused { refusal } => HistoryEnd::Refused { refusal },
        },
        Err((segment, offset, fault)) => HistoryEnd::Damaged {
            segment,
            offset,
            fault,
        },
    }
}

/// Test-only serde visitor preserves object order without serde_json's map sorting.
#[cfg(test)]
pub(super) fn check_field_keys(value: &dyn Render, fields: &[(&str, &str)]) {
    use serde::de::{MapAccess, Visitor};
    use serde::Deserializer;
    struct Keys;
    impl<'de> Visitor<'de> for Keys {
        type Value = Vec<String>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("an object")
        }
        fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
            let mut keys = vec![];
            while let Some(key) = map.next_key::<String>()? {
                keys.push(key);
                map.next_value::<serde::de::IgnoredAny>()?;
            }
            Ok(keys)
        }
    }
    let mut bytes = Vec::new();
    value.render(&mut bytes).unwrap();
    let keys = serde_json::Deserializer::from_slice(&bytes)
        .deserialize_map(Keys)
        .unwrap();
    assert_eq!(
        keys,
        fields.iter().map(|(name, _)| *name).collect::<Vec<_>>()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library_log::{self as lib, codec, store, tests as fixture};
    use std::{collections::BTreeSet, fs, ops::ControlFlow, path::PathBuf};

    fn collect(source: &store::ReadSnapshot) -> Vec<(u64, lib::Id, lib::Hash)> {
        let mut out = Vec::new();
        assert_eq!(
            source.frames(&mut |record, sequence, hash| {
                out.push((sequence, record.envelope.record_uuid, hash));
                ControlFlow::Continue(())
            }),
            Ok(FramesEnd::Complete)
        );
        out
    }

    /// S1: the byte walker delivers exactly the replay receipt hashes for all tags.
    #[test]
    fn s1_frames_match_received() {
        let dir = tempfile::tempdir().unwrap();
        lib::reference_fixtures::all_records(dir.path());
        let (log, _) = fixture::open(dir.path(), &fixture::Present);
        let (source, standing) = log.history_source().unwrap();
        assert_eq!(standing, HistoryStanding::Clean);
        let actual = collect(&source);
        let mut expected = Vec::new();
        let mut tags = BTreeSet::new();
        let replay = source
            .inspect(
                &fixture::Present,
                &mut |_| Ok(()),
                &mut |record, receipt| {
                    tags.insert(record.body.tag() as u16);
                    expected.push((receipt.sequence, receipt.record_uuid, receipt.record_hash));
                    Ok(())
                },
            )
            .unwrap();
        assert!(replay.refusal.is_none());
        assert!(!actual.is_empty());
        assert_eq!(tags.len(), 27);
        assert_eq!(actual.len(), 139);
        assert_eq!(actual, expected);
    }

    struct DamageFixture {
        source: store::ReadSnapshot,
        log: lib::LibraryLog,
        path: PathBuf,
        bytes: Vec<u8>,
        starts: Vec<usize>,
        _dir: tempfile::TempDir,
    }
    impl DamageFixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let (mut log, _) = fixture::open(dir.path(), &fixture::Present);
            for _ in 0..3 {
                fixture::put(&mut log, fixture::diagnostic());
            }
            let (source, _) = log.history_source().unwrap();
            let path = dir.path().join(&log.state().segments[0].name);
            let bytes = fs::read(&path).unwrap();
            let mut starts = vec![];
            let mut decoder = codec::Decoder::at(&bytes[84..], 1, 84);
            while let Some(frame) = decoder.next_frame().unwrap() {
                starts.push(frame.offset as usize);
            }
            Self {
                source,
                log,
                path,
                bytes,
                starts,
                _dir: dir,
            }
        }
        fn damage(&self) -> (String, u64, String) {
            self.source
                .frames(&mut |_, _, _| ControlFlow::Continue(()))
                .unwrap_err()
        }
    }

    /// S5(a): an interior payload corruption names the same boundary as replay.
    #[test]
    fn s5a_payload_crc_damage() {
        let mut f = DamageFixture::new();
        let start = f.starts[1];
        let structural =
            u32::from_le_bytes(f.bytes[start + 4..start + 8].try_into().unwrap()) as usize;
        f.bytes[start + 36 + structural] ^= 1;
        fs::write(&f.path, &f.bytes).unwrap();
        let replay = f
            .source
            .inspect(&fixture::Present, &mut |_| Ok(()), &mut |_, _| Ok(()))
            .unwrap();
        let refusal = replay.refusal.unwrap();
        let damage = f.damage();
        assert_eq!(damage.0, refusal.segment);
        assert_eq!(damage.1, refusal.offset);
        assert_eq!(damage.1, start as u64);
        let store::RefusalKind::Bytes(fault) = refusal.fault else {
            panic!("byte fault required")
        };
        assert_eq!(damage.2, fault.to_string());
    }

    /// S5(b): EOF on a frame boundary cannot shorten the captured prefix silently.
    #[test]
    fn s5b_exact_limit() {
        let f = DamageFixture::new();
        let cut = f.starts[2];
        fs::write(&f.path, &f.bytes[..cut]).unwrap();
        assert_eq!(
            f.damage(),
            (
                f.path.file_name().unwrap().to_str().unwrap().into(),
                cut as u64,
                "segment ends before its recorded limit".into()
            )
        );
    }

    /// S5(c): a zero-filled last frame is damage inside captured limits.
    #[test]
    fn s5c_zero_tail() {
        let mut f = DamageFixture::new();
        let start = f.starts[2];
        f.bytes[start..].fill(0);
        fs::write(&f.path, &f.bytes).unwrap();
        let damage = f.damage();
        assert_eq!(damage.1, start as u64);
        assert_eq!(damage.2, "segment ends before its recorded limit");
    }

    /// S5(d): a latched refold failure preserves every live byte limit.
    #[test]
    fn s5d_refused_limits() {
        let mut f = DamageFixture::new();
        let expected = collect(&f.source);
        let refusal = f
            .log
            .install_refold(Err(store::StoreError::Io(std::io::Error::other(
                "history test",
            ))))
            .unwrap();
        let (source, standing) = f.log.history_source().unwrap();
        assert_eq!(
            standing,
            HistoryStanding::Refused {
                refusal: format!("{refusal:?}")
            }
        );
        assert_eq!(collect(&source), expected);
    }

    /// S5(f): correct payload CRC cannot conceal a broken record hash chain.
    #[test]
    fn s5f_record_hash_damage() {
        let mut f = DamageFixture::new();
        let start = f.starts[1];
        let structural =
            u32::from_le_bytes(f.bytes[start + 4..start + 8].try_into().unwrap()) as usize;
        let payload = start + 36 + structural;
        f.bytes[payload] ^= 1;
        let crc = remanence_crc::crc64_xz(&f.bytes[payload..payload + 32]);
        f.bytes[payload + 32..payload + 40].copy_from_slice(&crc.to_le_bytes());
        fs::write(&f.path, &f.bytes).unwrap();
        let damage = f.damage();
        assert_eq!(damage.1, start as u64);
        assert_eq!(damage.2, "RecordHash");
    }

    /// S5(g): receipt parity spans rotations; the next header must bind the previous hash.
    #[test]
    fn s5g_segment_chain() {
        let dir = tempfile::tempdir().unwrap();
        let (mut log, clock) = fixture::open(dir.path(), &fixture::Present);
        for day in 0..3 {
            clock.0.store(
                fixture::NOW + day * 86_400_000_000_000,
                std::sync::atomic::Ordering::SeqCst,
            );
            fixture::put(&mut log, fixture::diagnostic());
        }
        assert_eq!(log.state().segments.len(), 3);
        let (source, _) = log.history_source().unwrap();
        let mut expected = vec![];
        let replay = source
            .inspect(&fixture::Present, &mut |_| Ok(()), &mut |_, receipt| {
                expected.push((receipt.sequence, receipt.record_uuid, receipt.record_hash));
                Ok(())
            })
            .unwrap();
        assert!(replay.refusal.is_none());
        assert_eq!(collect(&source), expected);
        let name = log.state().segments[1].name.clone();
        let path = dir.path().join(&name);
        let mut bytes = fs::read(&path).unwrap();
        bytes[34] ^= 1;
        let crc = remanence_crc::crc64_xz(&bytes[..76]);
        bytes[76..84].copy_from_slice(&crc.to_le_bytes());
        fs::write(path, bytes).unwrap();
        assert_eq!(
            source
                .frames(&mut |_, _, _| ControlFlow::Continue(()))
                .unwrap_err(),
            (name, 0, "PreviousSegmentHash".into())
        );
    }

    /// S5(h): ambiguity takes precedence when refusal is also latched.
    #[test]
    fn s5h_fenced_before_refused() {
        let mut f = DamageFixture::new();
        f.log
            .install_refold(Err(store::StoreError::Io(std::io::Error::other(
                "history test",
            ))))
            .unwrap();
        fs::write(f._dir.path().join("library-log.ambiguous"), []).unwrap();
        let (source, standing) = f.log.history_source().unwrap();
        assert_eq!(standing, HistoryStanding::Fenced);
        assert_eq!(collect(&source).len(), 3);
    }
    fn authority(root: &std::path::Path) -> LibraryAuthority {
        LibraryAuthority::open(
            &root.join("library"),
            fixture::LOG,
            store::Origin {
                host: "history-test".into(),
                pid: 1,
                build: "history-test".into(),
            },
            Box::new(fixture::FixedClock(std::sync::Arc::new(
                std::sync::atomic::AtomicU64::new(fixture::NOW),
            ))),
            Box::new(fixture::Ids),
            false,
            &fixture::Present,
            std::sync::Arc::new(crate::StateLockGuard::acquire(root).unwrap()),
        )
        .unwrap()
    }
    fn repeated_id(n: u8) -> String {
        format!("{n:02x}{n:02x}{n:02x}{n:02x}-{n:02x}{n:02x}-{n:02x}{n:02x}-{n:02x}{n:02x}-{n:02x}{n:02x}{n:02x}{n:02x}{n:02x}{n:02x}")
    }
    /// Written expectations follow the fixture's publication order, not the renderer.
    fn expected_heads() -> Vec<(String, String, Option<String>)> {
        let t1 = repeated_id(1);
        let t2 = repeated_id(2);
        let t3 = repeated_id(3);
        let d30 = repeated_id(30);
        let d31 = repeated_id(31);
        let cart = "3d1c420f-c7eb-5258-85b6-6d2751cd31cd";
        let reject = "d5d1d69c-916e-53ae-a5f9-7e36aea27f86";
        let bay = "bay:LIBA0001:0x0001";
        let other_bay = "bay:LIBB0002:0x0001";
        let standalone = "standalone:STANDALONE1";
        let mut out = vec![];
        let mut add = |event: &str, kind: &str, id: Option<&str>| {
            out.push((event.into(), kind.into(), id.map(str::to_owned)))
        };
        for (event, kind, id) in [
            ("Reprovision", "tape", t1.as_str()),
            ("Reprovision", "tape", t2.as_str()),
            ("Reprovision", "tape", t3.as_str()),
            ("ConfigurationAccepted", "configuration", "1"),
            ("AssignPool", "tape", t1.as_str()),
            ("AssignPool", "tape", t2.as_str()),
            ("Label", "tape", t2.as_str()),
            ("Label", "tape", t2.as_str()),
            ("MediumSpecSet", "tape", t1.as_str()),
            ("SupersedeTail", "tape", t1.as_str()),
            ("AcceptUnprotectedFinalization", "tape", t1.as_str()),
            ("FenceRaised", "tape", t1.as_str()),
            ("FenceReleased", "tape", t1.as_str()),
            ("AlarmChanged.Raise", "alarm", "contradicted-tape"),
            ("AlarmChanged.Raise", "alarm", "pending-record-rejected"),
            ("FenceRaised", "tape", t1.as_str()),
            ("FenceReleased", "tape", t1.as_str()),
            ("FenceRaised", "endpoint", other_bay),
            ("FenceReleased", "endpoint", other_bay),
            ("MotionIntent.Load", "endpoint", bay),
            ("MotionOutcome", "endpoint", bay),
            ("MotionIntent.Load", "endpoint", bay),
            ("ReadinessObserved", "endpoint", bay),
            ("MotionOutcome", "endpoint", bay),
            ("MotionIntent.Load", "endpoint", bay),
            ("InventoryReconciled", "library", "LIBA0001"),
            ("MotionIntent.Move", "library", "LIBA0001"),
            ("MotionOutcome", "library", "LIBA0001"),
            ("InventoryReconciled", "library", "LIBA0001"),
            ("MotionIntent.Unload", "endpoint", standalone),
            ("MotionOutcome", "endpoint", standalone),
            ("VerificationProgress", "tape", t1.as_str()),
            ("VerificationProgress", "tape", t1.as_str()),
            ("VerificationVerdict", "tape", t1.as_str()),
            ("VerificationVerdict", "tape", t1.as_str()),
            ("ManualFinalizationAccepted", "tape", t1.as_str()),
            ("MotionIntent.Load", "endpoint", standalone),
            ("ReadinessObserved", "endpoint", standalone),
            ("ReadinessObserved", "endpoint", standalone),
            ("DriveObserved", "drive", d30.as_str()),
            ("DriveObserved", "drive", d31.as_str()),
            ("DriveDecision.Retire", "drive", d30.as_str()),
            ("DriveDecision.Reinstate", "drive", d30.as_str()),
            ("DriveDecision.Annotate", "drive", d30.as_str()),
            ("DriveHealthObserved", "drive", d30.as_str()),
            ("CleaningRecorded.Register", "tape", cart),
            ("CleaningRecorded.Begin", "cleaning_run", "run"),
            ("CleaningRecorded.Advance", "cleaning_run", "run"),
            ("CleaningRecorded.Finish", "cleaning_run", "run"),
            ("CleaningRecorded.Expire", "tape", cart),
            ("CleaningRecorded.Register", "tape", reject),
            ("CleaningRecorded.Begin", "cleaning_run", "reject-run"),
            ("CleaningRecorded.Reject", "tape", reject),
            ("CleaningRecorded.Finish", "cleaning_run", "reject-run"),
            ("AlarmChanged.Raise", "alarm", "drive-fault"),
            ("AlarmChanged.Acknowledge", "alarm", "drive-fault"),
            ("AlarmChanged.Clear", "alarm", "drive-fault"),
        ] {
            add(event, kind, Some(id));
        }
        for event in [
            "Accepted",
            "Started",
            "CancelRequested",
            "CancellationRejected",
            "CompletedAfterCancel",
        ] {
            add(
                &format!("OperationRecorded.{event}"),
                "operation",
                Some(&repeated_id(40)),
            );
        }
        for (n, event) in [
            (41, "CancelledBeforeDispatch"),
            (42, "Finished"),
            (43, "Failed"),
            (44, "CompletionUnknown"),
        ] {
            add(
                "OperationRecorded.Started",
                "operation",
                Some(&repeated_id(n)),
            );
            add(
                &format!("OperationRecorded.{event}"),
                "operation",
                Some(&repeated_id(n)),
            );
        }
        for (n, event) in [(50, "Closed"), (51, "Orphaned"), (52, "LostByRestart")] {
            add("SessionObserved.Opened", "session", Some(&repeated_id(n)));
            add(
                &format!("SessionObserved.{event}"),
                "session",
                Some(&repeated_id(n)),
            );
        }
        for event in [
            "WrapMapCached.Cache",
            "WrapMapCached.Invalidate",
            "RecoveryRecorded.RepairApplied",
            "RecoveryRecorded.AppendAuthorityRecovered",
        ] {
            add(event, "tape", Some(&t1));
        }
        add("Diagnostic.HardwareWarning", "drive", Some("30"));
        for n in [240, 241] {
            add("Reprovision", "tape", Some(&repeated_id(n)));
        }
        for n in 0..3 {
            add(
                "ReadinessObserved",
                "endpoint",
                Some(&format!("standalone:L1-W-{n}")),
            );
        }
        for name in ["serial-collision", "retired-reappeared"] {
            add("AlarmChanged.Raise", "alarm", Some(name));
        }
        for _ in 0..5 {
            add("FenceRaised", "tape", Some(&t1));
            add("FenceReleased", "tape", Some(&t1));
        }
        for _ in 0..14 {
            add("Diagnostic.RefusalRecorded", "tape", Some("fixture"));
        }
        for _ in 0..4 {
            add("VerificationVerdict", "tape", Some(&t1));
        }
        for _ in 0..6 {
            add("Diagnostic.RequestReceived", "fixture", None);
        }
        add("Reprovision", "tape", Some(&repeated_id(6)));
        add(
            "Reprovision",
            "tape",
            Some("07070707-0707-4707-8707-070707070707"),
        );
        for (event, kind, id) in [
            ("DriveObserved", "drive", d31.as_str()),
            ("DriveDecision.Annotate", "drive", d30.as_str()),
            ("DriveHealthObserved", "drive", d30.as_str()),
            ("InventoryReconciled", "library", "LIBB0002"),
            ("CleaningRecorded.Begin", "cleaning_run", "optional-run"),
            ("CleaningRecorded.Finish", "cleaning_run", "optional-run"),
            ("AlarmChanged.Raise", "alarm", "optional-alarm"),
        ] {
            add(event, kind, Some(id));
        }
        for event in ["OperationRecorded.Accepted", "OperationRecorded.Failed"] {
            add(event, "operation", Some(&repeated_id(70)));
        }
        for event in ["SessionObserved.Opened", "SessionObserved.Closed"] {
            add(event, "session", Some(&repeated_id(71)));
        }
        add("Retire", "tape", Some(&t2));
        add("Retire", "tape", Some(&t3));
        add("Reprovision", "tape", Some(&repeated_id(4)));
        add(
            "Reprovision",
            "tape",
            Some("05050505-0505-4505-8505-050505050505"),
        );
        out
    }

    /// S2: authority snapshots preserve every event/subject, schema order and exact detail.
    #[test]
    fn s2_names_subjects_and_json() {
        let root = tempfile::tempdir().unwrap();
        lib::reference_fixtures::all_records(&root.path().join("library"));
        let authority = authority(root.path());
        let (snapshot, standing) = authority.history_snapshot().unwrap();
        let expected = expected_heads();
        assert_eq!(expected.len(), 139);
        let mut count = 0;
        let end = walk(&snapshot, standing, &mut |entry| {
            let head = entry.head();
            assert_eq!(
                (
                    head.event_kind.clone(),
                    head.subject_kind.clone(),
                    head.subject_id.clone()
                ),
                expected[count],
                "sequence {}",
                head.sequence
            );
            assert_eq!(head.sequence, count as u64 + 1);
            assert_eq!(head.software_build, "remanence-fixture");
            assert_eq!(head.actor, entry.record.envelope.actor);
            assert_eq!(head.source_layer, entry.record.envelope.source_layer);
            assert_eq!(
                head.timestamp_unix_ns,
                entry.record.envelope.timestamp_unix_ns
            );
            assert_eq!(head.operation_id, entry.record.envelope.operation_id);
            assert_eq!(head.session_id, entry.record.envelope.session_id);
            let detail = entry.detail_json();
            let value: serde_json::Value = serde_json::from_str(&detail).unwrap();
            assert!(value.get("host_id").is_none());
            rendered_body(&entry.record.body).check_fields();
            if head.sequence == 71 {
                assert_eq!(detail, concat!(
                    "{\"record_kind\":\"SessionObserved\",\"record_uuid\":\"c0000000-0000-0000-0000-000000000047\",",
                    "\"record_hash\":\"ed0ae28931807b7ac40eca0bc811cf7cb68cc5c97cce184cbd43a51496518b37\",",
                    "\"process_id\":4242,\"idempotency_key\":null,\"subject\":{\"kind\":\"session\",\"id\":\"32323232-3232-3232-3232-323232323232\"},",
                    "\"body\":{\"session_id\":\"32323232-3232-3232-3232-323232323232\",\"kind\":\"Read\",\"endpoint\":null,",
                    "\"tape\":{\"uuid\":\"01010101-0101-0101-0101-010101010101\",\"incarnation\":1},\"event\":\"Opened\",\"previous\":null,\"reason\":null}}"
                ));
            }
            count += 1;
            ControlFlow::Continue(())
        });
        assert_eq!(end, HistoryEnd::Clean);
        assert_eq!(count, 139);
    }

    /// S3: subject references are consumed once, including inventory resolution.
    #[test]
    fn s3_subject_map_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        lib::reference_fixtures::all_records(dir.path());
        let (log, _) = fixture::open(dir.path(), &fixture::Present);
        let (source, _) = log.history_source().unwrap();
        let mut walker = Walker::default();
        let mut seen = BTreeMap::new();
        assert_eq!(
            source.frames(&mut |record, sequence, _| {
                if sequence == 26 {
                    assert!(walker.subjects.contains_key(&25));
                }
                let subject = walker.subject(&record.body, sequence);
                if sequence == 13 {
                    assert_eq!(seen.get(&12), Some(&subject));
                    assert!(!walker.subjects.contains_key(&12));
                }
                if sequence == 21 {
                    assert_eq!(seen.get(&20), Some(&subject));
                    assert_eq!(subject.0, "endpoint");
                    assert!(!walker.subjects.contains_key(&20));
                }
                if sequence == 26 {
                    assert!(!walker.subjects.contains_key(&25));
                }
                seen.insert(sequence, subject);
                ControlFlow::Continue(())
            }),
            Ok(FramesEnd::Complete)
        );
        let release = Body::FenceReleased(FenceReleased {
            fence: 999,
            evidence: "missing".into(),
            forced: false,
            detail: Bounded(vec![]),
        });
        assert_eq!(
            Walker::default().subject(&release, 1000),
            subject("fence", "999")
        );
    }

    /// S7: subject rows absent from the corpus resolve through one history walker.
    #[test]
    fn s7_subject_rows_without_corpus_cover() {
        let mut walker = Walker::default();
        let raise = Body::FenceRaised(FenceRaised {
            kind: FenceKind::DriveFault,
            scope: FenceScope::Model {
                model: "ULT3580-TD9".into(),
                firmware: "Q3F7".into(),
            },
            cause: FenceCause::Cleaning,
            evidence: None,
            at: None,
            operation: None,
            detail: Bounded(vec![]),
        });
        assert_eq!(
            walker.subject(&raise, 40),
            ("model".into(), Some("ULT3580-TD9/Q3F7".into())),
            "model fence raise"
        );
        let release = Body::FenceReleased(FenceReleased {
            fence: 40,
            evidence: "settled".into(),
            forced: false,
            detail: Bounded(vec![]),
        });
        assert_eq!(
            walker.subject(&release, 41),
            ("model".into(), Some("ULT3580-TD9/Q3F7".into())),
            "model fence release"
        );

        let bay = DriveKey::Bay(Bay {
            library_serial: "LIB-1".into(),
            bay: 3,
        });
        let standalone = Standalone {
            drive_serial: "SN-9".into(),
        };
        let mut ready = ReadinessObserved {
            operation: [77; 16],
            endpoint: bay.clone(),
            tape: None,
            barcode: None,
            phase: ReadinessPhase::StartupReconcile,
            state: ReadinessState::AbortedUnknown,
            previous: None,
            started_unix_ns: 1,
            limit_ms: Some(1000),
            evidence: None,
            load_intent: None,
            ready_generation: 1,
            result: ReadinessResult::AbortedUnknown,
            scope: ReadinessScope::Library,
            media: None,
            source_slot: None,
            run: None,
            cancel_source: None,
        };
        assert_eq!(
            walker.subject(&Body::ReadinessObserved(ready.clone()), 42),
            ("library".into(), Some("LIB-1".into())),
            "library readiness at a bay"
        );
        ready.endpoint = DriveKey::Standalone(standalone.clone());
        assert_eq!(
            walker.subject(&Body::ReadinessObserved(ready.clone()), 43),
            ("endpoint".into(), Some("standalone:SN-9".into())),
            "library readiness at a standalone drive"
        );
        ready.scope = ReadinessScope::Endpoint;
        ready.endpoint = bay;
        assert_eq!(
            walker.subject(&Body::ReadinessObserved(ready), 44),
            ("endpoint".into(), Some("bay:LIB-1:0x0003".into())),
            "endpoint readiness at a bay"
        );

        let inventory = Body::InventoryReconciled(InventoryReconciled {
            subject: InventorySubject::Standalone(standalone),
            observation_generation: 1,
            resolves: Bounded(vec![]),
            complete: true,
            elements: Bounded(vec![]),
        });
        assert_eq!(
            walker.subject(&inventory, 45),
            ("endpoint".into(), Some("standalone:SN-9".into())),
            "standalone inventory"
        );

        let outcome = Body::MotionOutcome(MotionOutcome {
            intent: DecisionRef {
                sequence: 998,
                record_uuid: [98; 16],
            },
            result: MotionResult::Unknown,
            evidence: fixture::evidence(),
            confirmation: None,
        });
        assert_eq!(
            walker.subject(&outcome, 46),
            ("motion".into(), Some("998".into())),
            "motion outcome without its intent"
        );
    }

    /// S4: writer-accepted large blobs are summarized; rendered bodies keep counting past the cap.
    #[test]
    fn s4_blob_and_body_bounds() {
        let root = tempfile::tempdir().unwrap();
        let (mut log, _) = fixture::open(&root.path().join("library"), &fixture::Present);
        fixture::put(&mut log, fixture::provision(1, None));
        let descriptors = vec![0xab; 4 * 1024 * 1024];
        let digest: Hash = Sha256::digest(&descriptors).into();
        fixture::put(
            &mut log,
            Body::WrapMapCached(WrapMapCached {
                tape: fixture::tape(1),
                calibration_generation: 1,
                action: WrapAction::Cache(Cache {
                    descriptors: Blob(descriptors),
                    mapped_extent_lba: 1,
                    write_epoch: 1,
                    digest,
                }),
            }),
        );
        fixture::put(
            &mut log,
            Body::Diagnostic(Diagnostic {
                event: DiagnosticEvent::RequestReceived,
                subject_kind: "limit".into(),
                subject_id: None,
                detail: Bounded(
                    (0..256)
                        .map(|n| {
                            DiagnosticEntry(
                                format!("k{n:03}").as_str().into(),
                                DiagnosticValue::Bytes(Blob(vec![0xab; 4096])),
                            )
                        })
                        .collect(),
                ),
            }),
        );
        drop(log);
        let authority = authority(root.path());
        let (snapshot, standing) = authority.history_snapshot().unwrap();
        let mut count = 0;
        assert_eq!(
            walk(&snapshot, standing, &mut |entry| {
                count += 1;
                let value: serde_json::Value = serde_json::from_str(&entry.detail_json()).unwrap();
                if entry.head.sequence == 2 {
                    assert_eq!(
                        value["body"]["action"]["Cache"]["descriptors"],
                        serde_json::json!({"len":4194304,"sha256":hex(&digest)})
                    );
                }
                if entry.head.sequence == 3 {
                    let mut full = vec![];
                    rendered_body(&entry.record.body).render(&mut full).unwrap();
                    let body: serde_json::Value = serde_json::from_slice(&full).unwrap();
                    assert_eq!(body["detail"].as_array().unwrap().len(), 256);
                    for pair in body["detail"].as_array().unwrap() {
                        assert_eq!(pair[1], serde_json::json!({"hex":"ab".repeat(4096)}));
                    }
                    assert_eq!(
                        value["body"],
                        serde_json::json!({"omitted":{"rendered_bytes":full.len()}})
                    );
                    let mut counted = CountedBody::default();
                    rendered_body(&entry.record.body)
                        .render(&mut counted)
                        .unwrap();
                    assert_eq!(counted.rendered_bytes, full.len());
                    assert!(counted.rendered_bytes > DETAIL_LIMIT);
                    assert!(counted.bytes.len() <= DETAIL_LIMIT);
                }
                ControlFlow::Continue(())
            }),
            HistoryEnd::Clean
        );
        assert_eq!(count, 3);
    }

    /// S4: the exact whole-detail bound includes its outer keys, measured in UTF-8 bytes.
    #[test]
    fn s4_exact_whole_detail_bound() {
        let mut record = fixture::record(
            Body::Diagnostic(Diagnostic {
                event: DiagnosticEvent::RequestReceived,
                subject_kind: "é".into(),
                subject_id: None,
                detail: Bounded(
                    (0..256)
                        .map(|n| {
                            DiagnosticEntry(
                                format!("k{n:03}").as_str().into(),
                                DiagnosticValue::Text(if n < 255 {
                                    "x".repeat(4096).as_str().into()
                                } else {
                                    "".into()
                                }),
                            )
                        })
                        .collect(),
                ),
            }),
            1,
        );
        let mut walker = Walker::default();
        let initial = walker.entry(&record, 1, [1; 32]).detail_json().len();
        assert!(initial < DETAIL_LIMIT);
        let remaining = DETAIL_LIMIT - initial;
        assert!(remaining < 4096);
        let Body::Diagnostic(diagnostic) = &mut record.body else {
            unreachable!()
        };
        diagnostic.detail.0[255].1 = DiagnosticValue::Text("x".repeat(remaining).as_str().into());
        let root = tempfile::tempdir().unwrap();
        let (mut log, _) = fixture::open(&root.path().join("library"), &fixture::Present);
        // Envelope process_id and null idempotency have the same encoded lengths.
        fixture::put(&mut log, record.body.clone());
        let Body::Diagnostic(diagnostic) = &mut record.body else {
            unreachable!()
        };
        let DiagnosticValue::Text(text) = &mut diagnostic.detail.0[255].1 else {
            unreachable!()
        };
        text.0.push('x');
        fixture::put(&mut log, record.body);
        drop(log);
        let authority = authority(root.path());
        let (snapshot, standing) = authority.history_snapshot().unwrap();
        let mut count = 0;
        assert_eq!(
            walk(&snapshot, standing, &mut |entry| {
                let detail = entry.detail_json();
                let value: serde_json::Value = serde_json::from_str(&detail).unwrap();
                let mut full = vec![];
                rendered_body(&entry.record.body).render(&mut full).unwrap();
                let mut prefix = vec![];
                entry.prefix(&mut prefix).unwrap();
                if count == 0 {
                    assert_eq!(detail.len(), DETAIL_LIMIT);
                    assert!(value["body"].get("omitted").is_none());
                } else {
                    assert!(full.len() < DETAIL_LIMIT);
                    assert_eq!(prefix.len() + full.len() + 1, DETAIL_LIMIT + 1);
                    assert_eq!(
                        value["body"],
                        serde_json::json!({"omitted":{"rendered_bytes":full.len()}})
                    );
                }
                count += 1;
                ControlFlow::Continue(())
            }),
            HistoryEnd::Clean
        );
        assert_eq!(count, 2);
    }

    /// S5(a)-(c): byte damage is preserved through the public history end conversion.
    #[test]
    fn s5_damage_through_walk() {
        for case in 0..3 {
            let root = tempfile::tempdir().unwrap();
            let directory = root.path().join("library");
            let (mut log, _) = fixture::open(&directory, &fixture::Present);
            for _ in 0..3 {
                fixture::put(&mut log, fixture::diagnostic());
            }
            let name = log.state().segments[0].name.clone();
            drop(log);
            let authority = authority(root.path());
            let (snapshot, standing) = authority.history_snapshot().unwrap();
            let path = directory.join(&name);
            let mut bytes = fs::read(&path).unwrap();
            let mut starts = vec![];
            let mut decoder = codec::Decoder::at(&bytes[84..], 1, 84);
            while let Some(frame) = decoder.next_frame().unwrap() {
                starts.push(frame.offset as usize);
            }
            let offset = if case == 0 { starts[1] } else { starts[2] };
            match case {
                0 => {
                    let structural =
                        u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap())
                            as usize;
                    bytes[offset + 36 + structural] ^= 1;
                    fs::write(&path, &bytes).unwrap();
                }
                1 => fs::write(&path, &bytes[..offset]).unwrap(),
                _ => {
                    bytes[offset..].fill(0);
                    fs::write(&path, &bytes).unwrap();
                }
            }
            let error = snapshot
                .frames(&mut |_, _, _| ControlFlow::Continue(()))
                .unwrap_err();
            assert_eq!(error.0, name);
            assert_eq!(error.1, offset as u64);
            assert_eq!(
                walk(&snapshot, standing, &mut |_| ControlFlow::Continue(())),
                HistoryEnd::Damaged {
                    segment: error.0,
                    offset: error.1,
                    fault: error.2
                }
            );
        }
    }
}
