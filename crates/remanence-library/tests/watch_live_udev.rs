//! Layer 2c §7.8 — live-udev integration test.
//!
//! `#[ignore]`-gated by default. Runs only when:
//! - Built with `--features linux-udev` (requires `pkg-config` +
//!   `libudev-dev` system packages).
//! - On a Linux host with udev running.
//! - As **root** (writing to `/sys/.../uevent` triggers a synthetic
//!   netlink event; non-root cannot poke it).
//! - At least one `/sys/class/scsi_generic/sg*` device is present.
//!
//! Invocation:
//!
//! ```text
//! REM_TEST_LIVE_SCSI=1 cargo test --features linux-udev --test watch_live_udev -- \
//!   --ignored --test-threads=1 --nocapture
//! ```
//!
//! Missing prerequisites fail when this ignored test is selected.

#![cfg(all(target_os = "linux", feature = "linux-udev"))]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use remanence_library::watch::{HotplugSource, LinuxUdevSource};

fn first_sg_device() -> Option<PathBuf> {
    let sys_dir = PathBuf::from("/sys/class/scsi_generic");
    let entries = std::fs::read_dir(&sys_dir).ok()?;
    for e in entries.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("sg") && name[2..].chars().all(|c| c.is_ascii_digit()) {
            return Some(sys_dir.join(name.as_ref()));
        }
    }
    None
}

fn poke_uevent(sg_path: &Path) -> std::io::Result<()> {
    use std::io::Write;
    let uevent = sg_path.join("uevent");
    let mut f = std::fs::OpenOptions::new().write(true).open(&uevent)?;
    // The udev contract: writing one of `add`, `change`, `remove`,
    // `online`, `offline` to `uevent` synthesises a netlink event.
    f.write_all(b"change\n")?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires root + Linux + a real /dev/sg* device"]
async fn live_udev_delivers_change_event() {
    assert_eq!(std::env::var("REM_TEST_LIVE_SCSI").as_deref(), Ok("1"),
        "run with REM_TEST_LIVE_SCSI=1 cargo test -p remanence-library --features linux-udev --test watch_live_udev -- --ignored");
    let sg = first_sg_device().expect("a /sys/class/scsi_generic/sg* device is required");
    let mut source = LinuxUdevSource::new().expect("live udev must be available");
    // Tighten the coalescer so the test doesn't have to wait a full
    // 500ms after the poke before the burst materialises.
    source.set_coalesce_window(Duration::from_millis(50));
    let mut rx = source.subscribe().expect("subscribe");

    // Poke after subscribing, so the event isn't missed in the
    // millisecond between MonitorBuilder::listen() and our first
    // recv().
    poke_uevent(&sg).expect("must be able to write the device uevent (requires root)");

    // The monitor catches every SCSI hot-plug event on the host, not
    // just ours, so an unrelated burst could plausibly arrive first.
    // Keep reading until the deadline; succeed as soon as any burst
    // touches our sg path; only fail if the deadline elapses with no
    // match. Codex review d38cc769 flagged the fail-on-first variant
    // as fragile on busy hosts.
    let sg_name = sg.file_name().unwrap().to_string_lossy().to_string();
    let sg_canonical = std::fs::canonicalize(&sg).ok();
    let burst_matches_target = |burst: &remanence_library::watch::Coalesced| -> bool {
        burst.touched_paths.iter().any(|p| {
            // Match by basename equality (avoids `sg4` matching `sg40`).
            let basename_match = p
                .file_name()
                .map(|n| n.to_string_lossy() == sg_name.as_str())
                .unwrap_or(false);
            // Match against canonical sg sysfs path or its parent
            // scsi_device path (kernels sometimes report the parent).
            let canonical_match = sg_canonical
                .as_ref()
                .map(|canon| {
                    p.starts_with(canon)
                        || canon.starts_with(p)
                        || p == &PathBuf::from("/dev").join(&sg_name)
                })
                .unwrap_or(false);
            basename_match || canonical_match
        })
    };

    let deadline = Instant::now() + Duration::from_secs(5);
    let timeout = tokio::time::sleep_until(deadline.into());
    tokio::pin!(timeout);

    loop {
        tokio::select! {
            biased;
            _ = &mut timeout => panic!(
                "no matching burst arrived within 5s for {}; expected a \
                 burst whose touched_paths mention that device",
                sg.display()
            ),
            item = rx.recv() => match item {
                Some(Ok(burst)) => {
                    eprintln!(
                        "got burst: {} events, subsystems={:?}, kinds={:?}, paths={}",
                        burst.raw_event_count,
                        burst.subsystems,
                        burst.kinds,
                        burst.touched_paths.len()
                    );
                    if burst_matches_target(&burst) {
                        return;
                    }
                    eprintln!("  …burst didn't mention {sg_name}; continuing");
                }
                Some(Err(e)) => panic!("watcher error: {e}"),
                None => panic!("receiver closed before any burst"),
            },
        }
    }
}
