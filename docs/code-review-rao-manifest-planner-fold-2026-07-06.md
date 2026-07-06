# Code Review - RAO manifest planner-fold bridge proof

Review target: commit `9df9d26` (`verif: bridge RAO manifest planner fold`).

Reviewer: Codex diff-gate pass, 2026-07-06.

## Findings

No Critical, High, Medium, or Low findings.

## Review Notes

The proof claim matches the implemented extraction: for any arbitrary Lean list
of valid generated planner steps, the extracted planner fold reaches a final
state while preserving accepted-entry and regular-prefix counter behavior.

The commit correctly avoids claiming a byte-level production proof. Its stated
boundary is the important one: the planner step consumed scalar membership
facts for duplicate path, duplicate file id, and hardlink-target prefix
membership. It did not prove `String`, `Vec`, or standard-library `BTreeSet`
internals.

That residual risk is narrowed by the follow-up membership bridge in this
change, which proves the scalar `BTreeSet::contains`/`insert` contract before
feeding those facts into the planner fold.

## Verification Reviewed

- `verif/rao-manifest cargo test`: planner-fold extraction tests covered the
  accepted/rejected generated-step cases.
- `verif/rao-manifest/lean lake build`: accepted the planner-fold theorem with
  only external Aeneas Slice/StringIter placeholder warnings.
- Production guardrail test:
  `planner_accepted_manifests_validate_against_reader_schema` validates real
  planner-emitted manifest CBOR through the reader schema.
