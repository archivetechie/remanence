//! End-to-end kernel readback, retry and heap bounds using a generated reader.
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use remanence_parity::closeout::{
    rebuild_uncovered, CheckValue, CheckValueBuilder, CloseoutError, CommittedExtent,
    CommittedReader, EpochAccumulator, UncoveredRange,
};
use remanence_parity::{data_shard_crc64, default_scheme, ParityScheme};

thread_local! {
    static HEAP: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
}
struct MeasuredAllocator;
// Only the calling test thread is measured. No retained data or worker thread
// participates in this generated reader, so its high-water mark is reproducible.
unsafe impl GlobalAlloc for MeasuredAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            let _ = HEAP.try_with(|state| {
                let (live, peak) = state.get();
                let live = live + layout.size();
                state.set((live, peak.max(live)));
            });
        }
        pointer
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        let _ = HEAP.try_with(|state| {
            let (live, peak) = state.get();
            state.set((live.saturating_sub(layout.size()), peak));
        });
        unsafe { System.dealloc(pointer, layout) };
    }
}
#[global_allocator]
static ALLOCATOR: MeasuredAllocator = MeasuredAllocator;

fn geometry() -> ParityScheme {
    ParityScheme {
        stripes_per_neighborhood: 16,
        ..default_scheme()
    }
}
fn fill(ordinal: u64, block: &mut [u8]) {
    for (i, byte) in block.iter_mut().enumerate() {
        *byte = ((ordinal * 31 + i as u64) % 251) as u8;
    }
}
fn range(count: u64, block_size: u32) -> UncoveredRange {
    let mut check = CheckValueBuilder::new();
    let mut block = vec![0; block_size as usize];
    for ordinal in 23..23 + count {
        fill(ordinal, &mut block);
        check.push(data_shard_crc64(&block));
    }
    UncoveredRange {
        epoch_id: 7,
        w: 23,
        t: 23 + count,
        check: check.value(),
        extents: vec![
            CommittedExtent {
                tape_file_number: 3,
                first_ordinal: 0,
                block_count: 25,
                physical_start_lba: 10,
            },
            CommittedExtent {
                tape_file_number: 5,
                first_ordinal: 25,
                block_count: 3,
                physical_start_lba: 100,
            },
            CommittedExtent {
                tape_file_number: 8,
                first_ordinal: 28,
                block_count: count - 5,
                physical_start_lba: 1000,
            },
        ],
    }
}
struct GeneratedReader<'a> {
    range: &'a UncoveredRange,
    ordinal: u64,
    reads: u64,
    locates: u64,
    proofs: u64,
    corrupt_passes: u64,
    heap_baseline: usize,
    measure_drop: bool,
}
impl<'a> GeneratedReader<'a> {
    fn new(range: &'a UncoveredRange, corrupt_passes: u64) -> Self {
        Self {
            range,
            ordinal: 0,
            reads: 0,
            locates: 0,
            proofs: 0,
            corrupt_passes,
            heap_baseline: 0,
            measure_drop: false,
        }
    }
}
impl CommittedReader for GeneratedReader<'_> {
    fn reprove_identity(&mut self) -> Result<(), CloseoutError> {
        if self.measure_drop {
            // The first parity allocation must be gone before reproof, not
            // merely overwritten after allocating another complete payload.
            let live = HEAP.with(|s| s.get().0);
            assert!(
                live < self.heap_baseline + 4096,
                "first parity payload still live: {live}"
            );
        }
        self.proofs += 1;
        Ok(())
    }
    fn locate(&mut self, partition: u32, lba: u64) -> Result<(), CloseoutError> {
        assert_eq!(partition, 0);
        self.locates += 1;
        let extent = self
            .range
            .extents
            .iter()
            .find(|e| lba >= e.physical_start_lba && lba - e.physical_start_lba < e.block_count)
            .expect("committed coordinate");
        self.ordinal = extent.first_ordinal + lba - extent.physical_start_lba;
        Ok(())
    }
    fn read_block(&mut self, block: &mut [u8]) -> Result<(), CloseoutError> {
        fill(self.ordinal, block);
        if self.proofs < self.corrupt_passes && self.ordinal == self.range.w {
            block[0] ^= 1;
        }
        self.ordinal += 1;
        self.reads += 1;
        Ok(())
    }
}

