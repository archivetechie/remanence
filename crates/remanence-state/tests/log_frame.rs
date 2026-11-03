//! Independent version-9/two-tag family tests for the common framing and host
//! publication boundary. No tape schema, fold, header or record is used here.
use remanence_crc::crc64_xz;
use remanence_library::durable_file::{self, DurableDirectory, DurableFile};
use remanence_state::log_frame::{
    self as frame, publication, Decoder, Family, Fault, Section, Tail, TornKind,
};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicU8, Ordering},
    Arc,
};

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("frame: {0}")]
    Frame(#[from] Fault),
    #[error("family version/tag")]
    Identity,
    #[error("structural bytes not consumed exactly")]
    Structural,
    #[error("I/O: {0}")]
    Io(#[from] io::Error),
}
struct Tiny;
impl Family for Tiny {
    type Record = u8;
    type Fault = Error;
    type Error = Error;
    fn validate(version: u16, tag: u16) -> Result<(), Error> {
        if version == 9 && matches!(tag, 1 | 2) {
            Ok(())
        } else {
            Err(Error::Identity)
        }
    }
    fn validate_short_identity(prefix: &[u8; frame::PREFIX_LEN], n: usize) -> Result<(), Error> {
        if prefix[..n.min(2)] != 9u16.to_le_bytes()[..n.min(2)] {
            return Err(Error::Identity);
        }
        if n > 2 {
            Self::validate(9, u16::from_le_bytes(prefix[2..4].try_into().unwrap()))?;
        }
        Ok(())
    }
    fn valid_identity(version: u16, tag: u16) -> bool {
        Self::validate(version, tag).is_ok()
    }
    fn decode(_tag: u16, structural: &[u8]) -> Result<u8, Error> {
        match structural {
            [0x7a, value, 0x6b] => Ok(*value),
            _ => Err(Error::Structural),
        }
    }
    fn error(_offset: u64, _sequence: Option<u64>, fault: Error) -> Error {
        fault
    }
}
fn encoded(sequence: u64, tag: u16) -> Vec<u8> {
    let mut bytes = Vec::new();
    frame::encode_frame(
        &mut bytes,
        9,
        tag,
        sequence,
        &[0x7a, 0, 0x6b],
        &[0x31, 0, 0x32],
    )
    .unwrap();
    bytes
}
fn reprefix(bytes: &mut [u8]) {
    let crc = crc64_xz(&bytes[..20]);
    bytes[20..28].copy_from_slice(&crc.to_le_bytes());
}

#[test]
fn hostile_length_with_bad_prefix_crc_never_reads_or_allocates_a_body() {
    struct PrefixOnly {
        prefix: io::Cursor<Vec<u8>>,
    }
    impl Read for PrefixOnly {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            assert!(
                out.len() <= frame::PREFIX_LEN,
                "body allocation/read before prefix CRC"
            );
            assert!(
                self.prefix.position() < frame::PREFIX_LEN as u64,
                "read past bad prefix"
            );
            self.prefix.read(out)
        }
    }
    let mut bytes = encoded(1, 1);
    bytes[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    let mut decoder = Decoder::<_, Tiny>::new(PrefixOnly {
        prefix: io::Cursor::new(bytes[..28].to_vec()),
    });
    assert!(matches!(
        decoder.next_frame(),
        Err(Error::Frame(Fault::Crc(Section::Prefix)))
    ));
    assert_eq!(decoder.retained_buffer_bytes(), 0);
    assert!(
        decoder.next_frame().unwrap().is_none(),
        "an error halts scanning"
    );
}

#[test]
fn independent_section_crcs_refuse_complete_corruption() {
    for (at, section) in [(28, Section::Structural), (28 + 3 + 8, Section::Payload)] {
        let mut bytes = encoded(1, 1);
        bytes[at] ^= 0x40;
        let mut decoder = Decoder::<_, Tiny>::new(bytes.as_slice());
        match decoder.next_frame() {
            Err(Error::Frame(Fault::Crc(actual))) => assert_eq!(actual, section),
            other => panic!("{section:?}: {other:?}"),
        }
    }
}

#[test]
fn every_byte_cut_of_two_frames_has_only_the_complete_prefix() {
    let first = encoded(1, 1);
    let bytes = [first.as_slice(), encoded(2, 2).as_slice()].concat();
    for cut in 0..=bytes.len() {
        let mut decoder = Decoder::<_, Tiny>::new(&bytes[..cut]);
        let mut count = 0;
        while let Some(frame) = decoder.next_frame().unwrap() {
            count += 1;
            assert_eq!(frame.sequence, count);
            assert_eq!(frame.record, 0);
            assert_eq!(frame.payload, &[0x31, 0, 0x32]);
        }
        assert_eq!(count as usize, cut / first.len(), "cut {cut}");
        if cut % first.len() == 0 {
            assert_eq!(decoder.tail(), Tail::Complete);
        } else {
            let Tail::Torn { offset, kind } = decoder.tail() else {
                panic!("cut {cut}");
            };
            assert_eq!(offset as usize, cut / first.len() * first.len());
            if cut % first.len() < frame::PREFIX_LEN {
                assert_eq!(kind, TornKind::ShortPrefix);
            } else {
                assert!(matches!(kind, TornKind::ShortBody | TornKind::ZeroFilled));
            }
        }
    }
}

#[test]
fn combined_cap_family_validation_and_exact_consumption() {
    for field in [0, 2, 4] {
        let mut bytes = encoded(1, 1);
        match field {
            0 => bytes[..2].copy_from_slice(&2u16.to_le_bytes()),
            2 => bytes[2..4].copy_from_slice(&3u16.to_le_bytes()),
            _ => bytes[4..8].copy_from_slice(&(frame::MAX_FRAME_BODY as u32).to_le_bytes()),
        }
        reprefix(&mut bytes);
        let mut decoder = Decoder::<_, Tiny>::new(bytes.as_slice());
        let result = decoder.next_frame();
        if field == 4 {
            assert!(matches!(result, Err(Error::Frame(Fault::FrameTooLarge))));
        } else {
            assert!(matches!(result, Err(Error::Identity)));
        }
        assert_eq!(decoder.retained_buffer_bytes(), 0);
    }
    let mut bytes = Vec::new();
    frame::encode_frame(&mut bytes, 9, 1, 1, &[0x7a, 0, 0x6b, 0xff], &[]).unwrap();
    assert!(matches!(
        Decoder::<_, Tiny>::new(bytes.as_slice()).next_frame(),
        Err(Error::Structural)
    ));
    let mut bytes = encoded(1, 1);
    bytes.extend_from_slice(b"trailing garbage");
    let mut decoder = Decoder::<_, Tiny>::new(bytes.as_slice());
    assert!(decoder.next_frame().unwrap().is_some());
    assert!(matches!(decoder.next_frame(), Err(Error::Identity)));
}

#[test]
fn short_body_internal_zero_followed_by_data_stays_torn() {
    let bytes = encoded(1, 1);
    let mut decoder = Decoder::<_, Tiny>::new(&bytes[..frame::PREFIX_LEN + 3]);
    assert!(decoder.next_frame().unwrap().is_none());
    assert_eq!(
        decoder.tail(),
        Tail::Torn {
            offset: 0,
            kind: TornKind::ShortBody
        }
    );
}

struct Injection {
    stage: AtomicU8,
    after: bool,
}
impl Injection {
    fn trip(&self, stage: u8, effect: impl FnOnce() -> io::Result<()>) -> io::Result<()> {
        if self
            .stage
            .compare_exchange(stage, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            if self.after {
                effect()?;
            }
            Err(io::Error::other("injected unknown publication outcome"))
        } else {
            Ok(())
        }
    }
}
struct FaultFile {
    path: PathBuf,
    injection: Arc<Injection>,
}
impl DurableFile for FaultFile {
    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.injection.trip(1, || {
            fs::OpenOptions::new()
                .append(true)
                .open(&self.path)?
                .write_all(bytes)
        })
    }
    fn sync(&mut self) -> io::Result<()> {
        self.injection
            .trip(2, || fs::File::open(&self.path)?.sync_all())
    }
}
struct FaultDirectory(Arc<Injection>);
impl DurableDirectory for FaultDirectory {
    fn create(&self, path: &Path) -> io::Result<Box<dyn DurableFile>> {
        self.open(path)
    }
    fn open(&self, path: &Path) -> io::Result<Box<dyn DurableFile>> {
        Ok(Box::new(FaultFile {
            path: path.into(),
            injection: self.0.clone(),
        }))
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.0.trip(3, || fs::rename(from, to))
    }
    fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.rename(from, to)
    }
    fn sync(&self, path: &Path) -> io::Result<()> {
        self.0.trip(4, || fs::File::open(path)?.sync_all())
    }
    fn set_len(&self, _: &Path, _: u64) -> io::Result<()> {
        panic!("publication must never truncate")
    }
}

