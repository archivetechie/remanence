//! Primitive validation, capacity, encoding and transfer tests. Session proofs live in engine::drive_adapter::tests::pool_sessions.
use std::collections::{HashSet, VecDeque};
use std::fs::{self, File};
use std::io::Read;
use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use remanence_format::{RemTarObjectOptions, FORMAT_ID};
use remanence_library::{
    BlockSink, PipelinedWriteDiagnostics, TapeConfig, TapeIoError, TapePosition, VecBlockSink,
    WormMediaState, WriteBatchOutcome, WriteFilemarksOutcome, WriteOutcome,
};
use remanence_parity::{
    bootstrap::write_bootstrap_block, BlockSinkRawTapeSink, CapacityReserveCause, ParityConfig,
    ParityError, ParityScheme, ParitySink, RawTapeSink, SchemeId,
    TerminalTripleCapacityRuntimeState,
};
use remanence_state::{
    watermark_floor_bytes, CatalogIndex, TapePoolConfig, TapeRecord,
    OBJECT_COPY_REPRESENTATION_PLAINTEXT,
};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::pb;
use crate::pool_selection::{FillOldest, PoolSelectionContext, PoolSelectionPolicy, Selection};

use super::*;
use super::{capacity::*, no_parity::*, overlap::*, prepare::*, staging::*};
use crate::tape_init::{build_tape_bootstrap, write_tape_bootstrap};

#[test]
fn rewritable_object_media_gate_fails_closed_for_worm_and_unknown() {
    let config = |write_protected, worm| TapeConfig {
        density_code: None,
        block_size: remanence_library::BlockSize::Variable,
        compression: false,
        max_block_size_bytes: 8 * 1024 * 1024,
        write_protected,
        worm,
    };

    require_rewritable_object_media(config(false, WormMediaState::NotWorm))
        .expect("positive rewritable evidence is admitted");
    assert_eq!(
        require_rewritable_object_media(config(false, WormMediaState::Worm)),
        Err(ObjectWriteMediaError::Worm)
    );
    assert_eq!(
        require_rewritable_object_media(config(false, WormMediaState::Unknown)),
        Err(ObjectWriteMediaError::UnknownWormState)
    );
    assert_eq!(
        require_rewritable_object_media(config(true, WormMediaState::NotWorm)),
        Err(ObjectWriteMediaError::WriteProtected)
    );
}

#[test]
fn shared_admission_authority_excludes_both_identities() {
    let first = crate::write_admission::WriteAdmissionCoordinator::default();
    let second = first.clone();
    let object_id = [0x5A; 16];
    let held = first
        .reserve("direct-shared", "same-caller", Some(object_id))
        .expect("first owner reserves both identities");

    let replay_conflict = second
        .reserve("direct-shared", "same-caller", None)
        .expect_err("another coordinator handle shares replay-key authority");
    assert_eq!(replay_conflict.code(), tonic::Code::Aborted);
    let uuid_conflict = second
        .reserve("other-pool", "other-caller", Some(object_id))
        .expect_err("canonical Object UUID authority spans pools in this coordinator");
    assert_eq!(uuid_conflict.code(), tonic::Code::Aborted);

    drop(held);
    second
        .reserve("direct-shared", "same-caller", Some(object_id))
        .expect("a non-durable claim releases when its owner drops");
}

fn test_capacity_pool_config(selected: &SelectedTape) -> TapePoolConfig {
    TapePoolConfig {
        scheme: None,
        id: selected.pool_id.clone(),
        display_name: None,
        copy_class: None,
        content_class: None,
        selection_policy: Default::default(),
        watermark_low: 0.92,
        watermark_high: 0.97,
        capacity_cap_bytes: None,
        block_size_bytes: u64::from(selected.block_size),
        min_object_size_bytes: 0,
    }
}

/// Build a deterministic public recipient for encrypted pool-write tests.

#[test]
fn canonical_plaintext_object_is_validated_and_written_verbatim() {
    const BLOCK_SIZE: usize = 4096;
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("multi.rem-object");
    let object_uuid = Uuid::from_bytes([0x41; 16]);
    let caller_object_id = "canonical-caller-41";
    let mut options = RemTarObjectOptions::new(
        object_uuid.to_string(),
        caller_object_id,
        "2026-08-11T09:30:00Z",
        Uuid::from_bytes([0x42; 16]).to_string(),
    );
    options.chunk_size = BLOCK_SIZE;
    let first = b"first canonical member\n".repeat(300);
    let second = b"second canonical member\n".repeat(500);
    let files = [
        remanence_format::RemTarFile {
            path: "tree/first.txt",
            file_id: "41414141-4141-4141-4141-414141414142",
            data: &first,
            mtime: None,
            executable: Some(false),
        },
        remanence_format::RemTarFile {
            path: "tree/second.txt",
            file_id: "41414141-4141-4141-4141-414141414143",
            data: &second,
            mtime: None,
            executable: Some(false),
        },
    ];
    let mut file_sink = remanence_library::FileBlockSink::create(&path, BLOCK_SIZE)
        .expect("create canonical object");
    let layout = remanence_format::write_rem_tar_object(&mut file_sink, &options, &files)
        .expect("build canonical object");
    file_sink.sync_all().expect("sync canonical object");
    drop(file_sink);
    let digest = sha256_file(&path).expect("hash canonical object");
    let request = WriteObjectToPoolRequest {
        pool_id: "canonical-pool".to_string(),
        source: WriteObjectSource::Path(path.clone()),
        archive_path: PathBuf::new(),
        caller_object_id: caller_object_id.to_string(),
        expected_content_sha256: Some(digest),
        expected_object_id: Some(*object_uuid.as_bytes()),
        input_kind: WriteObjectInputKind::CanonicalPlaintextRemObject,
        representation: PoolWriteRepresentation::Plaintext,
    };

    let prepared = prepare_pool_object(&request, BLOCK_SIZE as u32)
        .expect("validate canonical object before tape motion");
    assert_eq!(prepared.object_uuid, object_uuid);
    assert_eq!(prepared.options.caller_object_id, caller_object_id);
    assert_eq!(prepared.files.len(), 2);
    assert_eq!(
        prepared.plan.layout.projected_size_blocks,
        layout.projected_size_blocks
    );

    let mut written = VecBlockSink::new();
    assert_eq!(
        write_canonical_plaintext_blocks(&mut written, &prepared).expect("write canonical blocks"),
        digest
    );
    assert_eq!(written.blocks.concat(), std::fs::read(&path).unwrap());

    let noncanonical_path = temp.path().join("noncanonical-mode.rem-object");
    let mut noncanonical = std::fs::read(&path).expect("read canonical bytes");
    let header_offset = usize::try_from(layout.files[0].data_offset)
        .expect("header offset fits usize")
        .checked_sub(512)
        .expect("payload follows a ustar header");
    let header = &mut noncanonical[header_offset..header_offset + 512];
    header[100..108].copy_from_slice(b"0000777\0");
    header[148..156].fill(b' ');
    let checksum: u64 = header.iter().map(|byte| u64::from(*byte)).sum();
    header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
    std::fs::write(&noncanonical_path, noncanonical).expect("write noncanonical object");
    let noncanonical_request = WriteObjectToPoolRequest {
        pool_id: "canonical-pool".to_string(),
        source: WriteObjectSource::Path(noncanonical_path),
        archive_path: PathBuf::new(),
        caller_object_id: caller_object_id.to_string(),
        expected_content_sha256: None,
        expected_object_id: Some(*object_uuid.as_bytes()),
        input_kind: WriteObjectInputKind::CanonicalPlaintextRemObject,
        representation: PoolWriteRepresentation::Plaintext,
    };
    let error = match prepare_pool_object(&noncanonical_request, BLOCK_SIZE as u32) {
        Ok(_) => panic!("noncanonical ustar mode must fail exact regeneration"),
        Err(error) => error,
    };
    assert!(matches!(error, PoolWriteError::InvalidInput(_)), "{error}");
    assert!(error.to_string().contains("deterministic writer output"));

    let missing_identity_guard = WriteObjectToPoolRequest {
        pool_id: "canonical-pool".to_string(),
        source: WriteObjectSource::Path(path),
        archive_path: PathBuf::new(),
        caller_object_id: caller_object_id.to_string(),
        expected_content_sha256: Some(digest),
        expected_object_id: None,
        input_kind: WriteObjectInputKind::CanonicalPlaintextRemObject,
        representation: PoolWriteRepresentation::Plaintext,
    };
    let error = match prepare_pool_object(&missing_identity_guard, BLOCK_SIZE as u32) {
        Ok(_) => panic!("canonical object identity guard is mandatory"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("requires expected_object_id"));
}

#[test]
fn canonical_member_ranges_share_one_spool_descriptor() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("many-members.bin");
    std::fs::write(&path, vec![0x5a; 4096]).expect("write shared spool");
    let source = Arc::new(Mutex::new(File::open(&path).expect("open shared spool")));
    let mut readers = (0..4096)
        .map(|offset| SharedFileRangeReader::new(Arc::clone(&source), (offset % 4096) as u64, 1))
        .collect::<Vec<_>>();
    assert_eq!(Arc::strong_count(&source), readers.len() + 1);
    assert!(
        readers
            .iter()
            .all(|reader| Arc::ptr_eq(&reader.source, &source)),
        "every member range must reference the one shared spool File"
    );
    for reader in &mut readers {
        let mut byte = [0u8; 1];
        reader
            .read_exact(&mut byte)
            .expect("read shared member range");
        assert_eq!(byte, [0x5a]);
    }
}

#[test]
fn cloned_io_memory_reservations_share_one_atomic_budget() {
    let resources = crate::io_memory::IoMemoryReservation::new(10).expect("shared resources");
    let clone = resources.clone();
    let held = resources
        .try_reserve(7)
        .expect("first clone reserves bytes");

    assert_eq!(
        clone.try_reserve_with_available(4).unwrap_err(),
        3,
        "the second clone must observe the first clone's live grant"
    );
    drop(held);
    assert!(clone.try_reserve_with_available(10).is_ok());
}

#[test]
fn pool_write_result_rejects_physical_used_byte_overflow() {
    assert!(matches!(
        checked_physical_used_bytes(u64::MAX, 2),
        Err(PoolWriteError::PhysicalUsedBytesOverflow {
            position_lba: u64::MAX,
            block_size: 2,
        })
    ));
    assert_eq!(checked_physical_used_bytes(u64::MAX, 1).unwrap(), u64::MAX);
}

#[derive(Debug)]
struct StagedTestSink {
    inner: VecBlockSink,
    batch_blocks: u32,
    fail_on_batch_call: Option<u64>,
    batch_error: Option<TapeIoError>,
    early_warning_on_batch_call: Option<u64>,
    fail_space_to_eod: bool,
    fail_position: bool,
    fail_filemark: bool,
    pending_deferred_audit: bool,
    audited_partial_sense: bool,
    batch_calls: u64,
    events: Vec<String>,
    ring_buffers: u32,
    cdbs: Vec<Vec<u8>>,
    alignments: Vec<usize>,
    diagnostic_ioctl_samples: u64,
    diagnostic_resets: u64,
    diagnostic_publications: u64,
    ordered_events: Arc<Mutex<Vec<String>>>,
    position_overrides: VecDeque<TapePosition>,
    /// Receives the running count of `position()` calls, so a test can act at
    /// an exact point in the writer's sequence instead of racing it.
    position_notify: Option<std_mpsc::Sender<usize>>,
}

impl StagedTestSink {
    fn new(batch_blocks: u32) -> Self {
        assert!(batch_blocks > 1, "staged test must exercise batching");
        Self {
            inner: VecBlockSink::new(),
            batch_blocks,
            fail_on_batch_call: None,
            batch_error: None,
            early_warning_on_batch_call: None,
            fail_space_to_eod: false,
            fail_position: false,
            fail_filemark: false,
            pending_deferred_audit: false,
            audited_partial_sense: false,
            batch_calls: 0,
            events: Vec::new(),
            ring_buffers: remanence_library::DEFAULT_TAPE_IO_STAGING_RING_BUFFERS,
            cdbs: Vec::new(),
            alignments: Vec::new(),
            diagnostic_ioctl_samples: 0,
            diagnostic_resets: 0,
            diagnostic_publications: 0,
            ordered_events: Arc::new(Mutex::new(Vec::new())),
            position_overrides: VecDeque::new(),
            position_notify: None,
        }
    }

    fn failing_on_batch(batch_blocks: u32, fail_on_batch_call: u64) -> Self {
        let mut sink = Self::new(batch_blocks);
        sink.fail_on_batch_call = Some(fail_on_batch_call);
        sink
    }

    fn with_ring(batch_blocks: u32, ring_buffers: u32) -> Self {
        let mut sink = Self::new(batch_blocks);
        sink.ring_buffers = ring_buffers;
        sink
    }
}

impl BlockSink for StagedTestSink {
    fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
        self.events.push(format!("write_block:{}", buf.len()));
        self.inner.write_block(buf)
    }

    fn write_block_batch(
        &mut self,
        buf: &[u8],
        block_size_bytes: u32,
    ) -> Result<WriteBatchOutcome, TapeIoError> {
        let records = records_in_staged_batch(buf, block_size_bytes)
            .expect("test batch contains whole records");
        self.batch_calls = self.batch_calls.saturating_add(1);
        self.diagnostic_ioctl_samples = self.diagnostic_ioctl_samples.saturating_add(1);
        self.events.push(format!("write_batch:{records}"));
        if let Some(error) = self.batch_error.take() {
            return Err(error);
        }
        if self.fail_on_batch_call == Some(self.batch_calls) {
            return Err(TapeIoError::OperationFailed(format!(
                "injected sink failure on batch {}",
                self.batch_calls
            )));
        }
        let outcome = self.inner.write_block_batch(buf, block_size_bytes)?;
        if self.early_warning_on_batch_call == Some(self.batch_calls) {
            Ok(WriteBatchOutcome::from_computed_position(
                outcome.records_written,
                outcome.bytes_written,
                true,
                false,
                outcome.position_after,
            ))
        } else {
            Ok(outcome)
        }
    }

    fn write_block_batch_pipelined(
        &mut self,
        buf: &[u8],
        block_size_bytes: u32,
        cdb: &[u8],
    ) -> Result<WriteBatchOutcome, TapeIoError> {
        self.cdbs.push(cdb.to_vec());
        self.alignments
            .push((buf.as_ptr() as usize) % system_page_size());
        self.ordered_events
            .lock()
            .expect("ordered events")
            .push("classify".into());
        self.write_block_batch(buf, block_size_bytes)
    }

    fn write_batch_blocks(&self, _block_size_bytes: u32) -> u32 {
        self.batch_blocks
    }

    fn requested_write_batch_blocks(&self) -> u32 {
        self.batch_blocks
    }

    fn staging_ring_buffers(&self) -> u32 {
        self.ring_buffers
    }

    fn pipelined_write_diagnostics(&self) -> PipelinedWriteDiagnostics {
        PipelinedWriteDiagnostics {
            ioctl_samples: self.diagnostic_ioctl_samples,
            ioctl_max_us: self.diagnostic_ioctl_samples.saturating_mul(1_000),
            ..PipelinedWriteDiagnostics::default()
        }
    }

    fn reset_pipelined_write_diagnostics(&mut self) {
        self.diagnostic_ioctl_samples = 0;
        self.diagnostic_resets = self.diagnostic_resets.saturating_add(1);
    }

    fn publish_pipelined_write_diagnostics(&mut self) {
        self.diagnostic_publications += 1;
    }

    fn begin_pipelined_write_window(
        &mut self,
        command_count: u32,
        bytes: u64,
        first_records: u32,
        last_records: u32,
    ) {
        self.events.push(format!(
            "intent:{command_count}:{bytes}:{first_records}:{last_records}"
        ));
    }

    fn finish_pipelined_write_window_success(
        &mut self,
        command_count: u32,
        bytes: u64,
        first_records: u32,
        last_records: u32,
        _duration: Duration,
    ) {
        self.events.push(format!(
            "span_ok:{command_count}:{bytes}:{first_records}:{last_records}"
        ));
    }

    fn finish_pipelined_write_window_error(
        &mut self,
        _command_count: u32,
        _bytes: u64,
        _first_records: u32,
        _last_records: u32,
        error: &TapeIoError,
    ) {
        self.audited_partial_sense = matches!(
            error,
            TapeIoError::PartialBatchUncommittable { sense: Some(_), .. }
        );
        self.ordered_events
            .lock()
            .expect("ordered events")
            .push("audit".into());
        self.events.push("span_error".into());
    }

    fn flush_pending_pipeline_audit(&mut self) {
        if self.pending_deferred_audit {
            self.pending_deferred_audit = false;
            self.ordered_events
                .lock()
                .expect("ordered events")
                .push("audit".into());
        }
    }

    fn write_filemarks(&mut self, count: u32) -> Result<WriteFilemarksOutcome, TapeIoError> {
        self.events.push(format!("filemark:{count}"));
        self.inner.write_filemarks(count)
    }

    fn write_filemarks_immediate(&mut self, count: u32) -> Result<(), TapeIoError> {
        self.events.push(format!("filemark_immediate:{count}"));
        self.inner.write_filemarks(count).map(|_| ())
    }

    fn write_filemarks_pipelined(
        &mut self,
        count: u32,
    ) -> Result<WriteFilemarksOutcome, TapeIoError> {
        if self.fail_filemark {
            self.pending_deferred_audit = true;
            return Err(TapeIoError::OperationFailed(
                "injected WRITE FILEMARKS failure".into(),
            ));
        }
        self.write_filemarks(count)
    }

    fn space_to_end_of_data(&mut self) -> Result<TapePosition, TapeIoError> {
        self.events.push("space_eod".to_string());
        if self.fail_space_to_eod {
            return Err(TapeIoError::OperationFailed(
                "injected space-to-EOD failure".into(),
            ));
        }
        self.inner.space_to_end_of_data()
    }

    fn locate(&mut self, lba: u64) -> Result<TapePosition, TapeIoError> {
        self.events.push(format!("locate:{lba}"));
        self.inner.locate(lba)
    }

    fn position(&mut self) -> Result<TapePosition, TapeIoError> {
        self.events.push("position".to_string());
        if let Some(notify) = &self.position_notify {
            let calls = self
                .events
                .iter()
                .filter(|event| *event == "position")
                .count();
            let _ = notify.send(calls);
        }
        if self.fail_position {
            return Err(TapeIoError::OperationFailed(
                "injected READ POSITION failure".into(),
            ));
        }
        match self.position_overrides.pop_front() {
            Some(position) => Ok(position),
            None => self.inner.position(),
        }
    }
}

