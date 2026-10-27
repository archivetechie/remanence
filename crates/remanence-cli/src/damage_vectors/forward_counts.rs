//! Count real transport commands by protected-pass phase. The decorator tracks
//! returned positions; it never calls position/LOCATE/SPACE to observe a read.
use super::*;
use remanence_parity::raw::RawReadBatch;
use remanence_parity::verify_protected::{
    verify_protected_budget_for_test, verify_protected_step1_for_test,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Setup,
    Sweep,
    Sidecar,
    Fallback,
}

struct Counter<S> {
    inner: S,
    log: remanence_library::RecordingLog,
    phases: Vec<Phase>,
    phase: Phase,
    files: Vec<(u64, u64, TapeFileKind)>,
    data: BTreeMap<u64, u64>, // LBA -> protecting sidecar file
    parity_ends: BTreeMap<u64, u64>,
    started: std::collections::BTreeSet<u64>,
    active: Option<u64>,
    cursor: Option<u64>,
    reads: Vec<(u64, Phase)>,
    late_data: Vec<u64>,
    configured: bool,
    forward_sidecars: bool,
    sidecar_locates: BTreeMap<u64, usize>,
    setup: bool,
}

impl<S: RawTapeSource> Counter<S> {
    fn account<T>(&mut self, f: impl FnOnce(&mut S) -> T) -> T {
        let result = f(&mut self.inner);
        self.phases.resize(self.log.borrow().len(), self.phase);
        result
    }
    fn read_addresses(&mut self, start: u64, count: u32) {
        for address in start..start + u64::from(count) {
            self.reads.push((address, self.phase));
            if self
                .data
                .get(&address)
                .is_some_and(|owner| self.started.contains(owner))
            {
                self.late_data.push(address);
            }
        }
        if self
            .active
            .is_some_and(|file| self.parity_ends.get(&file) == Some(&(start + u64::from(count))))
        {
            self.active = None;
        }
    }
    fn counts(&self) -> [[usize; 3]; 4] {
        let mut counts = [[0; 3]; 4];
        let commands = self.log.borrow();
        assert_eq!(commands.len(), self.phases.len());
        for (cdb, phase) in commands.iter().zip(&self.phases) {
            let column = match cdb[0] {
                0x92 => 0,
                0x11 | 0x91 => 1,
                0x34 => 2,
                _ => continue,
            };
            counts[*phase as usize][column] += 1;
        }
        counts
    }
}
impl<S: RawTapeSource> RawTapeSource for Counter<S> {
    fn verification_context(&self) -> remanence_library::verification_io::VerificationContext {
        self.inner.verification_context()
    }