#[test]
fn unknown_publication_cuts_leave_absence_or_complete_file_and_sticky_marker() {
    for stage in 1..=4 {
        for after in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let injection = Arc::new(Injection {
                stage: AtomicU8::new(stage),
                after,
            });
            let _registration = durable_file::test_support::register(
                root.path().into(),
                Arc::new(FaultDirectory(injection.clone())),
            );
            let target = root.path().join("tiny.log");
            let mut temp = publication::Temporary::new(root.path(), "tiny", "initial").unwrap();
            let bytes = [encoded(1, 1), encoded(2, 2)].concat();
            assert!(
                publication::append_and_publish(
                    &mut temp.file,
                    &bytes,
                    Some(&temp.path),
                    &target,
                    root.path(),
                    || Ok(())
                )
                .is_err(),
                "stage {stage}, after={after}"
            );
            assert_eq!(injection.stage.load(Ordering::SeqCst), 0, "fault reached");
            assert!(publication::has_ambiguity(&target).unwrap());
            assert!(target.with_extension("ambiguous").exists());
            publication::clear_process_fence(&target);
            assert!(
                publication::has_ambiguity(&target).unwrap(),
                "marker survives owner loss"
            );
            if target.exists() {
                assert_eq!(fs::read(&target).unwrap(), bytes);
            } else {
                assert!(stage < 4 && !(stage == 3 && after));
            }
        }
    }
}