fn tape_position_with_warning(block_position_end_of_warning: bool) -> TapePosition {
    TapePosition {
        lba: 0,
        partition: 0,
        beginning_of_partition: false,
        end_of_partition: false,
        block_position_end_of_warning,
    }
}

#[derive(Debug)]
struct SingleBlockOutcomeSink {
    bytes_written: u32,
    early_warning: bool,
    end_of_medium: bool,
    writes: u64,
}

impl BlockSink for SingleBlockOutcomeSink {
    fn write_block(&mut self, _buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
        self.writes = self.writes.saturating_add(1);
        Ok(WriteOutcome::from_computed_position(
            self.bytes_written,
            self.early_warning,
            self.end_of_medium,
            TapePosition {
                lba: self.writes,
                partition: 0,
                beginning_of_partition: false,
                end_of_partition: false,
                block_position_end_of_warning: self.early_warning,
            },
        ))
    }

    fn write_filemarks(&mut self, _count: u32) -> Result<WriteFilemarksOutcome, TapeIoError> {
        Err(TapeIoError::OperationFailed(
            "single-block test sink does not write filemarks".to_string(),
        ))
    }

    fn position(&mut self) -> Result<TapePosition, TapeIoError> {
        Ok(TapePosition {
            lba: self.writes,
            partition: 0,
            beginning_of_partition: self.writes == 0,
            end_of_partition: false,
            block_position_end_of_warning: self.early_warning,
        })
    }
}

#[test]
/// Bootstrap completion must reject incomplete data before attempting its filemark.
fn bootstrap_rejects_incomplete_block_before_filemark() {
    let payload = build_tape_bootstrap(
        [0x42; 16],
        4096,
        ParityConfig::None,
        "1970-01-01T00:00:00Z",
        "remanence-test",
    );
    for (bytes_written, end_of_medium) in [(0, false), (4095, false), (4097, false), (4096, true)] {
        let mut sink = SingleBlockOutcomeSink {
            bytes_written,
            early_warning: end_of_medium,
            end_of_medium,
            writes: 0,
        };
        let error = write_tape_bootstrap(&mut sink, &payload)
            .expect_err("incomplete bootstrap must fail before the filemark");
        assert!(
            matches!(
                error,
                PoolWriteError::TapeIo(TapeIoError::PartialBatchUncommittable {
                    requested_records: 1,
                    requested_bytes: 4096,
                    written_bytes,
                    end_of_medium: reported_eom,
                    ..
                }) if written_bytes == u64::from(bytes_written) && reported_eom == end_of_medium
            ),
            "{error:?}"
        );
        assert_eq!(sink.writes, 1);
    }
}

#[test]
fn fixed_block_helper_rejects_short_bytes_and_hard_eom() {
    for (bytes_written, end_of_medium) in [(3, false), (4, true)] {
        let mut sink = SingleBlockOutcomeSink {
            bytes_written,
            early_warning: false,
            end_of_medium,
            writes: 0,
        };

        let error = write_fixed_blocks(&mut sink, 4, &[0xA5; 4])
            .expect_err("incomplete fixed block must fail");
        let message = error.to_string();
        assert!(
            message.contains("partial fixed batch uncommittable"),
            "{message}"
        );
        assert!(message.contains("requested_bytes=4"), "{message}");
        assert!(
            message.contains(&format!("written_bytes={bytes_written}")),
            "{message}"
        );
        assert!(
            message.contains(&format!("end_of_medium={end_of_medium}")),
            "{message}"
        );
    }
}

#[test]
fn fixed_block_helper_accepts_full_bytes_with_early_warning() {
    let mut sink = SingleBlockOutcomeSink {
        bytes_written: 4,
        early_warning: true,
        end_of_medium: false,
        writes: 0,
    };

    let blocks = write_fixed_blocks(&mut sink, 4, &[0x5A; 8])
        .expect("full fixed blocks remain successful at early warning");

    assert_eq!(blocks, 2);
    assert_eq!(sink.writes, 2);
}

#[tokio::test]
async fn overlap_first_block_gate_requires_high_prefill_then_position_proof() {
    let capacity = 2 * crate::append_ring::APPEND_RING_SLAB_BYTES as u64;
    let manager = crate::io_memory::IoMemoryReservation::new(capacity).expect("manager");
    let (mut producer, _consumer, control) =
        crate::append_ring::create_append_ring(&manager, capacity, 50, 25, capacity).expect("ring");
    producer
        .push(&vec![0x11; crate::append_ring::APPEND_RING_SLAB_BYTES / 2])
        .await
        .expect("sub-high prefill");
    let mut sink = StagedTestSink::new(2);
    {
        let mut gated = OverlapBlockSink {
            inner: &mut sink,
            control: Arc::clone(&control),
            expected_initial_lba: 0,
            expected_next_lba: 0,
            initial_position_proved: false,
            write_started: false,
            low_water_events: 0,
        };
        let error = gated
            .write_block(&[0u8; 4])
            .expect_err("sub-high ring must not reach tape");
        assert!(error.to_string().contains("first-block gate"), "{error}");
        let error = gated
            .space_to_end_of_data()
            .expect_err("sub-high ring must not position an append");
        assert!(error.to_string().contains("first-block gate"), "{error}");
    }
    assert!(
        sink.events.is_empty(),
        "no tape command may precede the high-water gate: {:?}",
        sink.events
    );

    producer
        .push(&vec![0x22; crate::append_ring::APPEND_RING_SLAB_BYTES / 2])
        .await
        .expect("reach high prefill");
    {
        let mut gated = OverlapBlockSink {
            inner: &mut sink,
            control,
            expected_initial_lba: 0,
            expected_next_lba: 0,
            initial_position_proved: false,
            write_started: false,
            low_water_events: 0,
        };
        gated.write_block(&[0u8; 4]).expect("gated first block");
    }
    assert_eq!(sink.events, ["position", "write_block:4"]);
}

#[test]
fn no_parity_append_lba_accepts_tape_files_beyond_u32() {
    let append = NoParityAppendContext {
        tape_file_number: u64::from(u32::MAX) + 1,
        previous_total_committed_ordinals: 11,
        fresh_tape: false,
        expected_append_lba: None,
    };

    assert_eq!(append.expected_append_lba().unwrap(), 4_294_967_308);
}

#[tokio::test]
async fn overlap_append_gate_counts_bootstrap_and_committed_trailing_filemark() {
    let capacity = 2 * crate::append_ring::APPEND_RING_SLAB_BYTES as u64;
    let manager = crate::io_memory::IoMemoryReservation::new(capacity).expect("manager");
    let (mut producer, _consumer, control) =
        crate::append_ring::create_append_ring(&manager, capacity, 50, 25, capacity).expect("ring");
    producer
        .push(&vec![0x21; crate::append_ring::APPEND_RING_SLAB_BYTES])
        .await
        .expect("reach high prefill");

    let object_blocks = 2u64;
    let append = NoParityAppendContext {
        tape_file_number: 2,
        previous_total_committed_ordinals: object_blocks,
        fresh_tape: false,
        expected_append_lba: None,
    };
    let expected = append.expected_append_lba().expect("expected append LBA");

    let mut sink = StagedTestSink::new(2);
    sink.write_block(&[0xb0; 4]).expect("bootstrap block");
    sink.write_filemarks(1).expect("bootstrap filemark");
    for _ in 0..object_blocks {
        sink.write_block(&[0x0b; 4])
            .expect("committed object block");
    }
    sink.write_filemarks(1)
        .expect("committed object trailing filemark");
    sink.inner.set_next_lba_for_test(0);
    sink.events.clear();

    {
        let mut gated = OverlapBlockSink {
            inner: &mut sink,
            control: Arc::clone(&control),
            expected_initial_lba: expected,
            expected_next_lba: expected,
            initial_position_proved: false,
            write_started: false,
            low_water_events: 0,
        };
        let observed = position_no_parity_append(&mut gated).expect("position and prove EOD");
        assert_eq!(expected, observed.lba);
        assert_eq!(observed.lba, 5, "block + filemark records define EOD");
    }
    assert_eq!(sink.events, ["space_eod"]);

    sink.inner.set_next_lba_for_test(0);
    sink.events.clear();
    let old_fencepost = expected.checked_sub(1).expect("nonzero append LBA");
    let mut gated = OverlapBlockSink {
        inner: &mut sink,
        control,
        expected_initial_lba: old_fencepost,
        expected_next_lba: old_fencepost,
        initial_position_proved: false,
        write_started: false,
        low_water_events: 0,
    };
    let error = position_no_parity_append(&mut gated)
        .expect_err("one-LBA fencepost must still fail closed");
    assert!(
        error.to_string().contains(&format!(
            "expected partition 0 lba {old_fencepost}, observed partition 0 lba {expected}"
        )),
        "{error}"
    );
    assert_eq!(sink.events, ["space_eod"]);
}

#[test]
fn batched_recovery_locates_to_journal_eod_and_overwrites_longer_physical_tail() {
    let mut sink = StagedTestSink::new(2);
    for seed in 0..7u8 {
        sink.write_block(&[seed; 4]).expect("seed physical tail");
    }
    sink.events.clear();

    position_no_parity_append_at_checkpoint(&mut sink, 3)
        .expect("journal EOD is authoritative despite longer physical tail");

    assert_eq!(sink.events, ["locate:3", "position"]);
    assert_eq!(sink.position().expect("position after locate").lba, 3);
    sink.write_block(&[0x99; 4])
        .expect("overwrite orphaned tail");
    assert_eq!(sink.position().expect("position after overwrite").lba, 4);
}

#[tokio::test]
async fn overlap_batched_recovery_uses_locate_instead_of_space_eod() {
    let capacity = 2 * crate::append_ring::APPEND_RING_SLAB_BYTES as u64;
    let manager = crate::io_memory::IoMemoryReservation::new(capacity).expect("manager");
    let (mut producer, _consumer, control) =
        crate::append_ring::create_append_ring(&manager, capacity, 50, 25, capacity).expect("ring");
    producer
        .push(&vec![0x21; crate::append_ring::APPEND_RING_SLAB_BYTES])
        .await
        .expect("reach high prefill");
    let mut sink = StagedTestSink::new(2);
    for seed in 0..7u8 {
        sink.write_block(&[seed; 4]).expect("seed physical tail");
    }
    sink.events.clear();

    let mut gated = OverlapBlockSink {
        inner: &mut sink,
        control,
        expected_initial_lba: 3,
        expected_next_lba: 3,
        initial_position_proved: false,
        write_started: false,
        low_water_events: 0,
    };
    position_no_parity_append_at_checkpoint(&mut gated, 3)
        .expect("overlap recovery locates to journal EOD");

    assert_eq!(sink.events, ["locate:3", "position"]);
}

#[test]
fn overlap_low_water_pause_refills_then_reproves_next_lba() {
    let capacity = 4 * crate::append_ring::APPEND_RING_SLAB_BYTES as u64;
    let manager = crate::io_memory::IoMemoryReservation::new(capacity).expect("manager");
    let (mut producer, mut consumer, control) =
        crate::append_ring::create_append_ring(&manager, capacity, 50, 25, 8 * capacity)
            .expect("ring");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime
        .block_on(producer.push(&vec![
            0x31;
            2 * crate::append_ring::APPEND_RING_SLAB_BYTES + 1
        ]))
        .expect("initial high-water fill");

    let mut sink = StagedTestSink::new(2);
    let mut gated = OverlapBlockSink {
        inner: &mut sink,
        control: Arc::clone(&control),
        expected_initial_lba: 0,
        expected_next_lba: 0,
        initial_position_proved: false,
        write_started: false,
        low_water_events: 0,
    };
    gated.write_block(&[0x41; 4]).expect("first block");

    let mut drained = vec![0u8; crate::append_ring::APPEND_RING_SLAB_BYTES + 1];
    consumer
        .read_exact(&mut drained)
        .expect("drain to low watermark");
    assert!(control.should_pause(), "ring must be at the low watermark");

    let refill = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("refill runtime");
        runtime.block_on(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            producer
                .push(&vec![0x52; 2 * crate::append_ring::APPEND_RING_SLAB_BYTES])
                .await
                .expect("refill to high watermark");
            producer
        })
    });
    gated
        .write_block(&[0x42; 4])
        .expect("resume after fresh proof");
    let producer = refill.join().expect("refill thread");
    drop(producer);
    drop(gated);

    assert_eq!(
        sink.events,
        [
            "position",
            "write_block:4",
            "position",
            "position",
            "write_block:4",
        ],
        "resume must flush/prove, wait, then issue a fresh proof before WRITE"
    );
}