    fn configure_fixed_block_size(&mut self, size: u32) -> Result<(), ParityError> {
        self.phase = if self.setup && !self.configured {
            Phase::Setup
        } else {
            Phase::Sidecar
        };
        if self.phase == Phase::Sidecar {
            let file = self
                .files
                .iter()
                .find(|(file, _, kind)| {
                    *kind == TapeFileKind::ParitySidecar && !self.started.contains(file)
                })
                .expect("one configuration per sidecar")
                .0;
            self.started.insert(file);
            self.active = Some(file);
        }
        self.configured = true;
        self.account(|s| s.configure_fixed_block_size(size))
    }
    fn locate_physical(&mut self, hint: PhysicalPositionHint) -> Result<(), ParityError> {
        let (file, _, kind) = self
            .files
            .iter()
            .rev()
            .find(|(_, start, _)| *start <= hint.lba)
            .copied()
            .unwrap();
        self.phase = match kind {
            TapeFileKind::Object => {
                if self
                    .data
                    .get(&hint.lba)
                    .is_some_and(|owner| self.started.contains(owner))
                {
                    Phase::Fallback
                } else {
                    Phase::Sweep
                }
            }
            TapeFileKind::ParitySidecar => {
                if self.forward_sidecars {
                    let start = self.files.iter().find(|(f, _, _)| *f == file).unwrap().1;
                    assert!(
                        self.reads
                            .iter()
                            .filter(|(lba, _)| start <= *lba && *lba < self.parity_ends[&file])
                            .all(|(lba, _)| *lba < hint.lba),
                        "backward sidecar LOCATE to {hint:?}"
                    );
                    let locates = self.sidecar_locates.entry(file).or_default();
                    *locates += 1;
                    assert!(
                        *locates <= 1,
                        "sidecar {file} has more than one entry LOCATE"
                    );
                }
                self.started.insert(file);
                self.active = Some(file);
                Phase::Sidecar
            }
            TapeFileKind::ParityMap if self.active.is_some() => Phase::Sidecar,
            TapeFileKind::ParityMap if self.phase == Phase::Setup => Phase::Setup,
            TapeFileKind::ParityMap => Phase::Sweep,
            TapeFileKind::Bootstrap if self.phase == Phase::Setup => Phase::Setup,
            _ => panic!("unexpected protected-pass LOCATE {hint:?}"),
        };
        self.cursor = None;
        self.account(|s| s.locate_physical(hint))?;
        self.cursor = Some(hint.lba);
        Ok(())
    }
    fn locate_end_of_data(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        self.account(|s| s.locate_end_of_data())
    }
    fn space_filemarks(&mut self, n: i64) -> Result<SpaceFilemarksOutcome, ParityError> {
        self.account(|s| s.space_filemarks(n))
    }
    fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        self.account(|s| s.position())
    }
    fn read_record(&mut self, buf: &mut [u8]) -> Result<RawReadOutcome, ParityError> {
        let start = self
            .cursor
            .expect("all protected reads explicitly positioned");
        let result = self.account(|s| s.read_record(buf));
        self.read_addresses(start, 1);
        self.cursor = result.as_ref().ok().map(|r| r.position_after().lba);
        result
    }
    fn read_records(
        &mut self,
        buf: &mut [u8],
        size: u32,
        max: u32,
        remaining: u32,
    ) -> RawReadBatch {
        let start = self.cursor.expect("batch positioned");
        let result = self.account(|s| s.read_records(buf, size, max, remaining));
        assert!(
            matches!(result.end, remanence_parity::raw::RawReadEnd::Complete),
            "healthy fixture batch {result:?}"
        );
        self.read_addresses(start, result.records_read);
        self.cursor = result.position_after.map(|p| p.lba);
        result
    }
}

fn counter<S: RawTapeSource>(
    inner: S,
    log: remanence_library::RecordingLog,
    vector: &VectorImage,
    setup: bool,
) -> Counter<S> {
    let map = &vector.written.map;
    let mut data = BTreeMap::new();
    let mut parity_ends = BTreeMap::new();
    for sidecar in map
        .entries()
        .iter()
        .filter(|e| e.kind == TapeFileKind::ParitySidecar)
    {
        let file = &vector.image.files[sidecar.tape_file_number as usize];
        parity_ends.insert(
            sidecar.tape_file_number,
            file.start_record as u64
                + if setup {
                    sidecar.block_count
                } else {
                    let header = remanence_parity::sidecar::parse_sidecar_header_block(
                        &file.bytes[..BLOCK as usize],
                        &vector.written.inputs.tape_uuid,
                    )
                    .unwrap();
                    header.shard_index_block_count + header.parity_block_count
                },
        );
        for object in map
            .entries()
            .iter()
            .filter(|e| e.kind == TapeFileKind::Object)
        {
            let first = object.first_parity_data_ordinal.unwrap();
            for ordinal in first.max(sidecar.protected_ordinal_start.unwrap())
                ..(first + object.block_count).min(sidecar.protected_ordinal_end_exclusive.unwrap())
            {
                let address = map
                    .physical_position(TapeFilePosition {
                        tape_file_number: object.tape_file_number,
                        block_within_file: ordinal - first,
                    })
                    .unwrap()
                    .lba;
                assert!(data.insert(address, sidecar.tape_file_number).is_none());
            }
        }
    }
    Counter {
        inner,
        log,
        phases: vec![],
        phase: Phase::Setup,
        files: map
            .entries()
            .iter()
            .map(|e| {
                (
                    e.tape_file_number,
                    vector.image.files[e.tape_file_number as usize].start_record as u64,
                    e.kind,
                )
            })
            .collect(),
        data,
        parity_ends,
        started: Default::default(),
        active: None,
        cursor: None,
        reads: vec![],
        late_data: vec![],
        configured: false,
        forward_sidecars: setup,
        sidecar_locates: BTreeMap::new(),
        setup,
    }
}

