# Revisions in preparation

**Nothing in this directory is normative.** The revision of each document that
governs — the one an implementation is measured against, and the one whose
version string a conformance claim refers to — is the published copy in
[../publication/](../publication/).

## Why this directory exists

A published revision is not cut every time an item closes. Items accumulate
until there are enough of them to be worth a revision, a deposit, and the
attention of everyone who has to re-read the document. Between those points
the resolved text has to live somewhere, and the one place it must not live is
the published directory: a reader who opens that directory is entitled to
assume that what they find there is what governs today.

So resolved text collects here, in full, under a version string strictly
greater than the published one. A reader can see both what governs now and
what is being prepared, without having to work out which is which.

## The rules

- The published copy governs, always. Where a copy here and the published copy
  disagree, the published copy is correct and this one is a proposal.
- A document appears here only while a revision is being prepared. It carries a
  version strictly greater than its published counterpart.
- When the revision is cut, the file moves to `../publication/`, its digest is
  recorded in `DEPOSITED.sha256` at deposit, and its copy here disappears.
- The Open Items appendix of a published revision is a snapshot taken when that
  revision was fixed. It is not updated to reflect work in this directory; the
  live list at <https://archivetech.org/spec/issues> is where an item's current
  state is recorded.

## Comment

Comment is as welcome on this text as on the published text, and is more useful
here, because here it can still change the revision cheaply. Raise it the same
way — see the Status section of the document in question.

## Current state

| Document | Preparing | Published | Items closed in the preparing copy |
| --- | --- | --- | --- |
| REM-PARITY | 1.0.0-draft.5 (replacement implemented; pre-freeze fuzz and physical validation open) | 1.0.0-draft.2 | The terminal index has three complete terminal replicas and two typed separation extents. There are no intermediate indexes. The design has an exact close reserve, irreversible finalization and recovery authority, and manual early finalization. The terminal byte tables are part of the specification (Sections 8.3 and 10). Review-only minimal and multi-object vectors at every legal block size are under `../../fixtures/rem-parity-terminal-index-draft/`. Older bootstrap/index prose is superseded and is not compatibility authority. Independent derivation, proof gates and the clean-slate VTL lifecycle have passed. The production-default one-GiB separation layout has passed at all three legal block sizes. Dedicated fuzz plateaus for terminal replicas, separation extents and parser walks remain open, as does supervised physical-media validation.<br><br>Section 1.5 states which rules belong in the specification. The rules it places outside the specification have left it. Most went to the REM Implementation and Operations Guide's chapters on writing, finalizing and reading tapes and on capacity admission. The description of the reference implementation's journals went to that implementation's documentation. Section 18's freeze criteria moved to [“How a revision is frozen”](../README.md#how-a-revision-is-frozen), keeping their existing numbers. The candidate-vector rule and negative-vector coverage list moved there too. Section 3.4 now says that a Writer does not report a tape as `Finalized` while fewer than three complete replicas exist. Section 8.2 lists bootstrap key 3 as optional, and readers still tolerate its absence.<br><br>The preparing copy defines the tape-reading roles and uses consistent terms for terminal replicas, separation extents, the terminal suffix and replica envelopes. It separates requirements from rationale and splits compound requirements. Section 12.3 item 1 now checks the frame's `tape_uuid`.<br><br>The scope move and readability pass changed no byte, valid tape or vector. |
| REM-OBJECT | 1.0.0-draft.4 (scope move done) | 1.0.0-draft.3 | Section 1.6 states which rules belong in the specification. The rules it places outside the specification have left it: most for the REM Implementation and Operations Guide, whose first revision this change writes, and the descriptions of the reference implementation's reader and wrapper tooling for that implementation's documentation. Sections 13.1 and 14 now say that the vectors' `expected.default_restore` fields are informative for conformance. No byte, valid object or vector changed. |
| REM-ENCRYPT | 1.0.0-draft.4 (scope move done) | 1.0.0-draft.3 | Section 1.6 states which rules belong in the specification. Because REM-OBJECT's rules on tool behaviour have left that document, REM-ENCRYPT no longer carries four requirements it took from REM-OBJECT by reference: reporting every nonconformity (Section 7.4), never panicking, crashing or allocating without bound on envelope input (Sections 11 and 12.9), REM-OBJECT's host-protection rules for restored members (Section 5.10), and re-reading a copy before recording it durable (Section 7.3). The other rules Section 1.6 places outside the specification have now left it too. Most went to the REM Implementation and Operations Guide, whose chapter on keys and secrets this change writes, and the rule on deterministic vector-generation hooks, with its release controls, moved to [“How a revision is frozen”](../README.md#how-a-revision-is-frozen). Sections 5.3 and 5.10 now state their rules as claims. Section 13 now says that an `expected_error` is matched by identifier name, and Section 13.4 that the vector `writer-one-slot` is informative for conformance. No byte, valid object or vector changed. |