#[test]
fn overlap_resume_refuses_position_drift_before_the_next_write() {
    let capacity = 4 * crate::append_ring::APPEND_RING_SLAB_BYTES as u64;
    let manager = crate::io_memory::IoMemoryReservation::new(capacity).expect("manager");
    let (mut producer, mut consumer, control) =
        crate::append_ring::create_append_ring(&manager, capacity, 50, 25, 8 * capacity)
            .expect("ring");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime
        .block_on(producer.push(&vec![
            0x61;
            2 * crate::append_ring::APPEND_RING_SLAB_BYTES + 1
        ]))
        .expect("initial high-water fill");

    let position = |lba| TapePosition {
        lba,
        partition: 0,
        beginning_of_partition: lba == 0,
        end_of_partition: false,
        block_position_end_of_warning: false,
    };
    let mut sink = StagedTestSink::new(2);
    sink.position_overrides = [position(0), position(1), position(9)].into();
    let (position_tx, position_rx) = std_mpsc::channel();
    sink.position_notify = Some(position_tx);
    let mut gated = OverlapBlockSink {
        inner: &mut sink,
        control: Arc::clone(&control),
        expected_initial_lba: 0,
        expected_next_lba: 0,
        initial_position_proved: false,
        write_started: false,
        low_water_events: 0,
    };
    gated.write_block(&[0x71; 4]).expect("first block");
    let mut drained = vec![0u8; crate::append_ring::APPEND_RING_SLAB_BYTES + 1];
    consumer.read_exact(&mut drained).expect("drain to low");

    let refill = std::thread::spawn(move || {
        // Refill only after the writer has read the low-water pause boundary
        // (its second READ POSITION). Refilling earlier lets the writer see a
        // full ring, skip the pause and write normally, and the resume proof
        // under test never runs.
        while position_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("writer reached the low-water pause boundary")
            < 2
        {}
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("refill runtime");
        runtime.block_on(async move {
            producer
                .push(&vec![0x72; 2 * crate::append_ring::APPEND_RING_SLAB_BYTES])
                .await
                .expect("refill to high");
        });
    });
    let error = gated
        .write_block(&[0x73; 4])
        .expect_err("drifted resume must fail closed");
    refill.join().expect("refill thread");
    drop(gated);
    assert!(error.to_string().contains("position drift"), "{error}");
    assert_eq!(
        sink.events,
        ["position", "write_block:4", "position", "position"],
        "no WRITE may follow a failed resume proof"
    );
}

#[test]
fn overlap_admission_plans_object_larger_than_legacy_64_gib_cap() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-large-overlap-admission-")
        .tempdir()
        .expect("tempdir");
    let mut state = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
    let tape_uuid = [0x6b; 16];
    let block_size = 256 * 1024;
    state
        .provision_tape(remanence_state::ProvisionTapeInput {
            tape_uuid,
            voltag: "LARGE1L9".into(),
            block_size,
            parity: ParityConfig::None,
            force: false,
        })
        .expect("provision mocked LTO-9");
    let selected = SelectedTape {
        standalone_endpoint: None,
        pool_id: "large.overlap".into(),
        tape_uuid,
        block_size,
        parity_config: ParityConfig::None,
    };
    let ring_bytes = crate::append_ring::APPEND_RING_SLAB_BYTES as u64;
    let manager = crate::io_memory::IoMemoryReservation::new(ring_bytes).expect("manager");
    let (_producer, consumer, control) = crate::append_ring::create_append_ring(
        &manager,
        ring_bytes,
        90,
        25,
        crate::APPEND_SPOOL_MAX_BYTES + 1,
    )
    .expect("ring");
    let digest = [0x42; 32];
    let mut request = WriteObjectToPoolRequest {
        pool_id: selected.pool_id.clone(),
        source: WriteObjectSource::Streamed(StreamedWriteSource::new(
            consumer,
            crate::APPEND_SPOOL_MAX_BYTES + 1,
            digest,
            control,
        )),
        archive_path: "large.bin".into(),
        caller_object_id: "overlap-larger-than-spool-cap".into(),
        expected_content_sha256: Some(digest),
        expected_object_id: None,
        input_kind: crate::WriteObjectInputKind::LogicalFile,
        representation: PoolWriteRepresentation::Plaintext,
    };

    let WriteObjectSource::Streamed(source) = &mut request.source else {
        panic!("streamed source")
    };
    source.mtime = Some("1234567890.123456789".into());
    let prepared = prepare_pool_object(&request, selected.block_size)
        .expect("large streamed source must be plannable without payload");
    assert_eq!(
        prepared.files[0].spec.mtime.as_deref(),
        Some("1234567890.123456789")
    );
    assert_eq!(
        prepared_payload_bytes(&prepared),
        crate::APPEND_SPOOL_MAX_BYTES + 1
    );
    let stored =
        prepare_stored_object(&prepared, &request.representation).expect("plaintext stored plan");
    let footprint =
        stored_footprint_bytes(&stored, &prepared, selected.block_size).expect("large footprint");
    ensure_selected_tape_has_capacity(
        &state,
        &test_capacity_pool_config(&selected),
        &selected,
        footprint,
        None,
    )
    .expect("mocked LTO-9 admits object beyond the legacy spool cap");
}

#[test]
fn batched_capacity_uses_session_local_physical_extent() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-batched-capacity-")
        .tempdir()
        .expect("tempdir");
    let mut state = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
    let tape_uuid = [0x6c; 16];
    let block_size = 256 * 1024;
    state
        .provision_tape(remanence_state::ProvisionTapeInput {
            tape_uuid,
            voltag: "BATCH1L9".into(),
            block_size,
            parity: ParityConfig::None,
            force: false,
        })
        .expect("provision mocked LTO-9");
    let selected = SelectedTape {
        standalone_endpoint: None,
        pool_id: "batched.capacity".into(),
        tape_uuid,
        block_size,
        parity_config: ParityConfig::None,
    };
    let capacity = raw_capacity_bytes(LtoGen::Lto9);
    let provisional_used_lba = capacity / u64::from(block_size) - 1;
    let object_size = u64::from(block_size) * 2;

    ensure_selected_tape_has_capacity(
        &state,
        &test_capacity_pool_config(&selected),
        &selected,
        object_size,
        None,
    )
    .expect("empty SQLite projection alone would admit the object");
    let err = ensure_selected_tape_has_capacity(
        &state,
        &test_capacity_pool_config(&selected),
        &selected,
        object_size,
        Some(provisional_used_lba),
    )
    .expect_err("the provisional physical prefix consumes capacity");
    assert!(
        matches!(err, PoolWriteError::SelectedTapeInsufficientCapacity { .. }),
        "{err}"
    );
}

#[test]
fn no_parity_exact_close_capacity_rolls_before_whole_object_motion() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-no-parity-terminal-capacity-")
        .tempdir()
        .expect("tempdir");
    let mut state = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
    let tape_uuid = [0x73; 16];
    let block_size = 256 * 1024;
    state
        .provision_tape(remanence_state::ProvisionTapeInput {
            tape_uuid,
            voltag: "NCAP01L9".into(),
            block_size,
            parity: ParityConfig::None,
            force: false,
        })
        .expect("provision mocked LTO-9");
    let selected = SelectedTape {
        standalone_endpoint: None,
        pool_id: "capacity.no-parity".into(),
        tape_uuid,
        block_size,
        parity_config: ParityConfig::None,
    };
    let pool_cfg = TapePoolConfig {
        scheme: None,
        id: selected.pool_id.clone(),
        display_name: None,
        copy_class: None,
        content_class: None,
        selection_policy: Default::default(),
        watermark_low: 0.97,
        watermark_high: 0.98,
        capacity_cap_bytes: None,
        block_size_bytes: u64::from(block_size),
        min_object_size_bytes: 0,
    };
    let capacity_blocks = raw_capacity_bytes(LtoGen::Lto9) / u64::from(block_size);
    let high_watermark_blocks =
        watermark_floor_bytes(capacity_blocks, pool_cfg.watermark_high).expect("high watermark");
    let context = BatchedNoParityAppendContext {
        append: NoParityAppendContext {
            tape_file_number: 3,
            previous_total_committed_ordinals: 1,
            fresh_tape: false,
            expected_append_lba: Some(high_watermark_blocks),
        },
        position: BatchedAppendPosition::JournalEod(high_watermark_blocks),
        object_row_count: 1,
    };

    let error = ensure_no_parity_terminal_close_capacity(&state, &pool_cfg, &selected, &context, 1)
        .expect_err("the current prefix must close before any block of the Object moves");
    assert!(matches!(
        error,
        PoolWriteError::TerminalCloseRequired { .. }
    ));

    let fresh = BatchedNoParityAppendContext {
        append: NoParityAppendContext {
            tape_file_number: 1,
            previous_total_committed_ordinals: 0,
            fresh_tape: true,
            expected_append_lba: None,
        },
        position: BatchedAppendPosition::FreshTape,
        object_row_count: 0,
    };
    let error = ensure_no_parity_terminal_close_capacity(
        &state,
        &pool_cfg,
        &selected,
        &fresh,
        capacity_blocks,
    )
    .expect_err("an impossible whole Object must be rejected on fresh media");
    assert!(matches!(
        error,
        PoolWriteError::Parity(ParityError::ObjectTooLargeForEmptyTape { .. })
    ));
}

#[test]
fn downward_pool_cap_is_the_shared_selection_and_terminal_capacity_basis() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-downward-capacity-basis-")
        .tempdir()
        .expect("tempdir");
    let mut state = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
    let tape_uuid = [0x74; 16];
    let block_size = 256 * 1024;
    state
        .provision_tape(remanence_state::ProvisionTapeInput {
            tape_uuid,
            voltag: "DCAP01L9".into(),
            block_size,
            parity: ParityConfig::None,
            force: false,
        })
        .expect("provision mocked LTO-9");
    let selected = SelectedTape {
        standalone_endpoint: None,
        pool_id: "capacity.downward".into(),
        tape_uuid,
        block_size,
        parity_config: ParityConfig::None,
    };
    let cap_bytes = 4 * 1024 * 1024 * 1024_u64;
    let pool_cfg = TapePoolConfig {
        capacity_cap_bytes: Some(cap_bytes),
        ..test_capacity_pool_config(&selected)
    };
    let expected_c = cap_bytes / u64::from(block_size);

    assert_eq!(
        parity_capacity_basis_blocks(&state, &pool_cfg, &selected).expect("capped terminal C"),
        expected_c
    );
    let tape = state
        .get_tape(&tape_uuid)
        .expect("query tape")
        .expect("tape");
    let fit = tape_fit_state_from_record(&tape, &pool_cfg, &selected.pool_id, 0)
        .expect("capped selection projection");
    assert_eq!(
        fit.usable_bytes,
        watermark_floor_bytes(cap_bytes, pool_cfg.watermark_high).expect("capped H")
    );
    ensure_selected_tape_has_capacity(&state, &pool_cfg, &selected, cap_bytes + 1, None)
        .expect_err("general admission must use the same capped C");

    let upward = TapePoolConfig {
        capacity_cap_bytes: Some(raw_capacity_bytes(LtoGen::Lto9)),
        ..pool_cfg
    };
    assert!(parity_capacity_basis_blocks(&state, &upward, &selected).is_err());
}

#[test]
fn close_only_authority_uses_capped_c_at_equality_and_below_low() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-close-only-capacity-authority-")
        .tempdir()
        .expect("tempdir");
    let mut state = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
    let tape_uuid = [0x5c; 16];
    let block_size = 256 * 1024;
    state
        .provision_tape(remanence_state::ProvisionTapeInput {
            tape_uuid,
            voltag: "CAP005L9".into(),
            block_size,
            parity: ParityConfig::None,
            force: false,
        })
        .expect("provision mocked LTO-9");
    let selected = SelectedTape {
        standalone_endpoint: None,
        pool_id: "capacity.close-only".into(),
        tape_uuid,
        block_size,
        parity_config: ParityConfig::None,
    };
    let capacity_blocks = 1_000_000;
    let pool_cfg = TapePoolConfig {
        watermark_low: 0.1,
        watermark_high: 0.2,
        capacity_cap_bytes: Some(capacity_blocks * u64::from(block_size)),
        ..test_capacity_pool_config(&selected)
    };
    let counts = remanence_parity::TapeIndexReplicaCounts {
        structural_entry_count: 1,
        object_row_count: 0,
    };
    let replica = remanence_parity::checked_tape_index_replica_layout(block_size, counts)
        .expect("replica layout");
    let gap_records = remanence_parity::index_separation_records(
        block_size,
        remanence_parity::DEFAULT_INDEX_SEPARATION_BYTES,
    )
    .expect("gap layout");
    let tail_charge = 3 * (replica.replica_record_count + 1) + 2 * (gap_records + 1);

    let equality_start = capacity_blocks - tail_charge - TERMINAL_CAPACITY_SAFETY_MARGIN_BLOCKS;
    let equality_layout = remanence_parity::TerminalTailLayout::new(
        0,
        block_size,
        1,
        equality_start,
        replica.replica_record_count,
        gap_records,
    )
    .expect("equality tail layout");
    let equality = authorize_terminal_close_only_plan(
        &state,
        Some(&pool_cfg),
        &selected,
        equality_start,
        counts,
        equality_layout.expected_eod_lba,
    )
    .expect("C equality succeeds");
    assert_eq!(
        equality.required_tape_blocks,
        capacity_blocks - equality_start
    );

    let one_short_cfg = TapePoolConfig {
        capacity_cap_bytes: Some((capacity_blocks - 1) * u64::from(block_size)),
        ..pool_cfg.clone()
    };
    let one_short = authorize_terminal_close_only_plan(
        &state,
        Some(&one_short_cfg),
        &selected,
        equality_start,
        counts,
        equality_layout.expected_eod_lba,
    )
    .expect_err("one block below the exact capped C must fail");
    assert!(
        matches!(
            one_short,
            PoolWriteError::Parity(ParityError::CapacityReserveExceeded {
                cause: CapacityReserveCause::TapeCapacity,
                ..
            })
        ),
        "{one_short}"
    );

    let below_low_start = 100;
    assert!(
        below_low_start
            < watermark_floor_bytes(capacity_blocks, pool_cfg.watermark_low)
                .expect("low watermark")
    );
    let below_low_layout = remanence_parity::TerminalTailLayout::new(
        0,
        block_size,
        1,
        below_low_start,
        replica.replica_record_count,
        gap_records,
    )
    .expect("below-low tail layout");
    authorize_terminal_close_only_plan(
        &state,
        Some(&pool_cfg),
        &selected,
        below_low_start,
        counts,
        below_low_layout.expected_eod_lba,
    )
    .expect("manual close below L uses the same exact authority");

    let unpooled_capacity_blocks = raw_capacity_bytes(LtoGen::Lto9) / u64::from(block_size);
    assert_eq!(
        terminal_watermark_blocks(unpooled_capacity_blocks, None)
            .expect("unpooled terminal watermarks"),
        (0, 1)
    );
    let unpooled_equality_start =
        unpooled_capacity_blocks - tail_charge - TERMINAL_CAPACITY_SAFETY_MARGIN_BLOCKS;
    let unpooled_equality_layout = remanence_parity::TerminalTailLayout::new(
        0,
        block_size,
        1,
        unpooled_equality_start,
        replica.replica_record_count,
        gap_records,
    )
    .expect("unpooled equality tail layout");
    let unpooled = authorize_terminal_close_only_plan(
        &state,
        None,
        &selected,
        unpooled_equality_start,
        counts,
        unpooled_equality_layout.expected_eod_lba,
    )
    .expect("unpooled close uses raw C with the canonical L/H basis");
    assert_eq!(
        unpooled.required_tape_blocks,
        unpooled_capacity_blocks - unpooled_equality_start
    );
}

