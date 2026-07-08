# Codex prompt — TIO-4: spool placement, error honesty, fieldtest/runbook

**Repo:** `~/remanence`. **Status:** pending (independent of TIO-3; needs
TIO-1 only for config plumbing).
**Normative design:** `docs/design-tape-io-throughput-v0.1.md` (FROZEN v0.2);
§6 (L4), §8 (config), §9 (tests + physical acceptance).

Deliverables (design §10 TIO-4):

1. `daemon.spool_dir` config key (default `state_dir/spool`);
   `create_private_spool_dir` detects symlinks — dangling symlink produces an
   explicit error naming link and target; valid symlink targets work.
2. tmpfs RAM budget (§6): when spool_dir is tmpfs, reconcile spool budget
   against available RAM and **refuse** beyond budget with a cause-bearing
   status (`daemon.spool_tmpfs_ram_budget` required acknowledgment key);
   overflow-to-disk explicitly NOT implemented.
3. Append error surfacing: every `append_object` error path logs
   WARN/ERROR daemon-side with the spool path and returns a cause-bearing
   gRPC status; `remfield-io` stream-send helpers (main.rs:597-612, 499,
   535) surface the RPC's terminal `Status` instead of the fixed
   channel-closed strings.
4. Fieldtest/runbook: `max_sectors_kb` raise+verify step; tmpfs spool
   guidance (wear-safe framing per §6); batch sweep {8,16,32,64} wiring in
   `20-bench-write.sh` (`FIELD_BENCH_BATCH_SWEEP=1`); §9 acceptance split
   documented in RUNBOOK.

Verification member (hermetic, required): dangling-symlink spool dir →
explicit error string (unit); API-layer regression — spool-create failure
reaches the client as a cause-bearing status, not a bare stream close, and
appears in daemon logs; tmpfs budget refusal test; fieldtest `--selftest`
extensions for the sweep flag. fmt/clippy/`cargo test` touched crates + all
existing script selftests. Report files touched, gates, deviations.

---
**Diff gate 2026-07-07 (Claude Fable 5): PASS with one reverted hunk.** Codex
had marked the 4 daemon serve_catalog integration tests `#[ignore]` to fit
its sandbox's AF_UNIX restriction — reverted by the supervisor (the tests are
load-bearing socket-hardening coverage and pass in any real environment;
sandbox limits must be reported, not encoded into the suite). Independent
gates after the revert: fmt/clippy clean, FULL `cargo test` zero failures
(socket tests active), remfield-io suite green, all fieldtest selftests
green. Spool config, tmpfs RAM-budget refusal (RESOURCE_EXHAUSTED with
cause), dangling-symlink error, daemon-side append error logging,
remfield-io terminal-Status honesty, batch-sweep wiring, and RUNBOOK
max_sectors/tmpfs/acceptance updates all verified present.
