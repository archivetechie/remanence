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
| REM-PARITY | 1.0.0-draft.4 (clean replacement in progress) | 1.0.0-draft.1 | Terminal triple index: three complete final replicas, two typed separation extents, no intermediate indexes, exact close reserve, irreversible finalization/recovery authority, and manual early finalization. Candidate byte tables are in `supporting/rem-parity-terminal-index-byte-draft.md`; review-only minimal and multi-object vectors at every legal block size are under `../../fixtures/rem-parity-terminal-index-draft/`. Older bootstrap/index prose in the preparing copy is being replaced and is not compatibility authority. |

REM-OBJECT and REM-ENCRYPT have no revision in preparation; their published
copies are current.