#[test]
fn parity_capacity_reservation_uses_physical_cursor_exact_layout_and_atomic_spool_grant() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-parity-capacity-runtime-")
        .tempdir()
        .expect("tempdir");
    let mut state = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
    let tape_uuid = [0x6d; 16];
    let block_size = 256 * 1024;
    let scheme = ParityScheme {
        id: SchemeId::new_static("capacity-runtime-test"),
        data_blocks_per_stripe: 2,
        parity_blocks_per_stripe: 1,
        stripes_per_neighborhood: 1,
    };
    state
        .provision_tape(remanence_state::ProvisionTapeInput {
            tape_uuid,
            voltag: "CAP001L9".into(),
            block_size,
            parity: ParityConfig::Scheme(scheme.clone()),
            force: false,
        })
        .expect("provision mocked LTO-9");
    let selected = SelectedTape {
        standalone_endpoint: None,
        pool_id: "capacity.runtime".into(),
        tape_uuid,
        block_size,
        parity_config: ParityConfig::Scheme(scheme.clone()),
    };
    let pool_cfg = TapePoolConfig {
        scheme: None,
        id: selected.pool_id.clone(),
        display_name: None,
        copy_class: None,
        content_class: None,
        selection_policy: Default::default(),
        watermark_low: 0.97,
        watermark_high: 0.98,
        capacity_cap_bytes: None,
        block_size_bytes: u64::from(block_size),
        min_object_size_bytes: 0,
    };
    let capacity_blocks =
        parity_capacity_basis_blocks(&state, &pool_cfg, &selected).expect("capacity basis");
    let mut backing = VecBlockSink::new();
    let mut raw = BlockSinkRawTapeSink::new(&mut backing);
    let mut journal = PerObjectTestJournal {
        tape_uuid,
        bundles: Vec::new(),
    };
    let mut parity = ParitySink::new_with_journal(
        &mut raw,
        &mut journal,
        scheme,
        tape_uuid,
        block_size,
        remanence_parity::WriterIdentity::fixed(
            "remanence-test".into(),
            std::time::SystemTime::UNIX_EPOCH.into(),
        ),
    )
    .expect("parity sink");
    parity.write_bootstrap().expect("initial bootstrap");
    assert_eq!(
        parity
            .terminal_triple_capacity_runtime_state()
            .expect("runtime state")
            .used_tape_blocks,
        2,
        "physical cursor includes the bootstrap body and trailing filemark"
    );

    let io_memory =
        crate::io_memory::IoMemoryReservation::new(1024 * 1024).expect("spool reservation manager");
    let mismatch = reserve_parity_object_capacity(
        parity
            .terminal_triple_capacity_runtime_state()
            .expect("runtime state"),
        parity.scheme(),
        &selected,
        (&pool_cfg, 0, 0),
        capacity_blocks,
        2,
        &io_memory,
    )
    .expect_err("journal authority cannot omit the live BOT structural row");
    assert!(
        mismatch
            .to_string()
            .contains("journal/sink authority mismatch"),
        "{mismatch}"
    );
    assert_eq!(io_memory.granted(), 0);

    let reservation = reserve_parity_object_capacity(
        parity
            .terminal_triple_capacity_runtime_state()
            .expect("runtime state"),
        parity.scheme(),
        &selected,
        (&pool_cfg, 1, 0),
        capacity_blocks,
        2,
        &io_memory,
    )
    .expect("exact reservation");
    let report = *reservation.report();
    assert!(report.projected_object_present);
    assert_eq!(report.object_tape_file_blocks, 3);
    assert_eq!(
        report.safety_margin_blocks,
        TERMINAL_CAPACITY_SAFETY_MARGIN_BLOCKS
    );
    assert!(report.final_parity_map_tape_file_blocks > 0);
    assert_eq!(io_memory.granted(), report.required_spool_bytes);

    let (reservation, spool_permit) = reservation.into_parts();
    parity
        .begin_object_with_terminal_triple_reservation(reservation)
        .expect("begin after atomic reservation");
    parity
        .write_block(&vec![0xA5; block_size as usize])
        .expect("first Object block");
    parity
        .write_block(&vec![0x5A; block_size as usize])
        .expect("second Object block");
    assert_eq!(
        io_memory.granted(),
        report.required_spool_bytes,
        "spool grant remains held while completed parity is staged"
    );
    parity.finish_object().expect("emit reserved sidecar");
    assert_eq!(
        io_memory.granted(),
        report.required_spool_bytes,
        "spool grant remains held through sidecar emission"
    );

    drop(spool_permit);
    assert_eq!(io_memory.granted(), 0);
}

#[test]
fn exact_terminal_admission_rejects_high_watermark_crossing_before_spool_grant() {
    let block_size = 1024 * 1024;
    let scheme = ParityScheme {
        id: SchemeId::new_static("terminal-admission-test"),
        data_blocks_per_stripe: 8,
        parity_blocks_per_stripe: 2,
        stripes_per_neighborhood: 1,
    };
    let selected = SelectedTape {
        standalone_endpoint: None,
        pool_id: "terminal.admission".to_string(),
        tape_uuid: [0x6c; 16],
        block_size,
        parity_config: ParityConfig::Scheme(scheme.clone()),
    };
    let pool_cfg = TapePoolConfig {
        scheme: None,
        id: selected.pool_id.clone(),
        display_name: None,
        copy_class: None,
        content_class: None,
        selection_policy: Default::default(),
        watermark_low: 0.1,
        watermark_high: 0.2,
        capacity_cap_bytes: None,
        block_size_bytes: u64::from(block_size),
        min_object_size_bytes: 0,
    };
    let io_memory = crate::io_memory::IoMemoryReservation::new(1).expect("one-byte spool manager");
    let error = reserve_parity_object_capacity(
        TerminalTripleCapacityRuntimeState {
            current_epoch_fill_blocks: 0,
            pending_completed_sidecars: 0,
            pending_completed_epoch_parity_bytes: 0,
            sidecar_entries_before_object: 0,
            structural_entries_before_object: 1,
            object_rows_before_object: 0,
            used_tape_blocks: 1_900,
        },
        &scheme,
        &selected,
        (&pool_cfg, 1, 0),
        10_000,
        200,
        &io_memory,
    )
    .expect_err("projected prefix crosses H and must roll before Object motion");
    assert!(
        error
            .to_string()
            .contains("requires finalizing the current prefix"),
        "{error}"
    );
    assert_eq!(
        io_memory.granted(),
        0,
        "rejected admission must precede the atomic parity spool grant"
    );
}

#[test]
fn parity_spool_shortfall_is_rejected_before_object_tape_motion() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-parity-capacity-shortfall-")
        .tempdir()
        .expect("tempdir");
    let mut state = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
    let tape_uuid = [0x6e; 16];
    let block_size = 256 * 1024;
    let scheme = ParityScheme {
        id: SchemeId::new_static("capacity-shortfall-test"),
        data_blocks_per_stripe: 2,
        parity_blocks_per_stripe: 1,
        stripes_per_neighborhood: 1,
    };
    state
        .provision_tape(remanence_state::ProvisionTapeInput {
            tape_uuid,
            voltag: "CAP002L9".into(),
            block_size,
            parity: ParityConfig::Scheme(scheme.clone()),
            force: false,
        })
        .expect("provision mocked LTO-9");
    let selected = SelectedTape {
        standalone_endpoint: None,
        pool_id: "capacity.shortfall".into(),
        tape_uuid,
        block_size,
        parity_config: ParityConfig::Scheme(scheme.clone()),
    };
    let pool_cfg = TapePoolConfig {
        scheme: None,
        id: selected.pool_id.clone(),
        display_name: None,
        copy_class: None,
        content_class: None,
        selection_policy: Default::default(),
        watermark_low: 0.97,
        watermark_high: 0.98,
        capacity_cap_bytes: None,
        block_size_bytes: u64::from(block_size),
        min_object_size_bytes: 0,
    };
    let capacity_blocks =
        parity_capacity_basis_blocks(&state, &pool_cfg, &selected).expect("capacity basis");
    let mut backing = VecBlockSink::new();
    let mut raw = BlockSinkRawTapeSink::new(&mut backing);
    let mut journal = PerObjectTestJournal {
        tape_uuid,
        bundles: Vec::new(),
    };
    let mut parity = ParitySink::new_with_journal(
        &mut raw,
        &mut journal,
        scheme,
        tape_uuid,
        block_size,
        remanence_parity::WriterIdentity::fixed(
            "remanence-test".into(),
            std::time::SystemTime::UNIX_EPOCH.into(),
        ),
    )
    .expect("parity sink");
    parity.write_bootstrap().expect("initial bootstrap");
    let position_before = parity
        .terminal_triple_capacity_runtime_state()
        .expect("runtime state")
        .used_tape_blocks;
    let io_memory = crate::io_memory::IoMemoryReservation::new(11).expect("spool budget");
    let retained = io_memory
        .try_reserve_epoch(10, "tape 01 drive A".into())
        .unwrap();

    let error = reserve_parity_object_capacity(
        parity
            .terminal_triple_capacity_runtime_state()
            .expect("runtime state"),
        parity.scheme(),
        &selected,
        (&pool_cfg, 1, 0),
        capacity_blocks,
        2,
        &io_memory,
    )
    .expect_err("completed parity epoch exceeds the atomic spool budget");
    assert!(error
        .to_string()
        .contains("retained epochs [tape 01 drive A]"));
    assert!(
        std::error::Error::source(&error).is_none(),
        "the display already includes the capacity cause"
    );
    drop(retained);
    assert!(
        matches!(
            error,
            PoolWriteError::ParitySpoolRefused { ref cause, .. } if matches!(cause.as_ref(), ParityError::CapacityReserveExceeded {
                    cause: CapacityReserveCause::ParitySpoolCapacity,
                    remaining_spool_bytes: Some(1),
                    ..
                })
        ),
        "{error}"
    );
    assert_eq!(
        parity
            .terminal_triple_capacity_runtime_state()
            .expect("runtime state")
            .used_tape_blocks,
        position_before,
        "capacity rejection performs no Object write or filemark"
    );
    assert_eq!(io_memory.granted(), 0);
}

#[test]
fn parity_capacity_distinguishes_fresh_media_limit_from_current_tape_shortfall() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-parity-capacity-boundary-")
        .tempdir()
        .expect("tempdir");
    let mut state = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
    let tape_uuid = [0x6f; 16];
    let block_size = 256 * 1024;
    let scheme = ParityScheme {
        id: SchemeId::new_static("capacity-boundary-test"),
        data_blocks_per_stripe: 2,
        parity_blocks_per_stripe: 1,
        stripes_per_neighborhood: 1,
    };
    state
        .provision_tape(remanence_state::ProvisionTapeInput {
            tape_uuid,
            voltag: "CAP003L9".into(),
            block_size,
            parity: ParityConfig::Scheme(scheme.clone()),
            force: false,
        })
        .expect("provision mocked LTO-9");
    let selected = SelectedTape {
        standalone_endpoint: None,
        pool_id: "capacity.boundary".into(),
        tape_uuid,
        block_size,
        parity_config: ParityConfig::Scheme(scheme.clone()),
    };
    let pool_cfg = TapePoolConfig {
        scheme: None,
        id: selected.pool_id.clone(),
        display_name: None,
        copy_class: None,
        content_class: None,
        selection_policy: Default::default(),
        watermark_low: 0.97,
        watermark_high: 0.98,
        capacity_cap_bytes: None,
        block_size_bytes: u64::from(block_size),
        min_object_size_bytes: 0,
    };
    let mut backing = VecBlockSink::new();
    let mut raw = BlockSinkRawTapeSink::new(&mut backing);
    let mut journal = PerObjectTestJournal {
        tape_uuid,
        bundles: Vec::new(),
    };
    let mut parity = ParitySink::new_with_journal(
        &mut raw,
        &mut journal,
        scheme.clone(),
        tape_uuid,
        block_size,
        remanence_parity::WriterIdentity::fixed(
            "remanence-test".into(),
            std::time::SystemTime::UNIX_EPOCH.into(),
        ),
    )
    .expect("parity sink");
    parity.write_bootstrap().expect("initial bootstrap");
    let fresh_runtime = parity
        .terminal_triple_capacity_runtime_state()
        .expect("fresh runtime state");
    assert_eq!(
        fresh_runtime.used_tape_blocks,
        PARITY_INITIAL_BOOTSTRAP_PREFIX_BLOCKS
    );

    let baseline_memory =
        crate::io_memory::IoMemoryReservation::new(1024 * 1024).expect("baseline spool manager");
    let capacity_blocks = 1_000_000;
    let baseline = reserve_parity_object_capacity(
        fresh_runtime,
        &scheme,
        &selected,
        (&pool_cfg, 1, 0),
        capacity_blocks,
        2,
        &baseline_memory,
    )
    .expect("fresh media admits the object under a valid exact profile");
    let required = baseline.report().required_tape_blocks;
    drop(baseline);

    let oversized_memory =
        crate::io_memory::IoMemoryReservation::new(1024 * 1024).expect("oversized spool manager");
    let error = reserve_parity_object_capacity(
        fresh_runtime,
        &scheme,
        &selected,
        (&pool_cfg, 1, 0),
        capacity_blocks,
        capacity_blocks,
        &oversized_memory,
    )
    .expect_err("the Object is impossible on every fresh replacement tape");
    assert!(
        matches!(
            error,
            PoolWriteError::Parity(ParityError::ObjectTooLargeForEmptyTape { .. })
        ),
        "{error}"
    );

    let current_runtime = TerminalTripleCapacityRuntimeState {
        used_tape_blocks: capacity_blocks - required + 1,
        ..fresh_runtime
    };
    let current_memory = crate::io_memory::IoMemoryReservation::new(1024 * 1024)
        .expect("current-tape spool manager");
    let error = reserve_parity_object_capacity(
        current_runtime,
        &scheme,
        &selected,
        (&pool_cfg, 1, 0),
        capacity_blocks,
        2,
        &current_memory,
    )
    .expect_err("the same Object no longer fits the current nonempty tape");
    assert!(
        matches!(error, PoolWriteError::TerminalCloseRequired { .. }),
        "{error}"
    );
}