#[test]
fn four_epoch_transport_counts_by_phase_and_pending_high_water() {
    use crate::tape_image_vectors::{inputs, object, recipe, write_model};
    let stripes = 16;
    let mut inputs = inputs("two-epoch");
    inputs.scheme.stripes_per_neighborhood = stripes;
    inputs.objects = [6 * stripes as usize, stripes as usize, stripes as usize]
        .into_iter()
        .enumerate()
        .map(|(index, blocks)| {
            let mut object = object(4, index);
            object.options.chunk_size = BLOCK as usize;
            let bytes = vec![0x35; (blocks - 3) * BLOCK as usize];
            let mut spec = object.files[0].spec.clone();
            spec.size_bytes = bytes.len() as u64;
            spec.file_sha256 = Some(Sha256::digest(&bytes).into());
            object.files = vec![crate::tape_image::TapeImageFile::from_bytes(spec, bytes)];
            object
        })
        .collect();
    inputs.checkpoint_after_objects.clear();
    inputs.nominal_extent_bytes = 20 * u64::from(BLOCK);
    let recipe = recipe(&inputs);
    let (written, image) = write_model(inputs).expect("four-epoch image");
    let vector = VectorImage {
        written,
        image,
        recipe,
    };
    let map = ScopedFilemarkMap::from_catalog(vector.written.map.clone(), 8 * u64::from(stripes));
    assert_eq!(
        map.map
            .entries()
            .iter()
            .filter(|e| e.kind == TapeFileKind::Object)
            .map(|e| e.block_count)
            .collect::<Vec<_>>(),
        vec![96, 16, 16]
    );
    let mut expected = None;
    let mut full_counts = Vec::new();
    for (name, budget) in [
        ("Step1", None),
        ("forward", Some(64 * 1024 * 1024)),
        ("partial", Some(450)),
    ] {
        let (mut drive, _, log) =
            source_with_command_log(&vector, &json!({"unreadable_records": []}));
        let io = drive.begin_verification_io();
        let mut raw = counter(
            DriveHandleRawSource::new(&mut drive),
            log,
            &vector,
            budget.is_some(),
        );
        let (actual, payload, total) = match budget {
            None => (
                verify_protected_step1_for_test(
                    &mut raw,
                    &map,
                    &vector.written.inputs.scheme,
                    &vector.written.inputs.tape_uuid,
                    BLOCK,
                )
                .unwrap(),
                0,
                0,
            ),
            Some(budget) => verify_protected_budget_for_test(
                &mut raw,
                &map,
                &vector.written.inputs.scheme,
                &vector.written.inputs.tape_uuid,
                BLOCK,
                budget,
            )
            .unwrap(),
        };
        assert!(actual.is_clean(), "{actual:?}");
        assert_eq!(actual.sidecars.len(), 4);
        if let Some(expected) = &expected {
            assert_eq!(&actual, expected);
        } else {
            expected = Some(actual);
        }
        for &address in raw.data.keys() {
            assert_eq!(
                raw.reads.iter().filter(|(lba, _)| *lba == address).count(),
                1,
                "{name}: data {address} must be read exactly once"
            );
        }
        if name == "forward" {
            assert!(
                raw.late_data.is_empty(),
                "no data revisit: {:?}",
                raw.late_data
            );
        }
        if name == "partial" {
            assert!(!raw.late_data.is_empty());
            assert!(payload > 0);
            assert!(total <= budget.unwrap());
        }
        let report = io.finish();
        assert_eq!(
            report.total.single_read,
            if name == "Step1" { 24 } else { 12 }
        );
        assert_eq!(report.total.batch_read, 18);
        let records = if name == "Step1" { 286 } else { 274 };
        assert_eq!(report.total.records_requested, records);
        assert_eq!(report.total.records_confirmed, records);
        assert_eq!(report.total.other, if name == "Step1" { 8 } else { 10 });
        assert_eq!(
            report.total.bytes,
            records * u64::from(BLOCK)
                + report.total.read_position * 32
                + report.total.other / 2 * 34
        );
        assert_eq!(report.total.hardware_batch_fallbacks, 0);

        use remanence_library::verification_io::VerificationPhase as Vp;
        let positioning = |p: Vp| {
            let c = &report.phases[p as usize];
            [c.locate, c.space, c.read_position]
        };
        println!(
            "io {name}: total={:?}; phases={:?}; protected_bytes={} ranges={:?}; backward={}",
            report.total,
            report.phases,
            report.protected_bytes,
            report.protected_ranges,
            report.backward_data_seeks
        );
        assert_eq!(
            report.total.locate as usize,
            raw.log.borrow().iter().filter(|c| c[0] == 0x92).count()
        );
        assert_eq!(
            report.total.space as usize,
            raw.log
                .borrow()
                .iter()
                .filter(|c| c[0] == 0x11 || c[0] == 0x91)
                .count()
        );
        assert_eq!(
            report.total.read_position as usize,
            raw.log.borrow().iter().filter(|c| c[0] == 0x34).count()
        );
        if name == "forward" {
            assert_eq!(report.backward_data_seeks, 0);
            assert_eq!(
                report.protected_bytes,
                raw.data.len() as u64 * u64::from(BLOCK)
            );
            for &address in raw.data.keys() {
                assert_eq!(
                    report
                        .protected_ranges
                        .iter()
                        .filter(|r| r.start_lba <= address && address < r.end_lba)
                        .count(),
                    1,
                    "protected address {address}"
                );
            }
            assert_eq!(positioning(Vp::ProtectedData), [3, 0, 3]);
            assert_eq!(positioning(Vp::ParityMap), [3, 0, 4]);
            assert_eq!(positioning(Vp::FallbackData), [0, 0, 0]);
            assert_eq!(report.total.algorithmic_fallback_epochs, 0);
        } else {
            assert!(report.total.algorithmic_fallback_epochs > 0);
            assert!(report.phases[Vp::FallbackData as usize].records_confirmed > 0);
        }
        let counts = raw.counts();
        println!("{name}: [LOCATE, SPACE, READ_POSITION] setup={:?} sweep={:?} sidecar-internal={:?} fallback={:?}; payload_high_water={payload} total_high_water={total}", counts[0], counts[1], counts[2], counts[3]);
        full_counts.push(counts);
    }
    assert_eq!(
        full_counts,
        vec![
            [[0, 0, 0], [1, 0, 1], [29, 0, 29], [5, 0, 5]],
            [[2, 0, 3], [4, 0, 4], [4, 0, 4], [0, 0, 0]],
            [[2, 0, 3], [2, 0, 2], [4, 0, 4], [4, 0, 4]],
        ]
    );
    assert_eq!(full_counts[1][Phase::Fallback as usize], [0; 3]);
    assert!(full_counts[0][Phase::Fallback as usize][0] > 0);
    assert!(full_counts[2][Phase::Fallback as usize][0] > 0);
}
