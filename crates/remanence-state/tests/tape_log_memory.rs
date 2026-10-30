//! Isolated allocator measurement of the complete encode/decode/fold pipeline.
//! This binary has one default test; the explicit 10M test is ignored by default.
//! All allocations (including producer, decoded records and parser) are counted.

#[path = "support/tape_log_stream.rs"]
mod stream;
use remanence_state::tape_log::{
    codec::{Decoder, Tail},
    fold::Fold,
};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::{
    atomic::{AtomicUsize, Ordering::SeqCst},
    Mutex,
};

struct CountingAllocator;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
// Also serialize if a developer explicitly requests --include-ignored.
static TEST_LOCK: Mutex<()> = Mutex::new(());
fn allocated(size: usize) {
    let live = LIVE.fetch_add(size, SeqCst) + size;
    PEAK.fetch_max(live, SeqCst);
}
// SAFETY: every operation delegates the unchanged pointer/layout to System;
// accounting uses nonallocating atomics and never touches the allocated memory.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller provides a valid allocation layout.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            allocated(layout.size());
        }
        p
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller provides a valid allocation layout.
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            allocated(layout.size());
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), SeqCst);
        // SAFETY: this pointer/layout pair was allocated by System.
        unsafe { System.dealloc(p, layout) };
    }
    unsafe fn realloc(&self, p: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: caller satisfies System's pointer, layout and new-size contract.
        let next = unsafe { System.realloc(p, layout, size) };
        if !next.is_null() {
            if size >= layout.size() {
                allocated(size - layout.size());
            } else {
                LIVE.fetch_sub(layout.size() - size, SeqCst);
            }
        }
        next
    }
}
#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn assert_bound(members: u64) {
    let _lock = TEST_LOCK.lock().unwrap();
    let initial = LIVE.load(SeqCst);
    PEAK.store(initial, SeqCst);
    let mut source = stream::MillionMemberStream::new(members);
    let mut decoder = Decoder::new(&mut source);
    let mut fold = Fold::new();
    let mut frames = 0;
    let mut baseline_peak = 0;
    // Less than one retained byte per post-baseline frame in the 1M run.
    const SLACK: usize = 2 * 1024;
    while let Some(frame) = decoder.next_frame().unwrap() {
        fold.push(&frame).unwrap();
        frames += 1;
        if frames == 1000 {
            baseline_peak = PEAK.swap(LIVE.load(SeqCst), SeqCst);
        } else if frames > 1000 {
            assert!(PEAK.load(SeqCst)<=baseline_peak+SLACK,"live heap grew after frame 1000: baseline={baseline_peak}, peak={}, frame={frames}",PEAK.load(SeqCst));
        }
    }
    let (summary, tail) = fold.finish(decoder.tail()).unwrap();
    assert_eq!(tail, Tail::Complete);
    assert_eq!(summary.object_count, 1);
    assert_eq!(summary.barrier_count, 1);
    assert!(frames > 4000);
    assert_eq!(source.emitted, members);
    let steady_peak = PEAK.load(SeqCst);
    let peak = baseline_peak.max(steady_peak);
    assert!(
        peak - initial < 1024 * 1024,
        "pipeline heap exceeded 1 MiB: {peak}"
    );
    assert!(steady_peak <= baseline_peak + SLACK);
    eprintln!("members={members}, frames={frames}, bytes={}, initial_live={initial}, frame_1000_peak={baseline_peak}, later_peak={steady_peak}, peak_live={peak}, slack={SLACK}",source.offset);
}
#[test]
fn one_million_members_have_flat_peak_live_heap() {
    assert_bound(1_000_000);
}
#[test]
#[ignore = "explicit 10M allocation-bound verification; the default suite runs 1M"]
fn ten_million_members_10m_peak_live_heap_bound() {
    assert_bound(10_000_000);
}
