# parity-state formal specification (Leanstral pilot)

Target: `verif/parity-state/src/lib.rs` (verbatim extraction of
`remanence-parity::model::{ObjectParityState, ObjectParityStateUpdateRange}`),
translated to Lean 4 via Charon → Aeneas. Theorem names below are stable; the
exact Lean statements get instantiated against Aeneas' generated definitions
(`Result` monad, machine-scalar `U64` with `.val` projections) once translation
lands.

Notation: `s` = `first_parity_data_ordinal`, `c` = `data_block_count`,
`W` = `highest_protected_ordinal`; all `u64`. The object's half-open ordinal
range is `[s, s+c)`.

## T1 — classification correctness (`from_ordinals_spec`)

For `c > 0` and `s + c ≤ u64::MAX` (no overflow):
`from_ordinals s c W = ok st` where

- `st = Protected ↔ s + c ≤ W`
- `st = Pending   ↔ s ≥ W`
- `st = Partial   ↔ s < W < s + c`

and the three cases are exhaustive and mutually exclusive (given `c > 0`).
This is `docs/layer3c-design.md` §7.2.1 / §10.1 verbatim.

## T2 — error completeness (`from_ordinals_err_iff`)

`from_ordinals s c W` fails **iff** `c = 0 ∨ s + c > u64::MAX`.
(No silent wrap, no panic path: total function over the full input space.)

## T3 — watermark-advance predicate safety (`includes_object_safe`) — the crown theorem

For any range built by `from_watermark_advance old new = ok (some r)`
(so `old < new`), and any object with `c > 0`, no overflow:

`r.includes_object s c = ok false → from_ordinals s c old = from_ordinals s c new`

i.e. every object the recomputation predicate *skips* provably has an unchanged
parity state across the watermark advance. This mechanizes the doc-comment
claim "the predicate … never misses an object whose summary state can change"
and is exactly what the Layer-5 catalog transaction relies on for correctness.

## T4 — advance monotonicity (`state_monotone_in_watermark`)

For `old ≤ new`, `c > 0`, no overflow: the state ordering
`Pending < Partial < Protected` is monotone in the watermark —

- `from_ordinals s c old = ok Protected → from_ordinals s c new = ok Protected`
- `from_ordinals s c new = ok Pending  → from_ordinals s c old = ok Pending`

(Protection never regresses under a monotonic advance; `from_watermark_advance`
rejecting `new < old` is what makes this a system-level invariant.)

## T5 — recompute consistency (`recompute_object_sound`)

`r.recompute_object s c = ok (some st) → from_ordinals s c r.new = ok st`
and `r.recompute_object s c = ok none → r.includes_object s c = ok false`.
(The convenience wrapper agrees with the primitive definitions.)

## Trust anchor

The Lean type checker (`lake build` with zero `sorry`) is the trust anchor.
Leanstral only *searches* for proofs; nothing it emits is trusted until the
checker accepts it. The `drift_guard` cargo test ties the verified extraction
back to the production source; if it fires, proofs must be re-established.
