# Development tools

`rem-dev` runs review-vector generation and scratch-tape drills. Release
archives and the field kit exclude it. Build and check vectors with:

```sh
cargo build -p remanence-cli --bin rem-dev
cargo run -p remanence-cli --bin rem-dev -- generate-tape-images --check
```

`generate-tape-images` accepts the former example's arguments unchanged:
`--check`, `--negative-cases PATH`, `--negative-supplement PATH`,
`--export-images DIRECTORY` and `--export-objects DIRECTORY`. The old
`generate_tape_images` example prints the replacement command and fails.
Frozen archive text retains historical command names to preserve its bytes.

The tape commands retain their arguments, acknowledgements, fault-injection
gates, report schemas and output. The raw-dump helper requires the same
`--allow SERIAL` and optional `--allow-derived SERIAL` globals.

| Command | What it does |
|---|---|
| `rem-dev tape terminal-index-drill --device <SG> --tape-uuid <UUID> --block-size <BYTES> --report <PATH> [--damage-plan <PLAN>] [--reconcile-outcome <OUTCOME>] [--full-verify]` | Run one read-only live-SG terminal-index verification leg and write schema `rem.tape.terminal-index-drill.v1`. `--damage-plan` defaults to `none`. `--reconcile-outcome` forces one no-write reconciliation refusal: `torn-worm` makes the next component readable at its proved start only as a torn WORM tail, and `unproved-start` makes the next component's immutable start unprovable. Replica plans are `none`, `a`, `b`, `c`, `ab`, `ac`, `bc`, `abc`, and `disagreement`; the latter substitutes a locally valid conflicting A edition and requires the fail-closed typed `TerminalIndexReplicaConflict` refusal with no selected replica, while `abc` executes BOT structural recovery. Gap plans are `gap-ab-header`, `gap-ab-footer`, `gap-bc-header`, and `gap-bc-footer`; each replaces the named record with a fixed `0xd7` block above the transport, requires `--full-verify`, and reports typed degraded separation evidence. Reports name the mechanism and exact injected LBAs. All injection occurs in the reader above the transport and never rewrites media. Full verification otherwise requires `--damage-plan none`. In the report's `full_verify` object, `outcome` names what the terminal suffix did and `complete` names what the tape did: `outcome: "verified_complete"` with `complete: false` means that the terminal suffix validated and the protected content did not, and `complete` is `true` only when both hold. See [Full verification](guide-full-verification.md). |
| `rem-dev dev write-dump-to-tape --dump <PATH> --tape <SERIAL> --bay <BAY> --i-understand-this-overwrites-the-loaded-tape [--record-size <BYTES>]` | Overwrite the loaded scratch tape with raw dump bytes (test fixture setup). `--tape` takes the serial of the library that holds the tape. `--record-size` sets the variable record size (default 1048576). |
| `rem-dev tape freeze-drill --device </dev/sgN> --block-size <BYTES> --damage-plan <PLAN> --report <PATH.json> [--data-mib <MIB>] [--default-index-separation] [--yes-i-know-scratch]` | Run one destructive REM-PARITY freeze criterion 4 fault-injected scratch-tape round-trip: write drill objects through a full terminal-index finalization, then inject a chosen read-side medium-error damage plan and verify recovery. `--damage-plan` is one of `bootstrap-copy0`, `sidecar-head`, `object-span`, `terminal-replica-c`, or `combined` (the maximal conforming set). `--yes-i-know-scratch` permits overwriting the tape when BOT cannot be read; `--default-index-separation` uses the production 1 GiB separation extent instead of the compact drill profile. `--block-size` is 262144, 524288 or 1048576; `--data-mib` defaults to 512. Writes a stable JSON report to `--report`. The drill does not consult `--allow`; the explicit `--device` is its only target selection. |

`cargo test -p remanence-cli --bin rem-dev` runs the development tests.
Operator inventory and API recovery tests also compile the fixture adapters
under `cfg(test)`; shipping library builds exclude them.

The existing `make rem-dev` target rebuilds the operator binaries and refreshes
the capability on `rem`. Its name predates this binary; use the Cargo command
above to build the development tools.
