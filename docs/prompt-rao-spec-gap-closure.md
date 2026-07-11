# Prompt: RAO publication spec-gap closure (remanence, branch rao-wrapped-key-header)

**Status: pending (dispatch after prompt-rao-v2-envelope-impl.md completes —
same worktree).**
Normative: `specs/publication/REVIEW-REPORT.md` + `SUMMARY.md` (2026-07-08
publication review; 261 findings). The spec text was already repaired and pinned
to the reference implementation; this prompt closes the THREE places where the
published spec now promises behavior the code lacks, plus the test-vector
deliverable. Direction from the owner 2026-07-11: fix in the implementation, then
update the spec text accordingly where the implementation pins further detail.

## Scope

1. **Scanner bootstrap re-typing reconciliation** (top critical): implement the
   §12.4 SHOULD-level reconciliation so a destroyed checkpoint-bootstrap block is
   not classified as a 1-block object (which poisons every digest scope over it —
   single-block damage defeating design goal 5). Find the scanner's typing path
   (walker/scanner in the parity/read estate), implement per §12.4, and if the
   implementation resolves ambiguities the spec leaves open, update §12.4 +
   Appendix C to match. Tests: destroyed-bootstrap fixture → correct re-typing,
   digest scopes intact.
2. **Manifest duplicate rejection** (`manifest.rs`): reject duplicate path and
   duplicate file_id per the spec'd rule (REVIEW-REPORT notes manifest.rs does
   not currently check). Negative-vector tests both ways.
3. **Missing-manifest reporting** (`reader.rs`): implement the spec'd reporting
   behavior. Test: object set with absent manifest → the specified report, not
   silence.
4. **Test-vector archive**: generate the vectors the publication specs promise
   ("distributed alongside this specification") as a reproducible artifact
   (make target), compute its SHA-256, and print it into RAO 1.0 §13 / parity
   §17 as the specs require.

## Constraints

- The wire format is SETTLED — nothing in this prompt may change emitted bytes
  for existing objects. If a fix appears to require that, STOP and record it.
- Possible overlap: the TIO thread (tape-io pipeline) also touches the read
  path on other branches. Keep changes minimal and well-factored; the merge
  coordination is handled outside this prompt.
- Spec edits go to `specs/publication/*.md` only (the originals in `specs/`
  stay untouched, per the review's convention).

## Definition of done (AGENTS.md applies)

cargo test green incl. the new fixtures/negative vectors; `make proof-inventory`
unaffected or updated; ordered commit-ready summary (you cannot commit).
