# parity-capacity formal specification

Target: the terminal-close and compact decision kernels in
`verif/parity-capacity/src/lib.rs`, extracted from the scalar arithmetic in
`crates/remanence-parity/src/capacity.rs`.

The extraction contains only the current terminal-triple capacity model.
ParityMap arithmetic is used only for the single final pre-A parity-closeout
file, never as terminal inventory authority.

## C1 — exact replica and separation geometry

For structural count `S:u64`, Object-row count `R:u64`, and supported block
size `B`, require `R <= S` and a nonzero structural prefix containing the
BOT Bootstrap.

```text
payload_bytes          = 64*S + 256*R
payload_records        = ceil(payload_bytes/B)
replica_records        = payload_records + 2
replica_charge         = replica_records + replica_filemark_charge
triple_replica_charge  = 3*replica_charge
```

The added two records are exactly one header and one replica-local footer. The
replica payload is streamed fixed-slot data; no one-block row ceiling is part of
this model.

For nominal separation bytes `G = 1 GiB`:

```text
gap_records       = ceil(G/B)
gap_charge        = gap_records + gap_filemark_charge
double_gap_charge = 2*gap_charge
```

The supported 256 KiB, 512 KiB, and 1 MiB record sizes yield respectively
4096, 2048, and 1024 records per gap. Header and footer are included in that
count. A different nominal extent, an unsupported block size, `R > S`, or
checked overflow fails closed.

## C2 — exact close reserve

Object admission and manual finalization use the same close kernel:

```text
parity_closeout = final_partial_sidecar_charge + final_parity_map_charge
terminal_tail   = triple_replica_charge + double_gap_charge
close_bound     = parity_closeout + terminal_tail + safety_allowance
required_tape   = prefix_commit_charge + close_bound
```

The input carries the selected cartridge/profile capacity basis `C`, pool
watermarks `L/H`, and barrier-proved physical remainder. It rejects
`remaining > C` and any policy other than `L < H <= C`. The worst legal close
must fit `C-H` before the first Object can use the profile.

Each term is disjoint. No record, footer, or filemark appears twice. Spool
reserve includes only encoded pending sidecar records and therefore excludes
tape-only filemarks, final ParityMap, gaps, replicas, and safety.

The final partial-sidecar charge uses the actual projected epoch remainder for
its data-CRC index rows. Its parity shard count remains the scheme-fixed
`stripes_per_neighborhood * parity_blocks_per_stripe`; it is not charged as a
full data-index layout.

Projection distinguishes automatic Object admission from manual close:

- automatic admission adds the Object file/row and sidecars completed by it;
- manual close adds no Object file, Object row, or Object filemark charge;
- the no-parity profile requires zero epoch/sidecar/spool state and sets every
  sidecar, parity-closeout, and spool term to zero while preserving A/B/C and
  both gaps;
- both close a nonempty partial epoch when present, fix exact post-closeout
  structural/Object counts, and preserve the complete terminal tail;
- both validate the worst-close profile and current tape/spool availability;
- equality at a tape or spool bound succeeds;
- both report the same current-tape shortage before spool shortage; pool
  orchestration classifies fresh-media impossibility from that same exact
  result instead of invoking a second capacity model.

Every nonempty sidecar directory emits exactly one external final ParityMap.
It belongs to pre-A parity closeout and consumes one structural slot in all
three replicas; it is not one of the five terminal-tail components. The
no-parity branch is currently tied to production by the compiled Rust
behavior check; it is extracted into Lean but does not yet carry a dedicated
success theorem. The Lean theorems prove the checked scalar formulas, exact multiplicities
`3` and `2`, fail-closed arithmetic, profile guards, projection identities,
and branch ordering over the extracted functions.

## C3 — irreversible five-component progress

The proof-facing progress type has exactly six states:

```text
BeforeReplicaA
AfterReplicaA
AfterSeparationAb
AfterReplicaB
AfterSeparationBc
AfterReplicaC
```

- successful advancement moves exactly one component and never skips;
- a failed or completion-unknown barrier leaves progress unchanged;
- completed replica projection is `0,1,1,2,2,3` and never decreases;
- Object admission is false throughout Finalizing;
- ordinary sealed/finalized projection is allowed only at `AfterReplicaC`.

These theorems establish the scalar progress and admission predicates used by
the lifecycle. They do not prove persistence of the caller's
`Open -> Finalizing -> Finalized/RecoveryRequired` state or independently prove
that orchestration never clears the finalizing flag; that durable transition is
outside this proof boundary.

## C4 — terminal replica selection

The compact selection kernel has five outcomes:

```text
ReplicaC | ReplicaB | ReplicaA | FullBotScan | Conflict
```

With agreeing common edition facts, selection prefers C, then B, then A. Any
two or more valid survivors whose common facts disagree yield `Conflict`.
No valid survivor yields `FullBotScan`, never an empty-success inventory.

## Boundaries

The proofs do not cover:

- deterministic CBOR encoding/decoding or fixed-slot bytes;
- HMAC-SHA-256, SHA-256, or CRC-64/XZ algebra;
- replica/separation header and footer buffer construction;
- device positioning, filemarks, EOD, persistence barriers, or media damage;
- journal fsync/SQLite ordering or restart orchestration;
- byte-level survivor-agreement fields or the physical BOT recovery walk;
- the standard library internals used by production collections and IO.

Drift guards and compiled-production behavior matrices connect selected scalar
inputs/results to production. They are change detectors, not semantic
equivalence proofs.

## Trust anchor

`lake build` type-checks the local Lean theorems. The proof inventory also
regenerates Aeneas output and compares it byte-for-byte with the checked-in
definitions. External Aeneas library warnings are outside the local proof
files; local maintained proof files must contain no placeholders.
