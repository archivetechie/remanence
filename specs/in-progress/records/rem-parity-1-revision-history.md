This is the revision-history record for the [Rem Tape Parity (REM-PARITY)
Format](../rem-parity-1-specification.md), preparing revision 1.0.0-draft.5.
It is informative.

At cut, this record moves with the specification to `specs/publication/records/`
and is deposited with it, with its own digest line in `DEPOSITED.sha256` and a
listing in the deposit description.

Section references name that specification unless another document is named.
Historical appendix letters retain the meanings stated in each entry; the
current [open-items record](rem-parity-1-open-items.md) holds Appendix D.
The former Status policy remains in the specification under
[Versioning and Change Policy](../rem-parity-1-specification.md#versioning-and-change-policy).

## Review Status at This Revision

**This is a review draft.** It is published for public review and is not yet
frozen. Generation 2 is implemented in the reference tree. Its review-only
candidate vectors for the terminal suffix are pinned and re-derived by the
terminal verifier without calling the Rust codec, and its proof and nonphysical
lifecycle/VTL gates have passed. Dedicated coverage-guided
terminal-replica/separation/parser-walk fuzz plateaus and the supervised
physical-media gate remain open. So does REM-PARITY freeze criterion 2,
recorded in `specs/README.md`. Its evidence is gathered as candidates:
whole-tape images (among them Appendix A.4's minimal tape); damage-matrix,
resume and negative vectors, most of them pinned; and a second implementation,
written from this document, that re-derives the pinned bytes Section 17 lists.
The criterion remains open until the items that Appendix D item TT-2 lists are
closed and the companion archive carries the fixtures at freeze. Candidate
vectors are not publication artifacts until the freeze gates close.

Section 11 states six Writer rules for tape files, commits, append positions,
epochs, parity from read-back and finalization; Sections 3.4 and 14 use that
contract. Barrier timing and short-epoch boundaries are Writer choices; this
change alters no tape byte, valid tape or Reader rule. Resume-vector inputs
add ordered block CRC evidence; existing outcomes stay unchanged, and one
negative case rejects a changed recorded CRC.

A finalized tape may carry an unprotected range `[W, T)`, which Verifiers
report; no tape valid before becomes invalid. One draft vector (isolated-3),
which tested only the retired equality, now expects a valid tape; no other
vector outcome changes, and the reference Reader follows in migration step 6.

## Appendix C. Revision History (Informative)

Entries are newest first. Each carries: date · version · kind
(erratum / minor / major / draft milestone) · what changed and its effect on
conformance. Milestones that predate the first published revision are marked
`[draft]`; they were reached in public working drafts, not in published
revisions of this specification, and the change policy in [Versioning and Change Policy](../rem-parity-1-specification.md#versioning-and-change-policy)
governs only the revisions that follow the first published one.

Each entry keeps the appendix lettering of the revision it describes: entries
before draft.5 use C for the closure record, D for the revision history, and E
for the open items. This lineage's "1.0.0-draft.2" entry (2026-08-02) is not
the published revision 1.0.0-draft.2 (2026-09-06, SHA-256
`1e4ad796bd77291f731d8d2611231984ba778f3650c1597f80eebdc7ba359fd2`), which is
an errata revision of draft.1.

- **2026-10-30 — 1.0.0-draft.5 — draft milestone (vectors).**
  `resume-open` becomes informative and pins one conforming result; a resumed
  Writer can end or continue the open epoch under Section 11.4 (W4) and the
  last paragraph of Section 11.6. `resume-closed` stays pinned. The inputs and
  expected results stay fixed. E9, `short-epoch-followed-by-another`, adds two
  short epochs with separate sidecars and a finalized terminal suffix, and pins each sidecar's
  coverage and recovery of both Objects. E9 is added after the blind comparison
  and checked against the second implementation without a blind decision;
  TT-2 records its evidence without changing freeze criterion 2. Both
  implementations exercise the cases, and the draft vector archive is rebuilt.
  No normative rule or existing vector outcome changes apart from `resume-open`'s classification.

- **2026-10-30 — 1.0.0-draft.5 — draft milestone (companion records).**
  Appendices C and D move to informative companion records, with their headings
  kept as pointers. Most of the former Status section leaves that section:
  review history moves to this record, and the version axes, finality promise
  and change policy remain in the specification under the new Versioning and
  Change Policy heading. Status keeps the document's standing and record links.
  The records move and are deposited with the specification; the deposited
  open-items record is the snapshot at that revision's freeze. No format
  requirement or vector outcome changes.

- **2026-10-29 — 1.0.0-draft.5 — draft milestone (uncovered finalization).**
  W6 lets finalization continue after a W5 proof failure on the operator's
  recorded acceptance; the uncovered epoch gets no sidecar.
  A finalized tape may carry an unprotected range `[W, T)`, which Verifiers
  report; no tape valid before becomes invalid. One draft vector (isolated-3),
  which tested only the retired equality, now expects a valid tape; no other
  vector outcome changes, and the reference Reader follows in migration step 6.

- **2026-10-29 — 1.0.0-draft.5 — draft milestone (Writer contract).**
  Section 11 states six Writer rules for tape files, commits, append positions,
  epochs, parity from read-back and finalization. Sections 3.4 and 14 use that
  contract. The atomic object-and-sidecar bundle, the one-file-in-flight rule
  and the object-boundary restart prescription are removed; the commit bound
  remains. Writers choose barrier timing and short-epoch boundaries, and prove
  read-back bytes against evidence recorded no later than commit. Sections 3.4
  and 6.6 state the Writer-side catalog-less limit and the cost of short epochs.
  This is a substantive change of Writer and Resumer obligations in the review
  draft; no tape byte, valid tape or Reader rule changes. Resume-vector inputs
  add ordered block CRC evidence; existing outcomes stay unchanged, and one
  negative case rejects a changed recorded CRC.

- **2026-09-06 — 1.0.0-draft.5 — replacement review draft.** States that the
  concept DOI remains reserved pending the first deposit, corrects the
  generation-number references in the shared status policy, and identifies
  the criterion-5 clean-room exercise as technical independence by an AI
  system rather than organizational independence.

  The same revision makes the text complete and correct for generation 2. It
  changes no tape byte and no behaviour of the reference implementation; where
  the earlier text described Reader behaviour differently from the reference
  implementation (the discovery order of Section 8.4 and the agreement set of
  Section 8.5), the text now states what the reference implementation does. It
  restores, adapted to generation 2, the ParityMap text that draft.4 dropped:
  Section 10.1 now gives the purpose of the sidecar epoch directory, the copy
  layout, the payload map with its key 6 and 7 obligations, the Reader's
  locator and agreement rules, and the SidecarEpochDirectory with separate
  Writer rules and decoder invariants, and Section 15 gains
  `DirectoryInvalid`. It moves the exact terminal bytes into the document: the
  planned layout and layout digest into Section 8.3, and the structural rows,
  the Object recovery rows (formerly a subsection of Section 8.2), the replica
  frame with a Kind column, the separation extents, and the replica validity
  conditions into Sections 10.2 through 10.6, which renumbers Section 10. The
  replica-agreement rule is stated once, in Section 8.5, over the
  edition-common fields of the replica frame, and the discovery steps of
  Section 8.4 consider every valid replica before accepting one. It makes
  these corrections:

  - It corrects the sidecar `schema_version` in Section 9.2 from 1 to 2.
  - It makes Bootstrap keys 20, 21, and 30 reserved.
  - It scopes the Section 8.1.1 registry to `schema_major` 2.
  - It replaces the deposit claim of Section 17 with the archive digest.
  - It removes the Status sentence that called every Section 18 criterion
    satisfied.
  - It names the slot counts `structural_row_count` and `object_row_count`.

  It also writes down rules the reference implementation already enforces but
  no earlier revision stated:

  - the record-count relations of Section 8.3;
  - the ParityMap copy agreement, the u32 bound on directory flags and the
    Writer rule that the sidecar epoch directory is marked final, in
    Section 10.1;
  - the structural-slot container and the encoded-length bounds of every fixed
    slot, structural and Object, in Section 10.2;
  - the footer arithmetic and edition-identity rules in Section 10.4;
  - the separation-extent validity rules in Section 10.6.

  The draft.1 closed-item snapshot, formerly Appendix C, described the
  checkpoint-bootstrap/parity-map design and does not establish conformance to
  the generation-2 replacement; that appendix is removed, and the revision
  history and the open items become Appendices C and D. No tape byte changed.

  Section 1.5 is added. It states which rules belong in this document and
  points to the REM Implementation and Operations Guide for the rest. A rule
  about what an implementation does stays only if ignoring it would do one of
  three things. It would produce bytes another conformant reader cannot read
  or would misread. It would let two conformant readers reach different
  conclusions about what a medium or an object contains. Or it would let a
  tool claim something the bytes do not support. The same section appears,
  word for word, as the last subsection of Section 1 of REM-OBJECT,
  REM-ENCRYPT and REM-PARITY.

  The rules that Section 1.5 places outside this document have now left it.
  Most went to the REM Implementation and Operations Guide, whose chapters on
  writing tapes, finalizing a tape and recovering from a crash, reading tapes,
  and capacity admission this change writes. Those chapters hold what a commit
  record should contain and how several records share commit authority,
  barrier batching, staged records, writer poisoning, position tracking, the
  compression check, resuming a tape, the finalization lifecycle as a tool runs
  it, the BOT walk's notice, progress, abort and retry budget, the selection
  order among agreeing replicas, the streaming inventory interface, when
  recovery refusals are made, bulk-recovery planning, and admission refusal.
  Other chapters of the Guide hold the allocation, panic, escaping and fuzzing
  practice of Sections 5.3, 10.2, 10.6, 15 and 16.2, reporting every
  nonconformity, exposing typed errors, the advice to encrypt confidential
  payloads, and the Writer rules for the descriptive fields: bootstrap keys 3
  and 4, ParityMap keys 6 and 7, and a random edition identity. The
  description of the reference implementation's journals, formerly the body of
  Appendix B.12, went to that implementation's documentation. The freeze
  criteria of Section 18, under their existing numbers, the rule on handling
  candidate vectors and the negative-vector coverage list of Section 17 went to
  the “How a revision is frozen” section of `specs/README.md`.

  Some rules stay in a changed form. Section 3.4 now says that a Writer does
  not report a tape as `Finalized` while fewer than three complete replicas
  exist, instead of prescribing that it retain `RecoveryRequired`; the
  `FinalizedDegraded` transition is unchanged. Section 8.2 now lists key 3 as
  optional, as key 4 already was; readers still tolerate its absence, and key 5
  stays required. The advice to payload formats in Section 4.3 and the advice
  on encryption in Section 16.4 are now informative text. Section 13.2 keeps
  its refusals and no longer says when they are made. Section 14 states step 4
  as its outcome, and its step 5 has left. Where a paragraph or more left, the
  section points to the Guide.

  No byte of the format changed, and no valid tape or vector changed. This
  document no longer requires a conformant tool to survive hostile input, to
  report every nonconformity, to report progress to an operator, to record
  which software wrote a bootstrap, or to refuse an Object that would leave
  too little room to finalize; the Guide recommends each.

  The readability pass then changed the text as follows. No byte of the format
  changed, and no valid tape or vector changed.

  - Sections 10.5, 12.3, 13.2 and 18 are retitled “Separation Extents”,
    “The Classification Ladder”, “Typed Refusals” and
    “Freeze Criteria and Status”, so that each title describes what its
    section now holds. Appendix B's headings and the qualifiers of
    Sections 8.1, 9.2, 9.6, 10.1.3 and 11.1 take title case.
  - Section 2.2 defines two roles: the Reader, any role that reads a tape,
    including the Writer when it reads back what it has written; and the
    Inventory Consumer, which receives a Scanner's inventory. A requirement
    that bound “readers”, “decoders” or a “Consumer” now names one of these
    roles, or the single role it binds, such as the Recoverer in Section 3.4.
    The rules of Sections 5.3, 7.4 and 10.1.5 keep the condition under which
    they apply.
  - Section 2.3 defines the terminal suffix, the terminal replica, the replica
    envelope and the separation extent, and points to the sidecar epoch
    directory. The document uses these terms where it said “gap”,
    “typed extent”, “member”, “terminal triple”, “five-file suffix”,
    “pre-tail” or a bare “envelope”, and names the tape-file kinds by them.
  - Section 2.3 states that the capitalised word “Object” is the defined term
    and that attributive uses are lower case. In running text the bootstrap is
    “the bootstrap”, and “BOT” stays only where the position is the point. The
    producing role is written “Writer” throughout.
  - The symbol `E` now means only a separation extent's nominal bytes
    (Sections 10.5 and 17). Section 3.3 and Appendices A.1 and B.2 write an
    epoch's size as `S × k`.
  - Section 2.1 declares that a paragraph opening with *Rationale.* is
    informative, and that the steps of a numbered list are taken in the order
    given unless the text says otherwise. Section 12.3 says that its items are
    numbered for reference, not as an order of trial. An explanation of why a
    rule exists now stands in a *Rationale.* paragraph.
  - Section 1.3 states how this document cites its companions, and
    Section 10.3 cites REM-ENCRYPT §5.2 and REM-OBJECT §4.5.1 in that form.
  - The Status section gives this document's version axes as a table, and
    limits “never reused for different bytes” to the version string of a
    deposited revision. The legacy terms for generation 2, “replacement” and
    Section 5.2's “retained codec”, are gone from the Status section,
    Sections 2.5, 5.2, 8, 8.1, 8.1.1, 8.2 and 8.4, and Appendix D.
  - Sections 1.2 and 3.1 say that the labels in their diagrams abbreviate the
    terms of Section 2.3, and Section 1.2 and Appendix A.4 describe replica
    selection by its outcome, since the order of trial is the Guide's.
  - Errata in the tables and the ladder: Section 12.3 item 1 checks the
    frame's `tape_uuid`, which the payload does not carry, and the ladder now
    says where the tape identity comes from; the Section 8.1.1 registry names
    REM-PARITY 1.0 as the revision that defines value 3, and reserves values 0
    to 2 permanently: they are never assigned under `schema_major` 2; and
    Section 8.2 lists key 3 as optional in its Presence cell, as key 4 is,
    while the text below the table keeps the Reader's obligation.
  - Errata in the text: the Abstract places the ParityMap before the terminal
    suffix; Section 9.6 cites Section 12.3 item 6, the sidecar footer probe,
    which the renumbering in draft.4 had left as item 4; Section 3.4 no longer
    refers to “required contents” that moved to the Guide; Section 4.2 no
    longer names a function of the reference implementation; Section 16.4 no
    longer speaks of several bootstrap timestamps; and the paragraph above on
    Section 1.5 says where that section stands in each document.
  - The draft.1 entry's audit narrative is cut to the readings of the text it
    concluded, one bullet each; how the implementation was fixed and tested is
    left to the implementation's own record.
  - Sentences that stated more than one requirement, or a requirement and an
    aside, were split, among them Section 3.3's statement of where parity
    shards live and Section 16.2's bounds on diagnostic text. Where a split or
    rewritten requirement binds a role, it now names that role.

  The vector step then changed the text as follows. No byte of the format,
  valid tape or vector changed.

  - Erratum, Appendix A.4. The minimal tape now includes, as tape file 3, the
    one final ParityMap that Sections 10.1.1 and 10.6 require when final
    parity closeout has a nonempty sidecar epoch directory, and its replicas
    carry four structural rows. The appendix is informative; no rule changed.
  - The Status section, Section 17 and Appendix D item TT-2 now say that the
    candidate set does not yet meet freeze criterion 2, and what the
    independent verifier does not yet re-derive. TT-2 also says that no
    vector yet covers append resume, and how the pinned archive's
    generation-1 cases are covered.

  Decisions on questions the review raised then changed the text as follows.

  - Sections 13.1 and 13.3 now make the sidecar epoch directory available
    whenever the tape's final ParityMap validates and is marked final, through
    a validated replica's structural rows or the Section 8.4.1 walk. Section
    10.1.1 defines the Writer's four authority fields: the canonical digest
    covers tape files 0 through the ParityMap's own entry, the scope count is
    its tape file number plus one, and the total-data and highest-protected
    ordinal fields are that prefix's `T` and `W` at finalization. Section 13.1
    validates the reconciled walked projection, scope count and recomputed `T`
    and `W` against those fields, establishing its scope and durable boundary. A
    mismatch with a validated final ParityMap gives no map, with no fallback
    to the bootstrap's scope; only when no final ParityMap validates does the
    bootstrap's scope remain. Sections 12.3 and 13.3 require walk identification
    by directory tape file and measured length, confirmed by the Section 13.1
    digest and scope checks. Identification reads no sidecar metadata, so
    unreadable or checksum-invalid metadata cannot prevent identification of
    other epochs. The tail hash is checked before the tail's metadata is used
    for rescue, so unvalidated tail metadata cannot guide recovery.
    Section 13.3's Recoverer prohibition covers every sidecar map entry the
    rescue uses on both routes: agreement is checked before placing a
    directory-assisted read. Section 15 widens `FilemarkMapDigestMismatch` to
    cover a walked map whose projection or scope fields disagree with the final
    ParityMap, retaining the replica structural projection case. The Recoverer
    rule settles what Section 13.3 left undecided. The Writer definitions, the
    Scanner's second pass and the Recoverer rule are a minor change of the draft.
  - Section 8.4 now points to Section 8.5 when footers propose different
    planned layouts. The pointer is informative and changes no requirement.
  - Section 8.4 now permits a Scanner to discover terminal replicas when the
    bootstrap is unreadable and the expected tape UUID, block size and parity
    scheme are supplied out of band. A readable bootstrap that disagrees
    with the supplied values is refused, not overridden. Section 8.4.1 now
    names the parity scheme (`k`, `m`, `S`, or no parity) as a third mandatory
    hint for the BOT walk. The Reader permission and the third mandatory
    hint are a minor change of the draft.
  - Sections 5.2, 9.2 and 12.1 now name the supplied tape UUID as the source
    of tape identity when the bootstrap is unreadable, and Section 12.3's
    pointer also cites Section 8.4. Sections 6.6, 13.1 and 13.3 now name the
    supplied scheme for that case, and Section 15's `SchemeMismatch`
    description includes it. These changes carry the hint rule through the
    identity and scheme checks.
  - Section 10.3 now points to REM-ENCRYPT §5.3 for distinct epoch ids in
    every key frame and the Sealer's nonzero rule. The Object recovery row
    still requires both. The pointer adds no requirement.
  - Appendix D gains item TT-7: which bootstraps count as unreadable is to be
    decided before freeze.

  No byte of the format changed, and no valid tape or vector changed.

  The fixture step then changed the text as follows. No byte of the format and
  no valid tape changed; the candidate set gained the vectors Section 17
  describes.

  - Section 17 now describes the tape images, the damage matrix, the resume
    vectors, the negative vectors and the second implementation, and no longer
    says that they do not exist. It removes the sentence that the verifier does
    not yet re-derive bootstrap, sidecar or ParityMap bytes, or Reed–Solomon
    parity, and TT-2 removes the sentence that no vector yet covers append
    resume. It calls the verifier of the terminal bytes the terminal verifier.
    The Status section says that the evidence for freeze criterion 2 is
    gathered and that the criterion remains open until the items of TT-2 close.
    Appendix D item TT-2 records the evidence and lists what remains open: the
    values the second implementation does not yet re-derive, and the formulas,
    questions and readings the text has yet to settle.
  - Wording follow-ups of the vector step change no requirement. Section 10.3
    says which rules key 22's recipient epoch ids obey; Section 14 step 1 says
    "the tape's torn tail"; Section 17 records the reading of `lost_count`
    behind the burst cases; the Status section and TT-1 describe the terminal
    verifier as Section 17 does; and paragraphs left with ragged wraps by
    earlier edits are rewrapped.

  Rulings on the questions the fixtures raised, and readings of the text the
  fixtures showed to be open, then changed the text as follows. No byte of the
  format changed. Where a Reader's outcome changes, the change is stated. The
  rulings, and the new requirements of Sections 2.2, 2.4, 3.5, 7.2, 8.4, 9.6,
  10.1.3, 10.1.4, 12.3 and 14, are a minor change of the draft.

  - Section 12.3 defines when a rung recognises a tape file. Items 2 and 3
    commit a file to its control type when the magic matches; items 1, 4, 5
    and 6 recognise a file only when it passes every check, its count
    included; a file that parses at such a rung but fails one of its checks is
    not recognised, and a count mismatch is reported as damage. A primary
    sidecar header that parses decides the sidecar rung, so item 6 is not
    tried after item 5 fails. With the head unreadable, a terminal footer now
    establishes the control type, where the text had said it may.
  - Sections 9.1 and 13.3 say which copy of a sidecar's index a Recoverer uses
    when the two differ. A valid footer decides, unless an available sidecar
    epoch directory entry disagrees with it, in which case neither does; when
    the footer is unusable, an available sidecar epoch directory entry
    decides; when neither is available, a Recoverer whose primary copy
    validates reads the tail copy too, and two valid copies that differ leave
    the epoch metadata-unavailable. With a valid footer, the
    directory-assisted rescue is not used. A Verifier reads both copies and
    reports a divergence.
  - Section 13.3 makes `total = 2H + P + 1`, with `H > 0`, a precondition of
    the directory-assisted rescue, and states that for such an entry the tail
    copy's block is `H + P`. An entry that fails the precondition is not
    available; it affects only its epoch. The Recoverer reports
    `SidecarMetadataUnavailable` whenever no copy can be validated.
  - Section 8.4 states, in two stages, which bootstraps count as unreadable
    when values are supplied, and a Scanner given the values now MUST use them
    when it cannot read the bootstrap. A first record whose length differs
    from the supplied block size is refused. Section 15 states the bootstrap
    error names by level. Appendix D item TT-7 closes.
  - Section 2.4 defines every formula by its exact integer value: a value
    that is recorded or used must fit, and a value that is only compared is
    compared exactly. Commit-record values that do not fit are `ResumeAppend`,
    and device-reported positions that do not fit or go backwards are
    `TapeIo`. Section 3.3 states that an inverse ordinal that would not fit is
    an implicit zero. Section 7.2 requires every position a recorded map
    describes to fit in u64.
  - Section 8.4 states the rule by which a footer supplies the planned
    terminal layout: it must be where it says, and the tape must not run past
    the layout's planned EOD.
  - Section 9.6 requires the sidecar footer's `tape_uuid` to match the tape.
    Section 10.1.3 requires the ParityMap's `block_size` to equal the tape's,
    and Section 10.1.4 extends header–footer agreement to every field both
    carry.
  - Section 10.6's now-retired `W = T` rule names the recorded fields; Section 12.6
    defines a degraded inventory; Section 13.5 defines `lost_count`.
  - Section 14 refuses a committed prefix that records a terminal component
    or a final ParityMap, and names the outcome of a failed re-read.
  - Section 3.5 states that a record shorter or longer than one block is
    invalid content of its component, never `TapeIo`.
  - Sections 8.4 and 16.3 refuse recorded drive compression on a parity tape
    only; a no-parity bootstrap may record compression, as Sections 8.2 and
    11.4 already allow. Section 2.2 says that a Verifier reports damage it finds
    anywhere with the error a Reader reports for that component.
  - The owner's rulings on two further questions then add: Section 13.3
    requires a Recoverer, when the footer, the primary copy and the final
    ParityMap are all unavailable, to try the tail copy at the position the
    terminal index's record of the sidecar gives, and to use it only if it
    validates and agrees with that record. An epoch that was
    metadata-unavailable in that case can now be recovered; and Section 2.2
    says that a full verification reads every data block and parity shard, and
    that a check of structure and metadata alone is not one.
  - A later text revision states rules that the reference implementation and
    the pinned vectors already follow and that the text had left unstated:
    what a refusal and an unreadable bootstrap do in discovery with supplied
    values, which footers a Scanner reads, how the walk measures a file and
    reports a torn or wrong-length one, the constraints on the footer's and
    the header's total block count, which comparisons a replica's validity
    includes, the order of the checks in the directory-assisted rescue, what
    makes a final ParityMap valid, and what a Verifier reports of an epoch
    whose index is missing. Appendix D item TT-2 records the limits that
    follow from these rules and states the reference as it is after the
    Verifier's full verification and the sidecar pass were built. Three points
    were decided. A structural artifact after the exact terminal suffix is not
    an Object of any inventory, by scope, and the walk may still list it as a
    candidate (Section 12.6). A footer whose magic matches commits a tape file
    to its control type also when the head is readable and no head rung parses
    it (Section 12.3). A no-parity bootstrap carries no parity scheme, so that
    a scheme record in its payload is `BootstrapParse`, and the sentence of
    Section 8.2 that allowed the record to be omitted now requires its
    omission
    (Sections 8.2, 15). The first two describe what the reference already
    does. The third changes the reference, whose parser accepted such a
    bootstrap; it now refuses one. The reference's Verifier also now reports
    an unreadable ParityMap copy, which Section 2.2 requires, even when the
    other copy is used, and an unreadable ParityMap footer, or one that is a
    record of the wrong length. A footer that reads and is invalid rejects the
    ParityMap (`ParityMapParse`). Any such finding leaves the tape not
    complete. No tape byte changes.
  - Sections 15 and 17 and Appendix D item TT-2 record these changes. TT-2
    also records what the second implementation now re-derives and what
    remains open.
  - A wording pass over the text of that revision then changed no
    requirement. Section 8.4 says that a scheme record that can still be
    decoded, in a no-parity bootstrap, is refused naming the scheme when the
    supplied scheme is no parity, and that the second row of its second table
    refuses the bootstrap when the supplied scheme is a parity scheme. Section
    10.1.3 repeats Section 2.2's rule for a ParityMap footer that cannot be
    read and for one that reads and is invalid. Section 10.6 ends its second
    list with the covered-count relationship. Section 12.3 says that disjoint
    magics do not make its rungs exclusive. Section 13.3
    drops a sentence that defined a final ParityMap's validation by itself,
    and says that a validated final ParityMap excludes the tail rescue from
    the terminal index also when it has no entry for the sidecar. Section 17
    states the damage matrix as it now stands before it describes the first
    25 cases. Appendix D item TT-2 no longer lists two reference gaps that
    the reference has closed. No tape byte and no vector outcome changes.
  - A further revision of Section 12.3 changes four requirements, where the
    text left a point open or disagreed with the reference implementation. It
    is a change of requirements and not of wording. No tape byte changes. The
    reference implementation already follows the first three changes, and the
    fourth is covered below.
    - The walk with supplied values types tape file 0 by its measured length
      when its first record is unreadable: one block is a bootstrap that the
      supplied values classify and do not authenticate, and more than one block
      ends the walk with `FilemarkMapReconstruct`, because no map can hold
      anything else at tape file 0. A filemark or EOD where the first record
      should be ends the walk in the same way, and a first record of the wrong
      length is refused and not typed. The rule is stated as an exception for
      tape file 0, beside the paragraph on an unreadable head block, whose text
      is kept; that paragraph gains one sentence, for the fourth change.
    - A rung parses a file when the file passes every check of that rung except
      its measured block count, and item 1 checks that the file is tape file 0.
      A bootstrap magic at any other tape file no longer classifies the file
      as a bootstrap.
    - On the footer route, a footer whose magic matches establishes the
      control type of items 2 and 3 whether or not the footer parses. A footer
      that does not parse, or whose count differs, is reported as damage. The
      two sentences that required a fully parsed footer are rewritten, because they contradicted the rest of Section 12.3
      and Section 10.6.
    - A ParityMap is recognised by its first block and its header, or, when
      the first block is unreadable or is no other rung's kind and not a
      ParityMap header that parses, through the footer and the tail header copy
      that the footer locates. Its payload is checked when the ParityMap is
      read. A ParityMap with an intact header and a damaged payload is no
      longer an Object candidate, so it no longer changes the data ordinals of
      the Objects after it. The route through the tail copy applies the copy
      fallback of Section 10.1.3 in the walk. For a first block that reads but
      is damaged, the earlier text already recognised the ParityMap through
      that fallback, and this text keeps that outcome. For a first block that
      cannot be read, the earlier text made the file an Object candidate, and
      Appendix D recorded that the walk could not find such a ParityMap; this
      text widens recognition to that case. The paragraph on an unreadable
      head block gains the sentence that its last-block read serves this
      route.

    The reference implementation does not yet follow the tail route of the
    fourth change: it types such a file an Object candidate. A later commit
    changes it, and until then the reference and this text differ there.
    An implementation written from the earlier text agrees with this text when
    a ParityMap's first block reads but is damaged and the tail copy is intact.
    It differs when the first block cannot be read; when the first block is
    damaged and the payload is damaged too, since the tail route reads no
    payload and the earlier fallback needed one copy's payload to validate;
    and when a ParityMap's payload or footer is damaged and its first
    header is intact. It differs on the third change when a terminal footer
    does not parse, and on the first and second when tape file 0 is unreadable
    or a bootstrap magic appears at a later tape file. One open point remains, and it is recorded
    for a later revision: a readable, valid bootstrap at tape file 0 that
    measures more than one block ends the reference's walk. Section 12.3 makes
    such a file, which item 1 does not recognise, an Object candidate (item 7),
    and the 1.0.0-draft.1 entry of this appendix records the reading that a
    Scanner does not abort a whole catalog-less walk for a count mismatch.
  - The reference implementation then gained the tail route of the fourth
    change, and fourteen damage cases (`s6-01` to `s6-14`) pin the four
    changes; `s6-10`, `s6-12` and `s6-13` pin the tail route, and `s6-11` and
    `s6-14` a ParityMap that neither route recognises. Section 17 counts
    them, and Appendix D item TT-2 no longer lists the tail route as a
    reference gap. No tape byte, requirement or earlier vector outcome
    changes.
  - Section 10.1.3 now says that a ParityMap header copy whose block cannot
    be read is invalid, as one whose CRC does not verify is, so the other
    copy is used when it is valid. The text had said how a footer that cannot
    be read is treated and not a header copy. It is a change of requirements
    that states what the reference implementation and the second
    implementation already did; `s6-13` pins it. No tape byte and no vector
    outcome changes.
  - Section 13.3 now says that the order of its steps is an order of
    precedence and not an order of reads. A Recoverer may read a sidecar tape
    file forward once, retain the blocks the steps need and apply the steps
    to them, provided it continues past unreadable records, places each block
    by a position the device reports or a positioning command establishes,
    and reads at its position any named block it has not obtained. It then
    reaches the same decisions as one that reads the footer first. A
    rationale paragraph gives the reason. This settles a reading the earlier
    text left open, and a Recoverer that reads the footer first remains
    conformant. No tape byte and no vector outcome changes.
  - Section 12.2 now says that a Reader whose role reads every block of a
    tape file in order may measure the file from those reads instead of by
    filemark spacing. The block count is the device's position after the
    trailing filemark, less the head block's position, less one, provided
    every read returned one whole block and the reported position agrees with
    the number of reads. A file that does not meet both conditions is measured
    and classified by the walk from its head block. Section 12.3 classifies
    the file from the blocks its rungs name, so the file is a tape file of the
    walk in every respect. Section 2.2 says that a Verifier may measure a file
    of the prefix this way when its full verification reads every block of
    that file, and that the prefix ends at replica A's planned start. A
    rationale paragraph gives the reason. This is a new permission: the
    measurement, the classification and every finding are the walk's, no
    requirement is relaxed, and a Reader that measures by spacing remains
    conformant. No tape byte and no vector outcome changes.

    Section 2.2 says that a Verifier may perform its full verification in
    parts, at different times and on different drives, when every part reads
    the same physical cartridge and verifies against the same edition and
    terminal-layout digests, and the parts together make every check of a
    full verification; it does so only when a terminal replica validates in
    every part, the verification finishes when its last part finishes, and a
    report that a tape verified in parts is complete states when the first
    part began and the last part ended. This is a new permission: no
    requirement is relaxed, and a single uninterrupted full verification
    remains conformant and unchanged. No tape byte and no vector outcome
    changes.
- **2026-08-11 — 1.0.0-draft.4 — replacement review draft.** Replaces the
  geometric/checkpoint-bootstrap design with one BOT Bootstrap and exactly
  three complete terminal index replicas separated by two typed extents.
  Adds checked streaming payload geometry, terminal capacity reservation,
  resumable five-component finalization, manual early finalization, and the
  explicit all-replicas-invalid BOT recovery taxonomy. Clarifies that optional
  fsynced host checkpoint authority may recover exact Object identifiers only
  where its tape identity, block size, committed prefix, file numbers, block
  counts, and complete Object-row bijection agree with the physical scan. This
  host-assisted classification changes no REM-PARITY media bytes. Draft.4
  remains pre-freeze pending the open evidence in Appendix E.

  The generation-2 sizes and widths, taken from a diff of the Section 2.5
  constants and the byte tables of the published revision against this one:

  - `BOOTSTRAP_SCHEMA_MAJOR` 1 to 2.
  - Bootstrap `sequence` widened from `u32` to `u64`, which moves
    `cbor_payload_len` to 0x2C, the header CRC to 0x30 and the payload to 0x38
    (`BOOTSTRAP_HEADER_LEN` 0x34 to 0x38), and raises the Bootstrap floor from
    0x3C to 0x40.
  - Sidecar `schema_version` and footer version 1 to 2.
  - Sidecar header `parity_block_count`, `data_crc_count`,
    `sidecar_header_block_count` and `inline_index_entry_bytes` widened from
    `u32` to `u64`, which moves the header CRC to 0xC0 and the inline index to
    0xC8 (`SIDECAR_HEADER_LEN` 0xB8 to 0xC8).
  - Sidecar footer `H` and `P` widened from `u32` to `u64`
    (`SIDECAR_FOOTER_LEN` 0x80 to 0x88, footer CRC 0x78 to 0x80).
  - Sidecar floor 0xD0 to 0xE0.
  - ParityMap schema and footer versions 1 to 2.
  - ParityMap `sequence` and `directory_scope_tape_file_count` widened from
    `u32` to `u64` (the other two scope fields were already `u64`), which
    moves every later header and footer field and makes the header and footer
    0xC8 bytes (CRC at 0xC0), the payload offset 0xC8, and the ParityMap floor
    0xC8.
  - Tape-file kinds 4 and 5 added, with the terminal replica and separation
    constants.

- **2026-08-08 — 1.0.0-draft.3 — review draft.** Defines *committed prefix*
  explicitly in Section 3.1 and clarifies the existing durable-boundary rule
  in Sections 3.4 and 14: when an implementation distributes the logical
  commit record across multiple records that it designates as required commit
  authority, those records must be present, their overlapping claims must
  agree, and their validated combination must yield one complete and
  unambiguous resume state before append positioning or writing. A missing,
  conflicting, incomplete, or ambiguous authority is a `ResumeAppend` failure;
  a rebuildable cache does not commit a tape file.
  Appendix B.12 records how the reference implementation applies that rule to
  its Layer 3c and checkpoint journals, including the empty-prefix treatment
  of an uncommitted bootstrap-only projection.

  This is a clarification of the abstract off-tape commit-record obligation,
  whose storage format remains implementation-defined. It changes no on-tape
  byte, tape-file ordering, Reader behavior, schema value, or pinned vector.

- **2026-08-02 — 1.0.0-draft.2 — review draft.** Resolves Appendix E item
  RP-2. Section 8.2 now bounds bootstrap key 3 to 128 bytes of printable
  US-ASCII and key 4 to 64 bytes of [RFC3339] `date-time`, states the Reader's
  obligation to treat a violating value as absent, and requires escaping
  wherever either value is rendered; Section 16.2 records the same bound among
  its hostile-input posture. Section 10.4 carries the same obligations to the
  parity_map payload's keys 6 and 7, which are the same two fields in the other
  structure that holds them. RP-2 as raised named only the bootstrap keys;
  bounding one pair and leaving the other unbounded would have closed the item
  without closing the hazard.

  This revision also promotes key 3 (and its parity_map counterpart, key 6)
  from OPTIONAL to a Writer MUST, and recommends a matchable
  `<implementation>/<version>` form. RP-2 treated both keys as disposable
  diagnostics. That was wrong about key 3: a tape is produced by an
  implementation and not by a specification, so when a cartridge does not
  decode as this document says it should, the identity of the writing software
  is the only thing that resolves the disagreement — and free text nobody can
  match to a release does not do that job. Readers still tolerate absence, so
  no earlier tape and no other implementation's tape is affected. No tape
  written under draft.1 becomes invalid and
  no Reader outcome changes: both keys were already OPTIONAL, so the treatment
  a violating value now receives is the treatment every conformant Reader
  already gave their absence. Were this a published revision rather than a
  review draft, it would classify as a minor revision under question 3 of the
  Status section's change policy. No pinned vector is affected, and the
  vectors already satisfy the new Writer rule; `schema_minor` is untouched.

- **2026-07-31 — 1.0.0-draft.1 — review draft.** Published for public review;
  not yet frozen. On freezing this becomes version 1.0.0, the first published
  revision, and the change policy in the Status section governs everything
  after it. This revision closes the Appendix C items and satisfies the
  Section 18 criteria; the criteria are formally discharged at freeze, not by
  this draft.

  Relative to the review draft published on 2026-07-25 (see below), this
  revision makes these changes:

  - It replaces the draft Status text with the three-question change policy
    (invalidate or change meaning → major; earlier readers cannot identify and
    cleanly refuse → major; obligations or registry assignment → minor under
    three conditions; otherwise erratum), and states the axis-independence
    principle and the worked classifications.
  - It rewrites Section 18's preamble, which described the document as a draft
    and permitted only errata after freeze.
  - It rewrites Section 8.1.1 as the schema-minor registry (Defined-by and
    wire meanings: 2 = `object_id` optional, 3 = `object_id` required) and
    corrects the stale `1 / 3` pair in Section 2.5.
  - It states the Reader obligation at `schema_minor` ≥ 3 in Section 8.2.1 and
    the reader rule for unrecognised `flags` bits, both matching the reference
    implementation.
  - It adds the semantics-freeze rule to Section 5.3.
  - It re-anchors the key-30 overflow-carrier prohibition from the document
    version to the registry mechanism.
  - It declares the Section 8.4 discovery-candidate set frozen for the life of
    `schema_major` 1.
  - It corrects the parity_map footer magic in Sections 2.5, 5.2 and 10.3,
    which described a `PARITY_MAP_FOOTER_MAGIC_LABEL` that appears in no
    implementation and on no tape — the footer shares the header's magic.
  - It corrects the minimum sidecar block size from 0xC0 to 0xD0 (a
    generation-1 value, here and below; draft.4 raised it to 0xE0) in
    Sections 2.5, 9.1 and 9.4, and adds the index-packing rejection the
    Section 9.4 pseudocode omitted.
  - It removes an unimplementable final-directory redundancy requirement from
    Section 8.3 that contradicted Sections 10.1, 10.7 and 11.3.
  - It records in Section 12.4 the `ParityMapReference`-locator precedence
    tier and the two-candidate requirement for structural discovery, both of
    which the reference Scanner already applied.

  **If you implemented from the review draft, re-check two things.**

  - Sections 2.5, 5.2 and 10.3 described a `PARITY_MAP_FOOTER_MAGIC_LABEL`
    (`"REM\0PMAPFOOT\x01"`) as the derivation for the `parity_map` footer
    locator's magic. No such label exists: the footer carries the *header's*
    magic, `HMAC(tape_uuid, PARITY_MAP_MAGIC_LABEL)[0..8]`, on every published
    artifact and in every implementation, and it is distinguished from a
    header by its position and by `copy_kind`. An implementation built from
    the draft computes eight wrong bytes and rejects every conformant
    `parity_map` tape file, so this correction restores interoperability
    rather than removing it.
  - The minimum sidecar block size is 0xD0, not 0xC0: sizes 0xC0 through 0xCF
    satisfy the header-plus-CRC floor but cannot pack even one index entry,
    and Section 9.4's algorithm now carries the rejection that makes this
    explicit.

  No tape byte and no published vector changed for either correction.

  **Implementation conformance.** An audit of this document against the
  reference implementation, the pinned vectors, the repository documentation
  and the project website found divergences between the implementation and
  this text. Each was resolved by correcting the implementation, and this text
  was not weakened. The corrections bring the implementation to these readings
  of the text:

  - A Scanner does not abort a whole catalog-less walk when one tape file's
    recorded block count disagrees with its measured length (Section 12.3).
  - A sidecar footer that parses but contradicts the map entry is treated as
    an invalid footer, and recovery falls back to the primary header rather
    than stopping (Section 13.3 step 2).
  - An epoch is metadata-unavailable only when no header/index copy can be
    validated, directory-assisted tail rescue included (Section 13.3 steps 3
    and 4).
  - Geometry and ordinal-range disagreements raise `SchemeMismatch`, as this
    document specifies, rather than a generic parse error.
  - The REM-OBJECT manifest depth bound applies through map values as well as
    arrays.

  Two further behaviours were examined and judged conformant, on these
  readings of the text:

  - Section 12.5's guarantee, that damage to one sidecar's metadata does not
    degrade recovery of any other epoch, concerns what can be recovered. A
    request that covers several epochs may fail as a whole when one of them is
    metadata-unavailable, provided each other epoch can still be recovered on
    its own.
  - Section 8.2's requirement that a parity bootstrap's `scheme_id` be
    `rs-cauchy-gf256-v1` binds what a conformant Writer may record. A Reader
    meets its obligation by refusing an unrecognised value legibly; it may do
    so when the scheme is first used, with `SchemeMismatch`, naming the scheme
    found and the scheme expected.

- **2026-07-29 — [draft] pre-freeze revisions II.**

  - Last-resort filemark-walk operational envelope specified (Section 8.4.1),
    with inter-file positioning exempted from the Section 8.4 rule-6 abort.
  - Bootstrap re-typing promoted from SHOULD to MUST with operational
    candidate criteria (Section 12.4).
  - Appendix C items 3 and 4 resolved.

  The first two are reader-obligation changes with no effect on the set of
  valid tapes.
- **2026-07-25 — pre-release copy.** A copy of this document, marked "Draft
  for review" and dated 2026-06-11, was distributed inside software release
  v1.0.0 (Zenodo 10.5281/zenodo.21551571, a *software* record). It was not
  deposited or citable as a document, carried no DOI of its own, and was
  reachable only by unpacking the source archive. It named Section 18 as the
  criteria still to be met. The vector archive current at
  that time was `b9be8760…`; the archive pinned by the first published
  revision is `77be73e7…`, which adds the REM-OBJECT object-row vectors and
  the independent re-derivation tool without altering any pre-existing
  member. The never-re-pin rule of the Status section binds published
  revisions and does not reach back into the draft era.
- **2026-07-22 — [draft] tape-alone recovery claims.**

  - Ordered persistence and the synchronizing barrier made normative on the
    tape I/O layer (Section 3.5).
  - Commit discipline extended with batched deferred synchronization and
    staged-record semantics (Sections 3.4, 11.1).
  - The attested prefix and the bare-tape tail taxonomy specified with salvage
    rules (Section 12.6).
  - Appendix B.8 reframed to per-file-marker rationale plus barrier-grain
    structural attestation.
- **2026-07-21 — [draft] pre-freeze revisions.**

  - Writer-legal block sizes closed over the discovery-candidate set
    (Section 8.4).
  - Object identity row keys clarified (Section 8.2.1).
  - Epochs redefined as explicit ordinal ranges with bare-counter ids, and
    short epochs legalized at any checkpoint boundary with
    `FINAL_PARTIAL_EPOCH` reserved for terminal `finish()` (Sections 3.3,
    10.5, 11.2).
  - The bootstrap directory ceiling made an admission-time refusal with
    mandatory headroom and seal-at-ceiling (Section 8.2.1).
  - Reference journal watermark note (Appendix B.12).
- **2026-06-11 — [draft] first draft.** Initial working baseline.
