# tape-init formal specification

Target: `crates/remanence-api/src/tape_init.rs`, specifically the pure
`decide_tape_init` safety decision core.

The verification extraction mirrors the production branch ordering and replaces
payloads that do not affect control flow with compact proof-facing values:
UUIDs are equality-only integers, pool strings are categorized as current pool,
foreign pool, or unknown pool, and error payloads are represented by stable
reason variants. The Rust `drift_guard` test ties those abstractions back to the
production source snippets.

## T1 -- committed pool conflicts dominate

Committed copies in a foreign or unknown pool are always an anomaly before BOT,
barcode, catalog-row, geometry, or physical-data facts are considered.

## T2 -- unreadable or foreign BOT refuses

With no committed pool conflict:

- BOT read errors refuse with `BotReadError`
- known foreign formats refuse with `ForeignFormat`
- readable unrecognized data refuses with `UnrecognizedData`

## T3 -- blank BOT rules

With no committed pool conflict:

- an available barcode and no committed copies yields `FreshInit`
- an assigned barcode is an anomaly
- a retired barcode is an anomaly
- after the barcode passes those checks, committed copies in the current pool
  refuse with `CommittedCopiesPresent`

## T4 -- clean Remanence bootstrap no-ops

With no committed pool conflict, a Remanence BOT UUID whose barcode is available
or assigned to the same UUID, with a matching active unwritten catalog row, no
physical data past bootstrap, no committed copies, and matching geometry, yields
`IdempotentNoOp`.

## T5 -- Remanence bootstrap hazards are ordered

For a Remanence BOT with no committed pool conflict, the guarded outcomes occur
in production order:

- barcode assigned elsewhere or retired is an anomaly
- missing catalog row needs explicit rebuild
- catalog UUID mismatch is a media-swap anomaly
- retired catalog row yields `FreshInit`
- barcode mismatch without relabel is an anomaly
- physical data past bootstrap refuses clobber
- committed copies in the current pool refuse clobber
- geometry mismatch requires scoped force
- catalog-written row refuses clobber

## Trust anchor

The Lean type checker (`lake build` with zero local placeholders) is the trust
anchor. The Rust `drift_guard` test ties this extraction back to
`crates/remanence-api/src/tape_init.rs`; if it fires, the extraction and proofs
must be re-established.