#[test]
fn batched_parity_post_motion_projection_failure_sets_dirty_after_retryable_spool_rejection() {
    // Isolate tracing's process-wide callsite cache from concurrent test subscribers.
    const CHILD: &str = "REMANENCE_PARITY_DIAGNOSTIC_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "pool_write::tests::batched_parity_post_motion_projection_failure_sets_dirty_after_retryable_spool_rejection", "--nocapture"])
            .env(CHILD, "1").output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(stdout.contains("1 passed"));
        println!("{stdout}");
        return;
    }
    let diagnostic_file = tempfile::tempfile().unwrap();
    let reader = diagnostic_file.try_clone().unwrap();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .flatten_event(true)
        .with_writer(move || diagnostic_file.try_clone().unwrap())
        .finish();
    let _subscriber = tracing::subscriber::set_default(subscriber);
    let temp = tempfile::Builder::new()
        .prefix("remanence-parity-session-retention-")
        .tempdir()
        .expect("tempdir");
    let mut state = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
    let pool_id = "capacity.session";
    let tape_uuid = [0x70; 16];
    let block_size = 256 * 1024;
    let scheme = ParityScheme {
        id: SchemeId::new_static("capacity-session-test"),
        data_blocks_per_stripe: 2,
        parity_blocks_per_stripe: 1,
        stripes_per_neighborhood: 1,
    };
    state
        .provision_tape(remanence_state::ProvisionTapeInput {
            tape_uuid,
            voltag: "CAP004L9".into(),
            block_size,
            parity: ParityConfig::Scheme(scheme.clone()),
            force: false,
        })
        .expect("provision mocked LTO-9");
    state
        .project_tape_pool_membership(tape_uuid, pool_id)
        .expect("assign mocked tape to its selected pool");
    let pool_cfg = TapePoolConfig {
        scheme: None,
        id: pool_id.into(),
        display_name: None,
        copy_class: None,
        content_class: None,
        selection_policy: Default::default(),
        watermark_low: 0.92,
        watermark_high: 0.97,
        capacity_cap_bytes: None,
        block_size_bytes: u64::from(block_size),
        min_object_size_bytes: 0,
    };
    let selected = SelectedTape {
        standalone_endpoint: None,
        pool_id: pool_id.into(),
        tape_uuid,
        block_size,
        parity_config: ParityConfig::Scheme(scheme.clone()),
    };
    let payload_path = temp.path().join("payload.bin");
    std::fs::write(
        &payload_path,
        b"session retention after no-motion rejection",
    )
    .expect("write payload");
    let request = || WriteObjectToPoolRequest {
        pool_id: pool_id.into(),
        source: WriteObjectSource::Path(payload_path.clone()),
        archive_path: PathBuf::from("payload.bin"),
        caller_object_id: "capacity-session-caller".into(),
        expected_content_sha256: None,
        expected_object_id: None,
        input_kind: crate::WriteObjectInputKind::LogicalFile,
        representation: PoolWriteRepresentation::Plaintext,
    };

    // A position-query delay is outside WRITE commands but inside transfer.
    struct HostDelayRaw<'a> {
        inner: BlockSinkRawTapeSink<'a>,
        delay: &'a std::cell::Cell<bool>,
    }
    impl RawTapeSink for HostDelayRaw<'_> {
        fn write_fixed_block(
            &mut self,
            bytes: &[u8],
        ) -> Result<remanence_parity::RawWriteOutcome, ParityError> {
            self.inner.write_fixed_block(bytes)
        }
        fn write_filemarks(
            &mut self,
            count: u32,
            immed: bool,
        ) -> Result<remanence_parity::RawWriteOutcome, ParityError> {
            self.inner.write_filemarks(count, immed)
        }
        fn position(&mut self) -> Result<remanence_parity::PhysicalPositionHint, ParityError> {
            if self.delay.replace(false) {
                std::thread::sleep(Duration::from_millis(20));
            }
            self.inner.position()
        }
    }
    let mut backing = VecBlockSink::new();
    let host_delay = std::cell::Cell::new(false);
    let mut raw = HostDelayRaw {
        inner: BlockSinkRawTapeSink::new(&mut backing),
        delay: &host_delay,
    };
    let mut journal = PerObjectTestJournal {
        tape_uuid,
        bundles: Vec::new(),
    };
    let mut parity = ParitySink::new_with_journal(
        &mut raw,
        &mut journal,
        scheme,
        tape_uuid,
        block_size,
        remanence_parity::WriterIdentity::fixed(
            "remanence-test".into(),
            std::time::SystemTime::UNIX_EPOCH.into(),
        ),
    )
    .expect("parity sink");
    parity.write_bootstrap().expect("initial bootstrap");
    let position_before = parity
        .terminal_triple_capacity_runtime_state()
        .expect("runtime state")
        .used_tape_blocks;
    let mut session_state = Some(parity.into_session_state().expect("detach session"));

    let short_memory =
        crate::io_memory::IoMemoryReservation::new(1).expect("one-byte shared spool budget");
    let mut raw_write_attempted = false;
    let error = write_batched_parity_to_selected_tape_after_replay_check(
        &state,
        &mut raw,
        &mut journal,
        &mut session_state,
        &pool_cfg,
        request(),
        selected.clone(),
        &short_memory,
        &mut raw_write_attempted,
    )
    .expect_err("atomic spool shortfall must reject before Object motion");
    assert!(
        matches!(
            error,
            PoolWriteError::ParitySpoolRefused { ref cause, .. } if matches!(cause.as_ref(), ParityError::CapacityReserveExceeded {
                    cause: CapacityReserveCause::ParitySpoolCapacity,
                    ..
                })
        ),
        "{error}"
    );
    assert!(!raw_write_attempted);
    assert!(
        session_state.is_some(),
        "no-motion rejection retains the session"
    );
    assert_eq!(
        raw.position().expect("position after rejection").lba,
        position_before
    );

    let ample_memory = crate::io_memory::IoMemoryReservation::new(64 * 1024 * 1024)
        .expect("ample shared spool budget");
    host_delay.set(true);
    let result = write_batched_parity_to_selected_tape_after_replay_check(
        &state,
        &mut raw,
        &mut journal,
        &mut session_state,
        &pool_cfg,
        request(),
        selected.clone(),
        &ample_memory,
        &mut raw_write_attempted,
    )
    .expect("the same retained session accepts a later retry");
    assert!(!result.is_replay());
    {
        use std::io::{Read, Seek};
        let mut reader = reader;
        reader.rewind().unwrap();
        let mut text = String::new();
        reader.read_to_string(&mut text).unwrap();
        let events: Vec<serde_json::Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let object_id = Uuid::from_bytes(result.object.object_id).to_string();
        for phase in ["prepare", "transfer", "tail"] {
            let event = events
                .iter()
                .find(|event| event["phase"] == phase && event["object_id"] == object_id)
                .unwrap_or_else(|| panic!("missing {phase} for {object_id}: {text}"));
            assert_eq!(event["parity"], "scheme");
            assert!(event["elapsed_ms"].as_f64().unwrap() >= 0.0);
            if phase == "transfer" {
                let host = event["transfer_host_ms"].as_f64().unwrap();
                let between = event["between_writes_ms"].as_f64().unwrap();
                assert!(
                    !host_delay.get(),
                    "transfer executes the delayed position query"
                );
                assert!(
                    host >= 20.0,
                    "position-query time belongs to host time: {event}"
                );
                assert!(between <= host);
                assert!(
                    (crate::diagnostics::duration_ms(result.transfer_feed_wait) - between).abs()
                        < 0.001
                );
                assert!(event["block_write_calls"].as_u64().unwrap() > 0);
                println!("{event}");
            }
        }
    }
    assert!(raw_write_attempted);
    assert!(session_state.is_some());

    raw_write_attempted = false;
    FAIL_PARITY_POST_WRITE_PROJECTION.with(|flag| flag.set(true));
    let error = write_batched_parity_to_selected_tape_after_replay_check(
        &state,
        &mut raw,
        &mut journal,
        &mut session_state,
        &pool_cfg,
        request(),
        selected,
        &ample_memory,
        &mut raw_write_attempted,
    )
    .expect_err("post-motion projection failure is injected");
    assert!(
        error
            .to_string()
            .contains("injected post-write parity projection failure"),
        "{error}"
    );
    assert!(
        raw_write_attempted,
        "the caller must see raw motion before projection failure"
    );
    assert!(
        session_state.is_none(),
        "post-motion failure consumes the uncertain session for fencing"
    );
}

fn capacity_tape_record() -> TapeRecord {
    TapeRecord {
        seal_due: None,
        highest_protected_ordinal: 0,
        last_verified_at_utc: None,
        last_verification_state: None,
        last_verified_edition_digest: None,
        tape_uuid: vec![0x71; 16],
        voltag: Some("CAP005L9".into()),
        voltag_source: Some(remanence_state::LabelProvenance::Changer),
        cartridge_generation: Some("L9".into()),
        generation_source: Some("label".into()),
        kind: "data".into(),
        pool_id: Some("capacity.projection".into()),
        assignment_generation: 0,
        body_format: None,
        block_size: Some(4096),
        scheme_id: None,
        data_blocks_per_stripe: None,
        parity_blocks_per_stripe: None,
        stripes_per_neighborhood: None,
        last_committed_tape_file: None,
        total_committed_ordinals: 0,
        written_extent_lba: None,
        terminal_finalization: None,
        state: "ready".into(),
        updated_at_utc: "2026-08-09T00:00:00Z".into(),
    }
}

#[test]
fn selected_tape_geometry_rejects_partial_parity_columns() {
    let partial = TapeRecord {
        seal_due: None,
        last_verified_at_utc: None,
        last_verification_state: None,
        last_verified_edition_digest: None,
        data_blocks_per_stripe: Some(16),
        ..capacity_tape_record()
    };
    let error = selected_tape_geometry(&partial, "capacity.projection")
        .expect_err("stray parity geometry must not be interpreted as parity-off");
    assert!(
        error
            .to_string()
            .contains("parity scheme columns must be either all present or all null"),
        "{error}"
    );
}

#[test]
fn physical_extent_projection_prefers_authority_and_fails_closed_on_legacy_overflow() {
    let authoritative = TapeRecord {
        seal_due: None,
        last_verified_at_utc: None,
        last_verification_state: None,
        last_verified_edition_digest: None,
        written_extent_lba: Some(73),
        last_committed_tape_file: Some(u64::MAX),
        total_committed_ordinals: u64::MAX,
        ..capacity_tape_record()
    };
    assert_eq!(tape_physical_used_blocks(&authoritative).unwrap(), 73);

    let empty = capacity_tape_record();
    assert_eq!(tape_physical_used_blocks(&empty).unwrap(), 0);

    let ordinal_only = TapeRecord {
        seal_due: None,
        last_verified_at_utc: None,
        last_verification_state: None,
        last_verified_edition_digest: None,
        total_committed_ordinals: 9,
        ..capacity_tape_record()
    };
    assert_eq!(tape_physical_used_blocks(&ordinal_only).unwrap(), 20);

    let with_file_count = TapeRecord {
        seal_due: None,
        last_verified_at_utc: None,
        last_verification_state: None,
        last_verified_edition_digest: None,
        total_committed_ordinals: 9,
        last_committed_tape_file: Some(3),
        ..capacity_tape_record()
    };
    assert_eq!(tape_physical_used_blocks(&with_file_count).unwrap(), 17);

    let ordinal_overflow = TapeRecord {
        seal_due: None,
        last_verified_at_utc: None,
        last_verification_state: None,
        last_verified_edition_digest: None,
        total_committed_ordinals: u64::MAX,
        ..capacity_tape_record()
    };
    assert!(tape_physical_used_blocks(&ordinal_overflow).is_err());

    let file_count_overflow = TapeRecord {
        seal_due: None,
        last_verified_at_utc: None,
        last_verification_state: None,
        last_verified_edition_digest: None,
        last_committed_tape_file: Some(u64::MAX),
        ..capacity_tape_record()
    };
    assert!(tape_physical_used_blocks(&file_count_overflow).is_err());
}

#[test]
fn selector_fit_uses_barrier_proved_physical_extent_at_the_boundary() {
    let cfg = TapePoolConfig {
        scheme: None,
        id: "capacity.projection".into(),
        display_name: None,
        copy_class: None,
        content_class: None,
        selection_policy: Default::default(),
        watermark_low: 0.92,
        watermark_high: 0.97,
        capacity_cap_bytes: None,
        block_size_bytes: 4096,
        min_object_size_bytes: 0,
    };
    let capacity = raw_capacity_bytes(LtoGen::Lto9);
    let usable = watermark_floor_bytes(capacity, cfg.watermark_high).expect("usable bytes");
    let boundary_lba = usable / cfg.block_size_bytes;
    let full = TapeRecord {
        seal_due: None,
        last_verified_at_utc: None,
        last_verification_state: None,
        last_verified_edition_digest: None,
        written_extent_lba: Some(boundary_lba),
        ..capacity_tape_record()
    };
    let fitting = TapeRecord {
        seal_due: None,
        last_verified_at_utc: None,
        last_verification_state: None,
        last_verified_edition_digest: None,
        tape_uuid: vec![0x72; 16],
        written_extent_lba: Some(boundary_lba - 1),
        ..capacity_tape_record()
    };
    let full_fit = tape_fit_state_from_record(&full, &cfg, &cfg.id, 0).expect("full fit");
    let fitting_fit = tape_fit_state_from_record(&fitting, &cfg, &cfg.id, 1).expect("fitting fit");
    let candidates = [full_fit, fitting_fit];
    let selection = FillOldest.select(&PoolSelectionContext {
        candidates: &candidates,
        projected_footprint: cfg.block_size_bytes,
    });
    assert_eq!(
        selection,
        Selection::UseTape {
            tape_uuid: [0x72; 16]
        }
    );
}

#[test]
fn l3_ordering_filemark_waits_for_final_clean_staged_batch() {
    let mut sink = StagedTestSink::new(2);

    run_staged_transfer(&mut sink, 4, |staged| {
        staged.write_block(&[1; 4])?;
        staged.write_block(&[2; 4])?;
        staged.write_block(&[3; 4])?;
        staged.write_filemarks(1)?;
        Ok(())
    })
    .expect("staged transfer succeeds");

    assert_eq!(
        sink.events,
        vec![
            "position",
            "intent:2:12:2:1",
            "write_batch:2",
            "write_batch:1",
            "span_ok:2:12:2:1",
            "filemark:1",
        ],
        "WRITE FILEMARKS must be actor-ordered after the final clean data batch"
    );
}

#[test]
fn l3_crash_after_producer_read_before_batch_write_leaves_no_tape_bytes() {
    let mut sink = StagedTestSink::with_ring(2, 2);

    let err = run_staged_transfer(&mut sink, 4, |staged| {
        staged.write_block(&[1; 4])?;
        Err::<(), PoolWriteError>(PoolWriteError::InvalidInput(
            "kill after producer read before batch write".to_string(),
        ))
    })
    .expect_err("source-side kill must fail transfer");

    assert!(err.to_string().contains("producer read"));
    assert!(
        sink.inner.blocks.is_empty(),
        "pending process-local buffer must not reach tape after source-side kill"
    );
    assert!(
        !sink
            .events
            .iter()
            .any(|event| event.starts_with("filemark")),
        "source-side failure must not emit filemark: {:?}",
        sink.events
    );
}

#[test]
fn l3_source_error_discards_unsubmitted_window_without_filemark() {
    let mut sink = StagedTestSink::new(2);

    let err = run_staged_transfer(&mut sink, 4, |staged| {
        for value in 0..4u8 {
            staged.write_block(&[value; 4])?;
        }
        Err::<(), PoolWriteError>(PoolWriteError::InvalidInput(
            "injected source error after first batch".to_string(),
        ))
    })
    .expect_err("source-side error must fail transfer");

    assert!(err.to_string().contains("source error"));
    assert_eq!(
        sink.events,
        vec!["position"],
        "an unsubmitted partial ring window is discarded after producer failure"
    );
}

#[test]
fn l3_sink_error_with_queued_buffers_drains_and_poisons_filemark() {
    let mut sink = StagedTestSink::failing_on_batch(2, 2);

    let err = run_staged_transfer(&mut sink, 4, |staged| {
        for value in 0..5u8 {
            staged.write_block(&[value; 4])?;
        }
        staged.write_filemarks(1)?;
        Ok(())
    })
    .expect_err("sink failure must fail transfer");

    assert!(err.to_string().contains("injected sink failure"));
    assert_eq!(
        sink.events,
        vec![
            "position",
            "intent:3:20:2:1",
            "write_batch:2",
            "write_batch:2",
            "span_error",
        ],
        "queued producer buffers are drained after poison, but no filemark reaches the sink"
    );
}

fn fence_test_fixture() -> (tempfile::TempDir, CatalogIndex, SelectedTape) {
    let temp = tempfile::Builder::new()
        .prefix("remanence-transfer-fence-")
        .tempdir()
        .expect("tempdir");
    let mut state = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
    let tape_uuid = [0x5a; 16];
    state
        .provision_tape(remanence_state::ProvisionTapeInput {
            tape_uuid,
            voltag: "FENCE1L9".into(),
            block_size: 4,
            parity: ParityConfig::None,
            force: false,
        })
        .expect("provision tape");
    (
        temp,
        state,
        SelectedTape {
            standalone_endpoint: None,
            pool_id: "fence.test".into(),
            tape_uuid,
            block_size: 4,
            parity_config: ParityConfig::None,
        },
    )
}

fn assert_one_transfer_fence(state: &CatalogIndex, expected_error: &str) {
    crate::tests::assert_fences_survive_empty_rebuild(
        state,
        &state.path().parent().unwrap().join("audit"),
    );
    let fences = state
        .list_active_tape_io_fences()
        .expect("list active tape-I/O fences");
    assert_eq!(
        fences.len(),
        1,
        "exactly one safety funnel persists a fence"
    );
    assert!(
        fences[0]
            .evidence_json
            .as_deref()
            .is_some_and(|evidence| evidence.contains(expected_error)),
        "fence evidence must retain the transfer failure: {fences:?}"
    );
}