#[test]
fn live_and_rebuilt_sidecars_match_for_short_and_full_nonzero_ranges() {
    for count in [11, 2048] {
        let range = range(count, 256);
        let mut live =
            EpochAccumulator::new([7; 16], &geometry(), 256, range.epoch_id, range.w).unwrap();
        let mut block = [0; 256];
        for ordinal in range.w..range.t {
            fill(ordinal, &mut block);
            live.push_block(ordinal, &block).unwrap();
        }
        assert_eq!(live.check_value(), range.check);
        let expected = live.into_sidecar().unwrap();
        let mut reader = GeneratedReader::new(&range, 0);
        let rebuilt = rebuild_uncovered([7; 16], &mut reader, &geometry(), 256, 0, &range).unwrap();
        assert_eq!(reader.reads, count);
        assert_eq!(reader.locates, 3);
        assert_eq!(reader.proofs, 0);
        assert!(rebuilt.into_sidecar().unwrap() == expected);
    }
}

#[test]
fn reread_drops_first_buffers_and_heap_never_retains_data() {
    const BLOCK: u32 = 8192;
    let scheme = geometry();
    let range = range(2048, BLOCK);
    for corrupt_passes in [1, 2] {
        let mut reader = GeneratedReader::new(&range, corrupt_passes);
        reader.heap_baseline = HEAP.with(|s| {
            let live = s.get().0;
            s.set((live, live));
            live
        });
        reader.measure_drop = true;
        let result = rebuild_uncovered([7; 16], &mut reader, &scheme, BLOCK, 0, &range);
        let peak = HEAP.with(|s| s.get().1) - reader.heap_baseline;
        let core = EpochAccumulator::reservation_bytes(&scheme, BLOCK).unwrap() as usize;
        assert!(peak < core + 64 * 1024, "peak {peak}, core {core}");
        assert!(peak < 2048 * BLOCK as usize / 8, "retained data payload");
        assert_eq!(reader.reads, 4096);
        assert_eq!(reader.proofs, 1);
        assert_eq!(reader.locates, 6);
        if corrupt_passes == 1 {
            assert_eq!(result.unwrap().check_value(), range.check);
        } else {
            assert!(
                matches!(result, Err(CloseoutError::CheckMismatch { expected, .. }) if expected == range.check)
            );
        }
    }
}

#[test]
fn invalid_extent_coverage_is_refused_before_positioning() {
    for kind in 0..4 {
        let mut range = range(11, 256);
        match kind {
            0 => range.extents[1].first_ordinal += 1,
            1 => range.extents[1].first_ordinal -= 1,
            2 => range.extents[1].tape_file_number = 3,
            _ => range.extents[2].physical_start_lba = u64::MAX,
        }
        let mut reader = GeneratedReader::new(&range, 0);
        assert!(matches!(
            rebuild_uncovered([7; 16], &mut reader, &geometry(), 256, 0, &range),
            Err(CloseoutError::Parity(_))
        ));
        assert_eq!(reader.locates, 0);
        assert_eq!(reader.reads, 0);
    }
    assert_eq!(CheckValue::EMPTY.0, 0);
}

#[test]
fn reader_failures_preserve_their_class_and_do_not_retry() {
    struct FailedRead(Option<CloseoutError>);
    impl CommittedReader for FailedRead {
        fn locate(&mut self, _: u32, _: u64) -> Result<(), CloseoutError> {
            Ok(())
        }
        fn reprove_identity(&mut self) -> Result<(), CloseoutError> {
            panic!("boundary and transport failures must not trigger the check retry")
        }
        fn read_block(&mut self, _: &mut [u8]) -> Result<(), CloseoutError> {
            Err(self.0.take().expect("only one read"))
        }
    }
    let range = range(11, 256);
    for error in [
        CloseoutError::Boundary { lba: 33 },
        CloseoutError::TapeIo(remanence_library::TapeIoError::Transport(
            remanence_library::ScsiError::Truncated { got: 0, need: 256 },
        )),
    ] {
        let boundary = matches!(error, CloseoutError::Boundary { .. });
        let mut reader = FailedRead(Some(error));
        let result = rebuild_uncovered([7; 16], &mut reader, &geometry(), 256, 0, &range);
        if boundary {
            assert!(matches!(result, Err(CloseoutError::Boundary { lba: 33 })));
        } else {
            assert!(matches!(
                result,
                Err(CloseoutError::TapeIo(
                    remanence_library::TapeIoError::Transport(_)
                ))
            ));
        }
    }
}
