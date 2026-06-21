# Codex Prompt — Chaos Phase F (rem CLI runtime hook)

Implement the **rem side** of Phase F: route `rem-debug`'s SCSI transport through
`ChaosTransport` over the real `LinuxSgTransport` when chaos is enabled, via the
existing public factory seam. The harness side is a separate prompt in
`~/system/docs/prompt-chaos-phase-f.md`.

## Source of truth
- **Design (read fully): `~/system/docs/design-chaos-harness-l2.md`** §3 — wins
  on conflict.
- Parent: `docs/chaos-adapter-design.md` (Phase F, Guardrails).
- Existing primitives: `remanence-chaos` `maybe_wrap_from_env` +
  `chaos_enabled_from_env` + `DeviceCtx`; the public `Library::open_with(policy,
  factory)` (`handle/mod.rs:1695`) and `discover_with(devices, factory)`
  (`discovery.rs`). `LibraryHandle::open_drive` reuses the stored factory.

## Mechanism (decided): wrap at the CLI layer — no library change, no back-dep
The rem production opens use bare `Library::open` / `discover()` (hardcoded
`LinuxSgTransport`). Switch **only the `rem-debug` CLI's** open/discover sites to
the public `open_with` / `discover_with` with a chaos-gated factory.
`open_drive` inherits chaos via the stored factory — no change there.
`remanence-cli` may depend on `remanence-chaos` directly (no cycle). Do **not**
touch `remanence-library`, `remanence-api`, or the daemon.

## Deliverables
1. Add `remanence-chaos` to `remanence-cli`'s `[dependencies]`.
2. A CLI-local transport-factory helper:
   `move |path| { let inner = LinuxSgTransport::open_rw(path).map_err(IoErrorKind::from)?;
   if chaos_real_enabled() { wrap inner in ChaosTransport via maybe_wrap_from_env
   (DeviceCtx::new().with_backend("linux").with_drive_id(<bay/sg id>)) } else {
   Ok(Box::new(inner) as Box<dyn SgTransport>) } }`.
   - `chaos_real_enabled()` = `REM_CHAOS_ENABLED` truthy **AND**
     `REM_CHAOS_ALLOW_REAL` truthy. The `REM_CHAOS_ALLOW_REAL` gate is the
     guardrail so chaos over real hardware never engages by accident. (Add a
     `remanence_chaos::chaos_real_enabled_from_env()` helper if cleaner.)
   - Map `ChaosError` → a clear CLI error at factory-build time (e.g.
     `REM_CHAOS_STATE` unreadable); do not smuggle it through the closure's
     `IoErrorKind` error type.
   - **DeviceCtx is minimal for L2** (backend `linux`, `drive_id` from the sg
     path / bay). Drive-scoped MED-05 is the marquee; tape-scoped targeting
     (loaded barcode) is a noted follow-up.
3. Switch the CLI's open/discover call sites to `open_with`/`discover_with` with
   this factory (the sites: `crates/remanence-cli/src/pool_ops.rs` ~179/779/1185;
   `crates/remanence-cli/src/lib.rs` ~4110/4583/7515/7925 for opens, ~221/636/8088
   for discover). `open_drive` sites need no change.
4. A test that, with the env gates **unset**, the factory returns the bare
   `LinuxSgTransport` (production byte-identical); with both set on a non-Linux /
   no-device build, the gating logic is exercised without touching hardware
   (gate the device-touching part `#[cfg(target_os = "linux")]`).

## Constraints
- **No `remanence-library` / `remanence-api` / daemon change.** CLI only.
- Production behavior **byte-identical** when `REM_CHAOS_ENABLED` /
  `REM_CHAOS_ALLOW_REAL` are unset.
- `cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D warnings`
  clean (note `--all-targets` — Phase E's lint slipped through without it);
  `cargo build --release` (harness freshness guard). Doc new `pub` items.
- Commit per `AGENTS.md` (journal + report).

## Acceptance (design §10)
- `rem-debug` routes through `ChaosTransport<LinuxSgTransport>` iff both env gates
  set; bare transport otherwise (test proves the off path is unchanged). No
  library/api/daemon change. Gates green (paste counts). Report what the hook
  enables and the deferred daemon-path wrapping.