#[test]
fn no_replace_and_immutable_prefix_replacement_preserve_old_readers() {
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("tiny.log");
    let original = [encoded(1, 1), encoded(2, 2)].concat();
    fs::write(&target, &original).unwrap();
    let mut reader = fs::File::open(&target).unwrap();
    let mut temp = publication::Temporary::new(root.path(), "tiny", "initial").unwrap();
    assert!(publication::append_and_publish(
        &mut temp.file,
        b"replacement",
        Some(&temp.path),
        &target,
        root.path(),
        || Ok(())
    )
    .is_err());
    assert_eq!(fs::read(&target).unwrap(), original);
    publication::replace_prefix(root.path(), &target, "tiny", encoded(1, 1).len() as u64).unwrap();
    assert_eq!(fs::read(&target).unwrap(), encoded(1, 1));
    let mut old = Vec::new();
    reader.read_to_end(&mut old).unwrap();
    assert_eq!(old, original);
    assert!(
        publication::has_ambiguity(&target).unwrap(),
        "replacement alone never clears authority ambiguity"
    );
}

/// The Family contract makes a one-byte version distinguishable from padding.
#[test]
fn family_versions_have_nonzero_low_bytes_and_short_identity() {
    assert_ne!(remanence_state::tape_log::VERSION & 0xff, 0);
    assert!(Tiny::validate(256, 1).is_err());
    assert!(!Tiny::valid_identity(256, 1));
    let mut prefix = [0; frame::PREFIX_LEN];
    prefix[..2].copy_from_slice(&256u16.to_le_bytes());
    assert!(Tiny::validate_short_identity(&prefix, 1).is_err());
    let bytes = encoded(1, 1);
    assert_ne!(bytes[0], 0);
    let mut decoder = Decoder::<_, Tiny>::new(&bytes[..1]);
    assert!(decoder.next_frame().unwrap().is_none());
    assert_eq!(
        decoder.tail(),
        Tail::Torn {
            offset: 0,
            kind: TornKind::ShortPrefix
        }
    );
    let mut padding = Decoder::<_, Tiny>::new(&[0u8][..]);
    assert!(padding.next_frame().unwrap().is_none());
    assert_eq!(
        padding.tail(),
        Tail::Torn {
            offset: 0,
            kind: TornKind::ZeroFilled
        }
    );
}

