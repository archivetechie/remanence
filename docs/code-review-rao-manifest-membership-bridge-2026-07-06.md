# Code Review - RAO manifest membership bridge proof

Review target: commit `18f9b5f` (`verif: bridge RAO planner membership facts`).

Reviewer: Codex diff-gate pass, 2026-07-06.

## Findings

No Critical, High, Medium, or Low findings.

## Review Notes

The proof claim matches the implemented extraction. The new bridge validates
source-entry membership facts before constructing the planner entry consumed by
the prior planner-fold theorem.

The scalar set contract is coherent:

- path and file-id insert facts must match the `BTreeSet::insert` contract
- regular entries require a fresh regular-path insert
- non-regular entries cannot insert into the regular-path set
- the constructed planner entry carries source ids and membership booleans
  through unchanged
- the arbitrary-list theorem composes those facts into the existing planner
  fold theorem

The docs stay inside the proved scope. They claim a semantic
`BTreeSet::contains`/`insert` contract over proof-facing scalar facts, not a
proof of Rust's standard-library `BTreeSet`, `String`, `Vec`, CBOR parsing, or
byte-level RAO encode/decode.

## Verification Reviewed

- `verif/rao-manifest cargo test`: membership bridge and drift-guard tests
  included in the 16-test proof crate run.
- `verif/rao-manifest/lean lake build`: accepted
  `planner_membership_fold_core_success_arbitrary`; only external Aeneas
  Slice/StringIter placeholder warnings.
- Repo gates from the implementation commit: `cargo test`, `cargo fmt --check`,
  `cargo clippy -- -D warnings`, and `cargo build --release` were green.