#[test]
fn overlap_recovery_cut_matrix_never_projects_partial_object() {
    for cut in [
        "before-first-block",
        "payload",
        "finish-validation",
        "filemark",
    ] {
        let (temp, mut state, selected) = fence_test_fixture();
        let mut sink = StagedTestSink::new(2);
        if cut == "filemark" {
            sink.fail_filemark = true;
        }
        let ring_bytes = crate::append_ring::APPEND_RING_SLAB_BYTES as u64;
        let manager =
            crate::io_memory::IoMemoryReservation::new(ring_bytes).expect("memory manager");
        let (_producer, _consumer, control) =
            crate::append_ring::create_append_ring(&manager, ring_bytes, 90, 25, ring_bytes)
                .expect("ring");
        let cut_control = Arc::clone(&control);
        let mut counted = CountingBlockSink::new(&mut sink, selected.block_size);
        let result = run_counted_fenced_staged_transfer(
            &mut state,
            &selected,
            &mut counted,
            4,
            Some(control),
            |staged| match cut {
                "before-first-block" => Err(PoolWriteError::InvalidInput(
                    "injected disconnect before first block".into(),
                )),
                "payload" => {
                    cut_control.mark_tape_started();
                    staged.write_block(&[0x11; 4])?;
                    Err(PoolWriteError::InvalidInput(
                        "injected disconnect during payload".into(),
                    ))
                }
                "finish-validation" => {
                    cut_control.mark_tape_started();
                    staged.write_block(&[0x22; 4])?;
                    Err(PoolWriteError::InvalidInput(
                        "injected Finish digest disagreement".into(),
                    ))
                }
                "filemark" => {
                    cut_control.mark_tape_started();
                    staged.write_block(&[0x33; 4])?;
                    staged.write_filemarks(1)?;
                    Ok(())
                }
                other => panic!("unhandled cut {other}"),
            },
        );
        assert!(result.is_err(), "cut {cut} must fail closed");
        assert!(
            state
                .list_native_objects()
                .expect("list native objects")
                .is_empty(),
            "cut {cut} must not project an object"
        );
        let fences = state
            .list_active_tape_io_fences()
            .expect("list tape-I/O fences");
        if cut == "before-first-block" {
            assert!(
                fences.is_empty(),
                "pre-write failure has no uncertain tape tail"
            );
        } else {
            assert_eq!(fences.len(), 1, "cut {cut} must fence the tape");
        }

        let retry_pool = "fence.retry";
        let retry_tape = [0x6d; 16];
        state
            .upsert_tape_pool_projection(remanence_state::TapePoolProjectionInput {
                pool_id: retry_pool.into(),
                display_name: None,
                copy_class: None,
                content_class: None,
                created_at_utc: None,
            })
            .expect("project retry pool");
        state
            .provision_tape(remanence_state::ProvisionTapeInput {
                tape_uuid: retry_tape,
                voltag: "RETRY1L9".into(),
                block_size: 4096,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision clean retry tape");
        state
            .project_tape_pool_membership(retry_tape, retry_pool)
            .expect("assign clean retry tape");
        let retry_cfg = TapePoolConfig {
            scheme: None,
            id: retry_pool.into(),
            display_name: None,
            copy_class: None,
            content_class: None,
            selection_policy: Default::default(),
            watermark_low: 0.0001,
            watermark_high: 1.0,
            capacity_cap_bytes: None,
            block_size_bytes: 4096,
            min_object_size_bytes: 0,
        };
        let retry_payload = format!("replay from zero after {cut}").into_bytes();
        let retry_digest: [u8; 32] = Sha256::digest(&retry_payload).into();
        let retry_path = temp.path().join("retry.bin");
        fs::write(&retry_path, &retry_payload).expect("write retained caller source");
        let retry_request = || WriteObjectToPoolRequest {
            pool_id: retry_pool.into(),
            source: WriteObjectSource::Path(retry_path.clone()),
            archive_path: "retry.bin".into(),
            caller_object_id: format!("retry-after-{cut}"),
            expected_content_sha256: Some(retry_digest),
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Plaintext,
        };
        let mut retry_sink = VecBlockSink::new();
        let landed = write_object_to_pool(&mut state, &mut retry_sink, &retry_cfg, retry_request())
            .expect("re-send from byte zero lands on a clean tape");
        assert!(!landed.is_replay());
        let blocks_after_landing = retry_sink.blocks.len();
        let replayed =
            write_object_to_pool(&mut state, &mut retry_sink, &retry_cfg, retry_request())
                .expect("same caller id and digest replays idempotently");
        assert!(replayed.is_replay());
        assert_eq!(retry_sink.blocks.len(), blocks_after_landing);
    }
}

#[test]
fn overlap_staged_only_failure_does_not_raise_a_false_tape_fence() {
    let (_temp, mut state, selected) = fence_test_fixture();
    let mut sink = StagedTestSink::new(2);
    let ring_bytes = crate::append_ring::APPEND_RING_SLAB_BYTES as u64;
    let manager = crate::io_memory::IoMemoryReservation::new(ring_bytes).expect("manager");
    let (_producer, _consumer, control) =
        crate::append_ring::create_append_ring(&manager, ring_bytes, 90, 25, ring_bytes)
            .expect("ring");
    let mut counted = CountingBlockSink::new(&mut sink, selected.block_size);
    let error = run_counted_fenced_staged_transfer(
        &mut state,
        &selected,
        &mut counted,
        4,
        Some(Arc::clone(&control)),
        |staged| {
            staged.write_block(&[0x91; 4])?;
            Err::<(), PoolWriteError>(PoolWriteError::InvalidInput(
                "source failed while first block was still process-local".into(),
            ))
        },
    )
    .expect_err("source failure aborts staged transfer");
    assert!(error.to_string().contains("process-local"), "{error}");
    assert!(!control.tape_started());
    assert!(sink.inner.blocks.is_empty());
    assert!(
        state
            .list_active_tape_io_fences()
            .expect("list fences")
            .is_empty(),
        "no physical WRITE attempt means there is no uncertain tape tail"
    );
}

#[test]
fn producer_error_after_committed_window_records_tape_io_fence() {
    let (_temp, mut state, selected) = fence_test_fixture();
    let mut sink = StagedTestSink::with_ring(2, 2);
    let error = run_fenced_staged_transfer(&mut state, &selected, &mut sink, 4, |staged| {
        for value in 0..4u8 {
            staged.write_block(&[value; 4])?;
        }
        staged.position()?;
        Err::<(), PoolWriteError>(PoolWriteError::InvalidInput(
            "producer source read failed after committed window".into(),
        ))
    })
    .expect_err("producer failure stops transfer");
    assert!(error.to_string().contains("producer source read failed"));
    assert_eq!(sink.batch_calls, 2, "one full ring window reached tape");
    assert_one_transfer_fence(&state, "producer source read failed");
}

#[test]
fn space_to_eod_error_records_tape_io_fence() {
    let (_temp, mut state, selected) = fence_test_fixture();
    let mut sink = StagedTestSink::new(2);
    sink.fail_space_to_eod = true;
    let error = run_fenced_staged_transfer(&mut state, &selected, &mut sink, 4, |staged| {
        staged.space_to_end_of_data().map_err(PoolWriteError::from)
    })
    .expect_err("SPACE(EOD) failure stops transfer");
    assert!(error.to_string().contains("space-to-EOD failure"));
    assert_one_transfer_fence(&state, "space-to-EOD failure");
}

#[test]
fn position_error_records_tape_io_fence() {
    let (_temp, mut state, selected) = fence_test_fixture();
    let mut sink = StagedTestSink::new(2);
    sink.fail_position = true;
    let error = run_fenced_staged_transfer(&mut state, &selected, &mut sink, 4, |staged| {
        staged.write_block(&[1; 4]).map_err(PoolWriteError::from)
    })
    .expect_err("READ POSITION failure stops transfer");
    assert!(error.to_string().contains("READ POSITION failure"));
    assert_one_transfer_fence(&state, "READ POSITION failure");
}

#[test]
fn disconnected_free_ring_cannot_mask_inflight_tape_failure() {
    let mut sink = StagedTestSink::with_ring(2, 2);
    sink.batch_error = Some(TapeIoError::PartialBatchUncommittable {
        requested_records: 2,
        written_records: 1,
        requested_bytes: 8,
        written_bytes: 4,
        end_of_medium: false,
        sense: Some(vec![0x70, 0, 0x40]),
    });
    let ordered = Arc::clone(&sink.ordered_events);
    let accounting = Arc::new(RingAccounting::default());
    let mut buffer = PageAlignedBuffer::try_new(8, accounting).expect("test ring buffer");
    buffer.append(&[1; 8]).expect("fill test ring buffer");
    let mut window = PipelinedWindow::new();
    window
        .push(PipelinedBatch {
            buffer,
            cdb: remanence_scsi::read_write::build_write_fixed_cdb(2),
            records: 2,
            block_size_bytes: 4,
        })
        .expect("one in-flight batch");
    let (free_tx, free_rx) = std_mpsc::sync_channel(2);
    drop(free_rx); // producer-side staged sink disappeared mid-window
    let error = execute_pipelined_window(&mut sink, window, &free_tx, None, &mut |_error| {
        ordered.lock().expect("ordered events").push("fence".into());
        Ok(())
    })
    .expect_err("in-flight tape WRITE fails after producer drops receiver");
    let message = error.to_string();
    assert!(
        message.contains("partial fixed batch uncommittable"),
        "{message}"
    );
    assert!(message.contains("staging buffer return"), "{message}");
    assert!(
        sink.audited_partial_sense,
        "deferred WRITE sense must be audited"
    );
    let ordered = sink.ordered_events.lock().expect("ordered events");
    assert_eq!(&ordered[..3], ["classify", "fence", "audit"]);
}

#[test]
fn filemark_fence_failure_still_flushes_deferred_audit_and_reports_both() {
    let mut sink = StagedTestSink::new(2);
    sink.fail_filemark = true;
    let ordered = Arc::clone(&sink.ordered_events);
    let error = run_staged_transfer_with_safety(
        &mut sink,
        4,
        |staged| staged.write_filemarks(1).map_err(PoolWriteError::from),
        |_error| {
            ordered.lock().expect("ordered events").push("fence".into());
            Err(PoolWriteError::InvalidInput(
                "injected fence callback failure".into(),
            ))
        },
    )
    .expect_err("filemark and fence both fail");
    let message = error.to_string();
    assert!(message.contains("WRITE FILEMARKS failure"), "{message}");
    assert!(message.contains("fence callback failure"), "{message}");
    assert_eq!(
        sink.ordered_events
            .lock()
            .expect("ordered events")
            .as_slice(),
        ["fence", "audit"]
    );
}

#[test]
fn pipelined_ring_rebuilds_trailing_cdb_and_uses_page_aligned_buffers() {
    let mut sink = StagedTestSink::with_ring(2, 4);

    run_staged_transfer(&mut sink, 4, |staged| {
        for value in 0..5u8 {
            staged.write_block(&[value; 4])?;
        }
        Ok(())
    })
    .expect("pipelined transfer succeeds");

    assert_eq!(sink.diagnostic_publications, 1);

    assert_eq!(
        sink.cdbs,
        vec![
            remanence_scsi::read_write::build_write_fixed_cdb(2).to_vec(),
            remanence_scsi::read_write::build_write_fixed_cdb(2).to_vec(),
            remanence_scsi::read_write::build_write_fixed_cdb(1).to_vec(),
        ],
        "the trailing partial buffer must rebuild TRANSFER LENGTH"
    );
    assert!(
        sink.alignments.iter().all(|alignment| *alignment == 0),
        "all submitted payload slices must be page aligned: {:?}",
        sink.alignments
    );
    assert!(sink.events.contains(&"intent:3:20:2:1".to_string()));
    assert!(sink.events.contains(&"span_ok:3:20:2:1".to_string()));
}

#[test]
fn hot_phase_histogram_reports_exact_mean_and_bucketed_tail() {
    let mut histogram = HotPhaseHistogram::default();
    histogram.record(Duration::from_micros(11));
    histogram.record(Duration::from_micros(39));

    assert_eq!(histogram.mean(), 25);
    assert_eq!(histogram.percentile(50, 100), 25);
    assert_eq!(histogram.percentile(95, 100), 50);
    assert_eq!(histogram.max_us, 39);
}

#[test]
fn transfer_stats_include_staging_wait_and_refill_histograms() {
    let mut diagnostics = StagingPhaseDiagnostics::default();
    diagnostics.wait_us.record(Duration::from_micros(11));
    diagnostics.wait_us.record(Duration::from_micros(39));
    diagnostics.refill_us.record(Duration::from_micros(101));

    let mut stats = BlockSinkStats::default();
    stats.record_staging(&diagnostics);

    assert_eq!(stats.staging_wait_samples, 2);
    assert_eq!(stats.staging_wait_mean_us, 25);
    assert_eq!(stats.staging_wait_p95_us, 50);
    assert_eq!(stats.refill_samples, 1);
    assert_eq!(stats.refill_mean_us, 101);
}

#[test]
fn pipelined_synchronous_batch_propagates_successful_early_warning() {
    let mut sink = StagedTestSink::with_ring(2, 4);
    sink.early_warning_on_batch_call = Some(1);

    let outcome = run_staged_transfer(&mut sink, 4, |staged| {
        staged
            .write_block_batch(&[7; 8], 4)
            .map_err(PoolWriteError::from)
    })
    .expect("full-record EW remains successful");

    assert_eq!(outcome.records_written, 2);
    assert_eq!(outcome.bytes_written, 8);
    assert!(outcome.early_warning);
    assert!(!outcome.end_of_medium);
}

#[test]
fn pipelined_terminal_poison_fences_before_audit_and_discards_queued_batches() {
    let mut sink = StagedTestSink::with_ring(2, 4);
    sink.fail_on_batch_call = Some(2);
    let ordered = Arc::clone(&sink.ordered_events);

    let err = run_staged_transfer_with_safety(
        &mut sink,
        4,
        |staged| {
            for value in 0..8u8 {
                staged.write_block(&[value; 4])?;
            }
            staged.write_filemarks(1)?;
            Ok(())
        },
        |error| {
            assert!(error.to_string().contains("injected sink failure"));
            ordered.lock().expect("ordered events").push("fence".into());
            Ok(())
        },
    )
    .expect_err("second hot submission fails");

    assert!(err.to_string().contains("injected sink failure"));
    assert_eq!(
        sink.batch_calls, 2,
        "queued batches must not issue after poison"
    );
    assert!(!sink
        .events
        .iter()
        .any(|event| event.starts_with("filemark")));
    let ordered = sink.ordered_events.lock().expect("ordered events");
    let fence = ordered.iter().position(|event| event == "fence").unwrap();
    let audit = ordered.iter().position(|event| event == "audit").unwrap();
    assert!(
        fence < audit,
        "safety persistence must precede audit: {ordered:?}"
    );
}

#[test]
fn pipelined_diagnostics_reset_for_each_staged_transfer() {
    let mut sink = StagedTestSink::with_ring(2, 4);

    run_staged_transfer(&mut sink, 4, |staged| {
        for value in 0..4u8 {
            staged.write_block(&[value; 4])?;
        }
        Ok(())
    })
    .expect("first transfer succeeds");
    assert_eq!(sink.pipelined_write_diagnostics().ioctl_samples, 2);
    assert_eq!(sink.pipelined_write_diagnostics().ioctl_max_us, 2_000);

    run_staged_transfer(&mut sink, 4, |staged| {
        staged.write_block(&[9; 4])?;
        Ok(())
    })
    .expect("second transfer succeeds");

    assert_eq!(sink.diagnostic_resets, 2);
    assert_eq!(sink.diagnostic_publications, 2);
    assert_eq!(sink.pipelined_write_diagnostics().ioctl_samples, 1);
    assert_eq!(
        sink.pipelined_write_diagnostics().ioctl_max_us,
        1_000,
        "the prior transfer maximum must not survive"
    );
}

#[test]
fn pipelined_ring_rejects_invalid_runtime_depths_and_checked_size_overflow() {
    for ring_buffers in [0, 1, 17] {
        let mut sink = StagedTestSink::with_ring(2, ring_buffers);
        let err = run_staged_transfer(&mut sink, 4, |_staged| Ok::<_, PoolWriteError>(()))
            .expect_err("invalid ring depth rejects before spawning");
        assert!(err.to_string().contains("staging ring depth"), "{err}");
    }

    let mut sink = StagedTestSink::with_ring(u32::MAX, 16);
    let err = run_staged_transfer(&mut sink, usize::MAX, |_staged| Ok::<_, PoolWriteError>(()))
        .expect_err("ring byte multiplication must be checked");
    assert!(
        err.to_string().contains("staging batch bytes overflow"),
        "{err}"
    );
}

#[test]
fn block_sink_stats_latches_hardware_early_warning() {
    let mut stats = BlockSinkStats::default();
    stats.record_block(256 * 1024, true);
    assert!(stats.early_warning);

    let mut stats = BlockSinkStats::default();
    stats.record_filemarks(1, true, Duration::from_millis(7));
    assert!(stats.early_warning);
    assert_eq!(stats.filemark_write_drain, Duration::from_millis(7));

    let mut stats = BlockSinkStats::default();
    stats.record_position(tape_position_with_warning(true));
    assert!(stats.early_warning);
}

#[test]
fn write_failure_with_position_secondary_keeps_partial_batch_fence_reason() {
    let error = TapeIoError::WriteFailureWithPositionError {
        write_error: Box::new(TapeIoError::PartialBatchUncommittable {
            requested_records: 4,
            written_records: 2,
            requested_bytes: 16,
            written_bytes: 8,
            end_of_medium: true,
            sense: Some(vec![0x70, 0, 0x40]),
        }),
        position_error: Box::new(TapeIoError::OperationFailed(
            "injected arbitration READ POSITION failure".into(),
        )),
    };
    let message = error.to_string();
    assert_eq!(
        tape_io_fence_reason_for_transfer_error(&message),
        "partial_batch"
    );
    assert!(message.contains("arbitration READ POSITION failure"));
}

#[test]
fn live_write_counter_advances_during_transfer() {
    let counter = Arc::new(crate::DriveByteCounters::new(0));
    let mut sink = VecBlockSink::new();
    let mut live_sink = LiveCounterBlockSink::new(&mut sink, Arc::clone(&counter), 4);

    let first = live_sink.write_block(b"abc").expect("first write");
    assert_eq!(first.bytes_written, 3);
    assert_eq!(counter.write_bytes(), 3);
    assert!(counter.write_bytes() > 0);
    assert!(counter.write_bytes() < 8);

    live_sink.write_filemarks(1).expect("filemark write");
    assert_eq!(counter.write_bytes(), 3);

    let second = live_sink.write_block(b"defgh").expect("second write");
    assert_eq!(second.bytes_written, 5);
    assert_eq!(counter.write_bytes(), 8);
}

#[test]
fn pool_write_record_to_proto_carries_append_commit_info() {
    let object = PoolWriteObjectRecord {
        object_id: [0x11; 16],
        caller_object_id: "caller-object".to_string(),
        content_sha256: [0x22; 32],
        logical_size_bytes: 123,
        body_format: FORMAT_ID.to_string(),
        created_at_utc: "2026-07-05T00:00:00Z".to_string(),
        copies: vec![PoolWriteObjectCopyRecord {
            tape_log_sequence: None,
            tape_incarnation: None,
            tape_uuid: [0x44; 16],
            tape_file_number: 3,
            first_body_lba: 9,
            pool_id: "camera.copy-a".to_string(),
            representation: OBJECT_COPY_REPRESENTATION_PLAINTEXT.to_string(),
            recipient_epoch_ids: None,
            metadata_frame_len: None,
            plaintext_digest: Some([0x33; 32]),
            stored_digest: Some([0x44; 32]),
        }],
    };

    let proto = object.to_proto();
    let info = proto
        .append_commit_info
        .expect("append commit info from first copy");
    assert_eq!(info.append_mode, pb::AppendMode::Append as i32);
    assert_eq!(info.tape_uuid, vec![0x44; 16]);
    assert_eq!(info.tape_file_number, Some(3));
    assert_eq!(info.first_body_lba, 9);
    assert_eq!(info.position_before_lba, None);
    assert_eq!(info.position_after_lba, None);
    assert_eq!(info.journal_record_ordinal, None);
    assert_eq!(
        proto.copies[0]
            .plaintext_digest
            .as_ref()
            .map(|digest| digest.value.as_slice()),
        Some(&[0x33; 32][..])
    );
    assert_eq!(
        proto.copies[0]
            .stored_digest
            .as_ref()
            .map(|digest| digest.algorithm.as_str()),
        Some("sha256")
    );
}

#[test]
fn written_ack_never_exposes_provisional_tape_locators() {
    let object = PoolWriteObjectRecord {
        object_id: [0x11; 16],
        caller_object_id: "caller-object".to_string(),
        content_sha256: [0x22; 32],
        logical_size_bytes: 123,
        body_format: FORMAT_ID.to_string(),
        created_at_utc: "2026-07-05T00:00:00Z".to_string(),
        copies: vec![PoolWriteObjectCopyRecord {
            tape_log_sequence: None,
            tape_incarnation: None,
            tape_uuid: [0x44; 16],
            tape_file_number: 37,
            first_body_lba: 9,
            pool_id: "camera.copy-a".to_string(),
            representation: OBJECT_COPY_REPRESENTATION_PLAINTEXT.to_string(),
            recipient_epoch_ids: None,
            metadata_frame_len: None,
            plaintext_digest: Some([0x33; 32]),
            stored_digest: Some([0x44; 32]),
        }],
    };
    let batch_id = Uuid::from_bytes([0x55; 16]);

    let proto = object.to_written_proto(batch_id, 4);
    let info = proto.append_commit_info.expect("WRITTEN append info");

    assert!(proto.copies.is_empty(), "copy locators remain invisible");
    assert_eq!(info.durability, pb::AppendDurability::Written as i32);
    assert!(info.tape_uuid.is_empty());
    assert_eq!(info.tape_file_number, None);
    assert_eq!(info.first_body_lba, 0);
    assert_eq!(info.batch_id, batch_id.as_bytes());
    assert_eq!(info.provisional_ordinal, Some(4));
}

#[test]
fn pool_write_record_to_proto_leaves_append_info_absent_without_copies() {
    let object = PoolWriteObjectRecord {
        object_id: [0x11; 16],
        caller_object_id: "caller-object".to_string(),
        content_sha256: [0x22; 32],
        logical_size_bytes: 123,
        body_format: FORMAT_ID.to_string(),
        created_at_utc: "2026-07-05T00:00:00Z".to_string(),
        copies: Vec::new(),
    };

    let proto = object.to_proto();
    assert!(proto.copies.is_empty());
    assert!(proto.append_commit_info.is_none());
}

#[test]
fn append_finish_does_not_double_count() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-pool-write-live-counter")
        .tempdir()
        .expect("tempdir");
    let index_path = temp.path().join("rem-state.sqlite");
    let mut index = CatalogIndex::open(&index_path).expect("open test index");
    let pool_id = "camera.copy-a";
    let tape_uuid = [4u8; 16];
    index
        .upsert_tape_pool_projection(remanence_state::TapePoolProjectionInput {
            pool_id: pool_id.to_string(),
            display_name: None,
            copy_class: Some("copy-a".to_string()),
            content_class: Some("camera".to_string()),
            created_at_utc: None,
        })
        .expect("project pool");
    index
        .provision_tape(remanence_state::ProvisionTapeInput {
            tape_uuid,
            voltag: "RMN001L1".to_string(),
            block_size: 4096,
            parity: ParityConfig::None,
            force: false,
        })
        .expect("provision tape");
    index
        .project_tape_pool_membership(tape_uuid, pool_id)
        .expect("project tape membership");
    let cfg = TapePoolConfig {
        scheme: None,
        id: pool_id.to_string(),
        display_name: None,
        copy_class: Some("copy-a".to_string()),
        content_class: Some("camera".to_string()),
        selection_policy: Default::default(),
        watermark_low: 0.0001,
        watermark_high: 1.0,
        capacity_cap_bytes: None,
        block_size_bytes: 4096,
        min_object_size_bytes: 0,
    };
    let selected = select_tape_in_pool(&index, &cfg, 6, &HashSet::new()).expect("select tape");

    let payload_path = temp.path().join("payload.bin");
    std::fs::write(&payload_path, b"abcdef").expect("write payload");
    let request = WriteObjectToPoolRequest {
        pool_id: pool_id.to_string(),
        source: WriteObjectSource::Path(payload_path.clone()),
        archive_path: PathBuf::from("payload.bin"),
        caller_object_id: "caller-object".to_string(),
        expected_content_sha256: None,
        expected_object_id: None,
        input_kind: crate::WriteObjectInputKind::LogicalFile,
        representation: PoolWriteRepresentation::Plaintext,
    };
    let counter = Arc::new(crate::DriveByteCounters::new(0));
    let mut sink = VecBlockSink::new();
    let result = write_to_selected_tape_with_live_counter(
        &mut index,
        &mut sink,
        &cfg,
        request,
        selected,
        Some(counter.clone()),
    )
    .expect("write object");

    let physical_bytes = sink
        .blocks
        .iter()
        .map(|block| block.len() as u64)
        .sum::<u64>();
    assert!(physical_bytes > 0);
    assert_eq!(counter.write_bytes(), physical_bytes);
    assert_eq!(result.object.logical_size_bytes, 6);
}

#[test]
fn lto_generation_parses_m8_type_m_suffix() {
    assert_eq!(
        MediaObservation::label_hint("RMN001M8").map(|observation| observation.generation),
        Some(LtoGen::M8)
    );
    assert_eq!(
        MediaObservation::label_hint("rmn001m8").map(|observation| observation.generation),
        Some(LtoGen::M8)
    );
    assert_eq!(raw_capacity_bytes(LtoGen::M8), 9_000_000_000_000);
}

#[test]
fn lto_generation_treats_lz_as_lto9_media_class() {
    assert_eq!(
        MediaObservation::label_hint("RMN001LZ").map(|observation| observation.generation),
        Some(LtoGen::Lto9)
    );
    assert_eq!(
        MediaObservation::label_hint("rmn001lz").map(|observation| observation.generation),
        Some(LtoGen::Lto9)
    );
    assert_eq!(raw_capacity_bytes(LtoGen::Lto9), 18_000_000_000_000);
}

#[test]
fn lto_generation_rejects_non_ascii_without_panic() {
    assert_eq!(
        MediaObservation::label_hint("éX").map(|observation| observation.generation),
        None
    );
}

#[test]
fn lto_drive_generation_parses_common_inquiry_products() {
    assert_eq!(
        lto_generation_from_drive_product("Ultrium 9-SCSI"),
        Some(LtoGen::Lto9)
    );
    assert_eq!(
        lto_generation_from_drive_product("LTO-8 HH"),
        Some(LtoGen::Lto8)
    );
    assert_eq!(lto_generation_from_drive_product("unknown"), None);
}

#[test]
fn lto_read_compatibility_uses_design_table() {
    let cases = [
        (
            LtoGen::Lto5,
            &[LtoGen::Lto5, LtoGen::Lto4, LtoGen::Lto3][..],
        ),
        (
            LtoGen::Lto6,
            &[LtoGen::Lto6, LtoGen::Lto5, LtoGen::Lto4][..],
        ),
        (
            LtoGen::Lto7,
            &[LtoGen::Lto7, LtoGen::Lto6, LtoGen::Lto5][..],
        ),
        (LtoGen::Lto8, &[LtoGen::Lto8, LtoGen::Lto7, LtoGen::M8][..]),
        (LtoGen::Lto9, &[LtoGen::Lto9, LtoGen::Lto8][..]),
    ];
    let all_tapes = [
        LtoGen::Lto1,
        LtoGen::Lto2,
        LtoGen::Lto3,
        LtoGen::Lto4,
        LtoGen::Lto5,
        LtoGen::Lto6,
        LtoGen::Lto7,
        LtoGen::M8,
        LtoGen::Lto8,
        LtoGen::Lto9,
    ];

    for (drive, readable) in cases {
        for tape in all_tapes {
            assert_eq!(
                can_read(drive, tape),
                readable.contains(&tape),
                "drive={drive:?} tape={tape:?}"
            );
        }
    }
    assert!(!can_read(LtoGen::Lto8, LtoGen::Lto6));
    assert!(!can_read(LtoGen::Lto9, LtoGen::Lto7));
    assert!(!can_read(LtoGen::Lto9, LtoGen::M8));
}

#[test]
fn lto_write_compatibility_uses_design_table() {
    let cases = [
        (LtoGen::Lto5, &[LtoGen::Lto5, LtoGen::Lto4][..]),
        (LtoGen::Lto6, &[LtoGen::Lto6, LtoGen::Lto5][..]),
        (LtoGen::Lto7, &[LtoGen::Lto7, LtoGen::Lto6][..]),
        (LtoGen::Lto8, &[LtoGen::Lto8, LtoGen::Lto7, LtoGen::M8][..]),
        (LtoGen::Lto9, &[LtoGen::Lto9, LtoGen::Lto8][..]),
    ];
    let all_tapes = [
        LtoGen::Lto1,
        LtoGen::Lto2,
        LtoGen::Lto3,
        LtoGen::Lto4,
        LtoGen::Lto5,
        LtoGen::Lto6,
        LtoGen::Lto7,
        LtoGen::M8,
        LtoGen::Lto8,
        LtoGen::Lto9,
    ];

    for (drive, writable) in cases {
        for tape in all_tapes {
            assert_eq!(
                can_write(drive, tape),
                writable.contains(&tape),
                "drive={drive:?} tape={tape:?}"
            );
        }
    }
    assert!(!can_write(LtoGen::Lto8, LtoGen::Lto6));
    assert!(!can_write(LtoGen::Lto9, LtoGen::Lto7));
    assert!(!can_write(LtoGen::Lto9, LtoGen::M8));
}

// -- pinned-tape admission matrix ---------------------------------------
//
// Pinning replaces selection, never admission: these tests pin the
// refusals. The batch-eligibility branch (committed tape without an
// adopted checkpoint journal) reuses the same helpers the pool-mode
// selection tests already exercise and needs a written tape to stage, so
// it is not duplicated here.

struct PinnedFixture {
    index: CatalogIndex,
    pool_cfg: TapePoolConfig,
    journal_dir: std::path::PathBuf,
    _temp: tempfile::TempDir,
}

const PINNED_POOL: &str = "camera.copy-a";
const PINNED_TAPE: TapeUuid = [0x5a; 16];

fn pinned_fixture() -> PinnedFixture {
    let temp = tempfile::Builder::new()
        .prefix("remanence-pinned-admission-")
        .tempdir()
        .expect("tempdir");
    let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
    index
        .upsert_tape_pool_projection(remanence_state::TapePoolProjectionInput {
            pool_id: PINNED_POOL.to_string(),
            display_name: None,
            copy_class: Some("copy-a".to_string()),
            content_class: Some("camera".to_string()),
            created_at_utc: None,
        })
        .expect("project pool");
    index
        .provision_tape(remanence_state::ProvisionTapeInput {
            tape_uuid: PINNED_TAPE,
            voltag: "PIN001L9".to_string(),
            block_size: 4096,
            parity: ParityConfig::None,
            force: false,
        })
        .expect("provision tape");
    index
        .project_tape_pool_membership(PINNED_TAPE, PINNED_POOL)
        .expect("project membership");
    let pool_cfg = TapePoolConfig {
        scheme: None,
        id: PINNED_POOL.to_string(),
        display_name: None,
        copy_class: Some("copy-a".to_string()),
        content_class: Some("camera".to_string()),
        selection_policy: Default::default(),
        watermark_low: 0.98,
        watermark_high: 1.0,
        capacity_cap_bytes: None,
        block_size_bytes: 4096,
        min_object_size_bytes: 0,
    };
    let journal_dir = temp.path().join("checkpoint-journals");
    std::fs::create_dir_all(&journal_dir).expect("journal dir");
    PinnedFixture {
        index,
        pool_cfg,
        journal_dir,
        _temp: temp,
    }
}

fn admit(
    fixture: &PinnedFixture,
    tape_uuid: TapeUuid,
    guard: &str,
) -> Result<SelectedTape, PinnedTapeError> {
    match admit_pinned_tape_for_write_session(
        &fixture.index,
        tape_uuid,
        guard,
        &fixture.pool_cfg,
        &fixture.journal_dir,
    )? {
        PinnedWriteDisposition::Writable(selected) => Ok(selected),
        PinnedWriteDisposition::HostOnlyTerminalRecovery(_) => {
            panic!("ordinary pinned-admission helper received recovery-only disposition")
        }
    }
}

#[test]
fn pinned_admission_accepts_matching_pooled_tape() {
    let fixture = pinned_fixture();
    let selected = admit(&fixture, PINNED_TAPE, PINNED_POOL).expect("admit pinned tape");
    assert_eq!(selected.tape_uuid, PINNED_TAPE);
    assert_eq!(selected.pool_id, PINNED_POOL);
    assert_eq!(selected.block_size, 4096);
}

#[test]
fn pinned_admission_refuses_unknown_tape() {
    let fixture = pinned_fixture();
    let error = admit(&fixture, [0x77; 16], PINNED_POOL).unwrap_err();
    assert!(
        matches!(error, PinnedTapeError::UnknownTape { .. }),
        "{error}"
    );
    // The message must teach the uninitialized-cartridge case.
    assert!(error.to_string().contains("rem tape init"), "{error}");
}

#[test]
fn pinned_admission_refuses_pool_guard_mismatch_naming_both_pools() {
    let fixture = pinned_fixture();
    let error = admit(&fixture, PINNED_TAPE, "offsite.copy-b").unwrap_err();
    match &error {
        PinnedTapeError::PoolGuardMismatch {
            required_pool_id,
            actual_pool_id,
            ..
        } => {
            assert_eq!(required_pool_id, "offsite.copy-b");
            assert_eq!(actual_pool_id.as_deref(), Some(PINNED_POOL));
        }
        other => panic!("expected PoolGuardMismatch, got {other}"),
    }
    let message = error.to_string();
    assert!(message.contains("offsite.copy-b"), "{message}");
    assert!(message.contains(PINNED_POOL), "{message}");
}

#[test]
fn pinned_admission_refuses_unpooled_tape_under_a_guard() {
    let fixture = pinned_fixture();
    let unpooled: TapeUuid = [0x5b; 16];
    {
        let mut index =
            CatalogIndex::open(fixture._temp.path().join("rem-state.sqlite")).expect("reopen");
        index
            .provision_tape(remanence_state::ProvisionTapeInput {
                tape_uuid: unpooled,
                voltag: "PIN002L9".to_string(),
                block_size: 4096,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision unpooled tape");
    }
    let fresh = CatalogIndex::open(fixture._temp.path().join("rem-state.sqlite")).expect("reopen");
    let error = admit_pinned_tape_for_write_session(
        &fresh,
        unpooled,
        PINNED_POOL,
        &fixture.pool_cfg,
        &fixture.journal_dir,
    )
    .unwrap_err();
    match &error {
        PinnedTapeError::PoolGuardMismatch { actual_pool_id, .. } => {
            assert_eq!(actual_pool_id, &None);
        }
        other => panic!("expected PoolGuardMismatch, got {other}"),
    }
}

#[test]
fn pinned_admission_refuses_cleaning_cartridge() {
    let fixture = pinned_fixture();
    let cleaning = {
        let mut index =
            CatalogIndex::open(fixture._temp.path().join("rem-state.sqlite")).expect("reopen");
        let record = index
            .ensure_cleaning_cartridge("CLN901L9")
            .expect("cleaning cartridge");
        let mut uuid = [0u8; 16];
        uuid.copy_from_slice(&record.tape_uuid);
        // A cleaning cartridge must refuse even if someone projected it
        // into the guarded pool.
        index
            .project_tape_pool_membership(uuid, PINNED_POOL)
            .expect("project cleaning membership");
        uuid
    };
    let fresh = CatalogIndex::open(fixture._temp.path().join("rem-state.sqlite")).expect("reopen");
    let error = admit_pinned_tape_for_write_session(
        &fresh,
        cleaning,
        PINNED_POOL,
        &fixture.pool_cfg,
        &fixture.journal_dir,
    )
    .unwrap_err();
    assert!(
        matches!(error, PinnedTapeError::NotADataTape { .. }),
        "{error}"
    );
}

#[test]
fn pinned_admission_refuses_sealed_tape() {
    let fixture = pinned_fixture();
    {
        let mut index =
            CatalogIndex::open(fixture._temp.path().join("rem-state.sqlite")).expect("reopen");
        index.seal_tape(PINNED_TAPE).expect("seal tape");
    }
    let fresh = CatalogIndex::open(fixture._temp.path().join("rem-state.sqlite")).expect("reopen");
    let error = admit_pinned_tape_for_write_session(
        &fresh,
        PINNED_TAPE,
        PINNED_POOL,
        &fixture.pool_cfg,
        &fixture.journal_dir,
    )
    .unwrap_err();
    match &error {
        PinnedTapeError::NotWritable { reason, .. } => {
            assert!(
                matches!(reason, WritabilityError::NotReady { .. }),
                "{reason}"
            );
        }
        other => panic!("expected NotWritable, got {other}"),
    }
}

#[test]
fn pinned_admission_refuses_fenced_tape() {
    let fixture = pinned_fixture();
    {
        let mut index =
            CatalogIndex::open(fixture._temp.path().join("rem-state.sqlite")).expect("reopen");
        index
            .record_tape_io_fence(remanence_state::TapeIoFenceInput {
                tape_uuid: PINNED_TAPE,
                barcode: Some("PIN001L9".to_string()),
                reason: "partial_batch".to_string(),
                evidence_json: None,
            })
            .expect("record fence");
    }
    let fresh = CatalogIndex::open(fixture._temp.path().join("rem-state.sqlite")).expect("reopen");
    let error = admit_pinned_tape_for_write_session(
        &fresh,
        PINNED_TAPE,
        PINNED_POOL,
        &fixture.pool_cfg,
        &fixture.journal_dir,
    )
    .unwrap_err();
    match &error {
        PinnedTapeError::Fenced { reason, .. } => {
            assert_eq!(reason, "partial_batch");
        }
        other => panic!("expected Fenced, got {other}"),
    }
}

/// The fresh no-parity writer records both injected bootstrap diagnostics.
#[test]
fn no_parity_bootstrap_records_fixed_writer_identity() {
    let mut sink = VecBlockSink::new();
    let identity = remanence_parity::WriterIdentity::fixed(
        "remanence-test".into(),
        std::time::SystemTime::UNIX_EPOCH.into(),
    );
    write_no_parity_bootstrap(&mut sink, [0x81; 16], 4096, &identity)
        .expect("write fresh no-parity bootstrap");
    let bootstrap = remanence_parity::bootstrap::parse_bootstrap_block(&sink.blocks[0])
        .expect("decode written BOT bootstrap");
    assert!(bootstrap.no_parity_flag);
    assert_eq!(bootstrap.written_by_version, identity.software());
    assert_eq!(bootstrap.written_at, "1970-01-01T00:00:00Z");
}

/// REM-PARITY 8.2: the Writer's no-parity bootstrap carries no scheme record,
/// and its frame parses back as a no-parity bootstrap without one.
#[test]
fn no_parity_tape_bootstrap_carries_no_scheme_record() {
    let payload = build_tape_bootstrap(
        [7u8; 16],
        4096,
        ParityConfig::None,
        "2026-09-30T00:00:00Z",
        "0.1.0",
    );
    assert!(payload.no_parity_flag);
    assert!(payload.scheme.is_none());
    let mut block = vec![0u8; 4096];
    write_bootstrap_block(&payload, &mut block).expect("no-parity bootstrap writes");
    let parsed = remanence_parity::bootstrap::parse_bootstrap_block(&block).expect("parses");
    assert!(parsed.no_parity_flag);
    assert!(parsed.scheme.is_none());
}

/// Both wrappers must count filemarks before reporting the following block.
#[tokio::test]
async fn overlap_and_staged_filemarks_advance_positions() {
    for staged_mode in [false, true] {
        for immediate in [false, true] {
            let capacity = 2 * crate::append_ring::APPEND_RING_SLAB_BYTES as u64;
            let manager = crate::io_memory::IoMemoryReservation::new(capacity).unwrap();
            let (mut producer, _consumer, control) =
                crate::append_ring::create_append_ring(&manager, capacity, 50, 25, capacity)
                    .unwrap();
            producer
                .push(&vec![0x11; crate::append_ring::APPEND_RING_SLAB_BYTES])
                .await
                .unwrap();
            let mut sink = StagedTestSink::new(2);
            let exercise = |inner: &mut dyn BlockSink| -> Result<(), PoolWriteError> {
                let mut gated = OverlapBlockSink {
                    inner,
                    control,
                    expected_initial_lba: 0,
                    expected_next_lba: 0,
                    initial_position_proved: false,
                    write_started: false,
                    low_water_events: 0,
                };
                assert_eq!(gated.write_block(&[1; 4])?.position_after.lba, 1);
                if immediate {
                    gated.write_filemarks_immediate(1)?;
                } else {
                    assert_eq!(gated.write_filemarks(1)?.position_after.lba, 2);
                }
                assert_eq!(gated.expected_next_lba, 2);
                assert_eq!(gated.write_block(&[2; 4])?.position_after.lba, 3);
                assert_eq!(gated.expected_next_lba, 3);
                Ok(())
            };
            if staged_mode {
                run_staged_transfer(&mut sink, 4, exercise).unwrap();
            } else {
                exercise(&mut sink).unwrap();
            }
            assert_eq!(sink.position().unwrap().lba, 3);
        }
    }
}

/// The receive digest replaces both file scans and remains a write-time guard.
#[test]
fn spool_receive_digest_matches_file_passes_and_large_multichunk_stream() {
    use sha2::{Digest, Sha256};
    let temp = tempfile::tempdir().unwrap();
    let large: Vec<u8> = (0..17 * 1024 * 1024 + 37)
        .map(|n| (n % 251) as u8)
        .collect();
    let fixtures =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/rem-object/objects");
    let d1 = std::fs::read(fixtures.join("rem-object-tv-d1-encrypted.rem-object")).unwrap();
    let e2 = std::fs::read(fixtures.join("rem-object-tv-e2.rem-object")).unwrap();
    for payload in [
        b"".as_slice(),
        b"abc",
        b"hello world",
        d1.as_slice(),
        e2.as_slice(),
        large.as_slice(),
    ] {
        let mut spool =
            crate::append_spool::Spool::create(temp.path(), payload.len() as u64).unwrap();
        for chunk in payload.chunks(65537) {
            spool.write_chunk(chunk).unwrap();
        }
        let received = spool.content_sha256();
        let path = spool.finish().unwrap();
        assert_eq!(received, <[u8; 32]>::from(Sha256::digest(payload)));
        assert_eq!(received, sha256_file(&path).unwrap());
        let old = remanence_stream::prepare_regular_file(&path, "payload.bin", "member").unwrap();
        assert_eq!(Some(received), old.spec.file_sha256);
        let request = WriteObjectToPoolRequest {
            pool_id: "digest-test".into(),
            source: WriteObjectSource::Spool {
                path: path.clone(),
                mtime: Some("123.5".into()),
                content_sha256: received,
                received_bytes: payload.len() as u64,
                staging_reason: "configured_serial",
            },
            archive_path: PathBuf::from("payload.bin"),
            caller_object_id: "digest-test".into(),
            expected_content_sha256: Some(received),
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Plaintext,
        };
        // A cached digest survives source modification at preparation; the byte writer
        // retains responsibility for checking that the bytes written match this digest.
        if !payload.is_empty() {
            std::fs::write(&path, vec![0xff; payload.len()]).unwrap();
        }
        let prepared = prepare_pool_object(&request, 256 * 1024).unwrap();
        assert_eq!(prepared.content_sha256, received);
        assert_eq!(prepared.files[0].spec.file_sha256, old.spec.file_sha256);
        assert_eq!(prepared.files[0].spec.size_bytes, old.spec.size_bytes);
        assert_eq!(prepared.files[0].spec.mtime.as_deref(), Some("123.5"));
        if !payload.is_empty() {
            let mut readers = open_prepared_readers(&prepared).unwrap();
            let mut streams = vec![remanence_format::RemTarFileStream::new(
                prepared.files[0].spec.clone(),
                readers[0].as_mut(),
            )];
            let error = remanence_format::write_rem_tar_object_from_readers(
                &mut VecBlockSink::new(),
                &prepared.options,
                &mut streams,
            )
            .unwrap_err();
            assert!(error.to_string().contains("streamed data hash"), "{error}");
        }
        // Both truncation and growth disagree with the measured receipt before motion.
        for length in [payload.len().saturating_sub(1), payload.len() + 1] {
            if length == payload.len() {
                continue;
            }
            std::fs::write(&path, vec![0; length]).unwrap();
            assert!(prepare_pool_object(&request, 256 * 1024)
                .err()
                .unwrap()
                .to_string()
                .contains("spool size differs from receipt"));
        }
        std::fs::remove_file(path).unwrap();
    }
}

/// Timing observes successful and failed calls while forwarding bytes and ordering.
#[test]
fn parity_raw_timing_separates_commands_from_host_gaps() {
    use remanence_parity::{PhysicalPositionHint, RawWriteOutcome};
    struct Raw {
        calls: Vec<String>,
    }
    impl RawTapeSink for Raw {
        fn locate_for_overwrite(&mut self, hint: PhysicalPositionHint) -> Result<(), ParityError> {
            self.calls.push(format!("locate:{}", hint.lba));
            Ok(())
        }
        fn write_fixed_block(&mut self, bytes: &[u8]) -> Result<RawWriteOutcome, ParityError> {
            self.calls.push(format!("block:{bytes:?}"));
            std::thread::sleep(Duration::from_millis(3));
            if bytes == b"fail" {
                return Err(ParityError::TapeIo(TapeIoError::OperationFailed(
                    "injected".into(),
                )));
            }
            Ok(RawWriteOutcome::WroteBlock {
                bytes_written: bytes.len() as u32,
                position_after: PhysicalPositionHint::new(1),
                early_warning: false,
                end_of_medium: false,
            })
        }
        fn write_filemarks(
            &mut self,
            count: u32,
            immed: bool,
        ) -> Result<RawWriteOutcome, ParityError> {
            self.calls.push(format!("filemarks:{count}:{immed}"));
            Ok(RawWriteOutcome::WroteFilemark {
                position_after: PhysicalPositionHint::new(2),
                early_warning: false,
                end_of_medium: false,
            })
        }
        fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            self.calls.push("position".into());
            Ok(PhysicalPositionHint::new(2))
        }
    }
    let mut raw = Raw { calls: Vec::new() };
    let stats = std::cell::Cell::new(BlockSinkStats::default());
    {
        let mut timed = TimedRawTapeSink {
            inner: &mut raw,
            stats: &stats,
            last_write_finished: None,
        };
        assert_eq!(
            timed.write_fixed_block(b"ok").unwrap().position_after().lba,
            1
        );
        let before = stats.get().write_command_elapsed;
        std::thread::sleep(Duration::from_millis(5));
        timed.write_filemarks(1, true).unwrap();
        assert_eq!(timed.position().unwrap().lba, 2);
        timed
            .locate_for_overwrite(PhysicalPositionHint::new(7))
            .unwrap();
        let error = timed.write_fixed_block(b"fail").unwrap_err();
        assert!(error.to_string().contains("injected"));
        assert!(stats.get().write_command_elapsed >= before + Duration::from_millis(3));
        assert!(stats.get().between_write_elapsed >= Duration::from_millis(5));
    }
    assert_eq!(
        raw.calls,
        [
            "block:[111, 107]",
            "filemarks:1:true",
            "position",
            "locate:7",
            "block:[102, 97, 105, 108]"
        ]
    );
    assert_eq!(stats.get().block_write_bytes, 2);
    assert_eq!(stats.get().filemark_calls, 1);
    assert_eq!(stats.get().position_calls, 1);
}

/// Catalog binding and request validation cannot be overridden by a forged selection.
#[test]
fn selected_geometry_and_request_guards_reject_forged_pool_and_parity() {
    let f = pinned_fixture();
    let selected = admit(&f, PINNED_TAPE, PINNED_POOL).unwrap();
    let mut forged_cfg = f.pool_cfg.clone();
    forged_cfg.id = "forged-pool".into();
    let mut forged = selected.clone();
    forged.pool_id = forged_cfg.id.clone();
    assert!(matches!(
        ensure_selected_tape_binding(&f.index, &forged_cfg, &forged),
        Err(PoolWriteError::InvalidInput(_))
    ));
    let mut wrong_parity = selected.clone();
    wrong_parity.parity_config = ParityConfig::Scheme(
        remanence_parity::default_scheme_for_block_size(selected.block_size),
    );
    assert!(matches!(
        ensure_selected_tape_binding(&f.index, &f.pool_cfg, &wrong_parity),
        Err(PoolWriteError::MissingTapeGeometry(_))
    ));
    let request = WriteObjectToPoolRequest {
        pool_id: "wrong-pool".into(),
        source: WriteObjectSource::Path("unreached".into()),
        archive_path: "unreached.bin".into(),
        caller_object_id: "invalid".into(),
        expected_content_sha256: None,
        expected_object_id: None,
        input_kind: WriteObjectInputKind::LogicalFile,
        representation: PoolWriteRepresentation::Plaintext,
    };
    assert!(matches!(
        ensure_request_pool_matches_config(&request, &f.pool_cfg),
        Err(PoolWriteError::InvalidInput(_))
    ));
}