/// A corrupt private spool must fail before a frame or progress is published.
#[test]
fn copy_frames_reports_prefix_crc_separately_from_size() {
    let mut bytes = encoded(1, 1);
    bytes[20] ^= 1;
    let mut output = Vec::new();
    let mut progress = 0;
    let error = frame::copy_frames(
        &mut bytes.as_slice(),
        &mut output,
        bytes.len() as u64,
        |n| progress += n,
    )
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(error.to_string(), "invalid preflighted frame prefix CRC");
    assert!(output.is_empty());
    assert_eq!(progress, 0);
}

/// Unsynced diagnostics skip file sync, but an uncertain write still fences.
#[test]
fn unsynced_append_fences_before_and_after_landed_write() {
    for after in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("diagnostic.log");
        fs::write(&target, b"prefix").unwrap();
        let injection = Arc::new(Injection {
            stage: AtomicU8::new(1),
            after,
        });
        let _registration = durable_file::test_support::register(
            root.path().into(),
            Arc::new(FaultDirectory(injection.clone())),
        );
        let mut output = durable_file::File::options()
            .append(true)
            .open(&target)
            .unwrap();
        assert!(
            publication::append_without_sync(&mut output, b"claim", &target, root.path()).is_err()
        );
        assert_eq!(injection.stage.load(Ordering::SeqCst), 0);
        assert!(publication::has_ambiguity(&target).unwrap());
        assert_eq!(
            fs::read(&target).unwrap(),
            if after {
                b"prefixclaim".as_slice()
            } else {
                b"prefix".as_slice()
            }
        );
    }
}

/// A diagnostic success leaves the sync fault armed for a later authority sync.
#[test]
fn unsynced_append_defers_sync_until_authority() {
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("diagnostic.log");
    fs::write(&target, b"prefix").unwrap();
    let injection = Arc::new(Injection {
        stage: AtomicU8::new(2),
        after: false,
    });
    let _registration = durable_file::test_support::register(
        root.path().into(),
        Arc::new(FaultDirectory(injection.clone())),
    );
    let mut output = durable_file::File::options()
        .append(true)
        .open(&target)
        .unwrap();
    publication::append_without_sync(&mut output, b"diagnostic", &target, root.path()).unwrap();
    assert_eq!(
        injection.stage.load(Ordering::SeqCst),
        2,
        "diagnostic did not sync"
    );
    assert!(!publication::has_ambiguity(&target).unwrap());
    assert!(publication::append_and_publish(
        &mut output,
        b"authority",
        None,
        &target,
        root.path(),
        || Ok(())
    )
    .is_err());
    assert_eq!(
        injection.stage.load(Ordering::SeqCst),
        0,
        "authority reached sync"
    );
    assert!(publication::has_ambiguity(&target).unwrap());
    assert_eq!(fs::read(&target).unwrap(), b"prefixdiagnosticauthority");
}
