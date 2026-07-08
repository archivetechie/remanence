### [CRITICAL] parity/implementability — rem-parity-1.0-specification.md §17 (damage matrix) vs 12.3/12.4 and 1.1 goal 5 (l.1513) 
ISSUE: The specified Scanner algorithm cannot deliver the outcome the spec asserts for a damaged bootstrap block: per 12.3 an unreadable-head 1-block file MUST be classified as an object candidate, the 12.4 overlay re-types only sidecars, so a destroyed checkpoint (or BOT) bootstrap becomes an object entry, shifts all subsequent ordinals, and makes digest validation fail for every scope covering that file — whole-map failure from single-block damage, contradicting goal 5 (line 101 'single-block damage to any one of them … never makes an unrelated epoch unrecoverable') and the Section 17 damage-matrix assertion 'never whole-tape failure'. No reconciliation step is specified; one implementer will reject the tape while another invents an unspecified re-typing search.
QUOTE: one bootstrap copy — each asserting the specified outcome (recovered / copy-health downgrade / one-epoch unavailability), never whole-tape failure
FIX: Specify the reconciliation algorithm (e.g., on digest mismatch, a Scanner MUST re-hypothesize kind=Bootstrap for unclassified 1-block files, renumber ordinals, and revalidate before declaring FilemarkMapDigestMismatch), or narrow goal 5, Section 12.5, and the Section 17 'one bootstrap copy' damage-matrix row to the outcomes the algorithms actually guarantee (e.g., damage to a non-authoritative bootstrap copy only).

### [CRITICAL] parity/implementability — rem-parity-1.0-specification.md §8.1 vs 16.2 (l.700) 
ISSUE: Self-contradiction on the bootstrap's trailing fill: the 8.1 frame table says readers ignore the zero fill, but 16.2 says 'reserved fields and declared zero-fill MUST be verified zero (misuse of reserved space is nonconformance…)'. Two careful implementers accept different inputs: one rejects a bootstrap with nonzero trailing bytes, the other accepts it. Every other structure (9.2, 9.3, 9.6, 10.2) says fill MUST be zero; only the bootstrap says 'readers ignore'.
QUOTE: | … | | zero fill to block end | | readers ignore |
FIX: Pick one rule and state it in 8.1: either change the row to 'MUST be zero; readers MUST verify' (consistent with 16.2), or carve out the bootstrap fill explicitly in 16.2 with the rationale for the exception.

### [CRITICAL] parity/implementability — rem-parity-1.0-specification.md §8.2.1 (l.742) 
ISSUE: `manifest_first_chunk_lba` (key 10) is never defined in-document, and its name collides with Section 3.2, where 'Physical LBA' is defined as a tape-absolute address. The row validity rule 'the manifest chunk range MUST fit within stored_block_count' implies an object-relative block index instead. Two careful implementers of key 30 write different bytes (tape-absolute LBA vs zero-based block index within the object tape file), and validators enforcing the fit rule will reject one of them. 'Chunk' vs tape block equivalence also rests only on the informative RAO binding in 4.4.
QUOTE: | 10 | uint | plaintext only | `manifest_first_chunk_lba` |
FIX: Define key 10 normatively in Section 8.2.1, e.g. 'the zero-based block index, within the object tape file, of the first manifest chunk (u64)' (or whatever the settled semantics is), and rename or footnote the 'lba' suffix so it cannot be read against Section 3.2's tape-absolute LBA definition; state explicitly that one RAO chunk equals one tape block for this row.

### [CRITICAL] parity/standalone — rem-parity-1.0-specification.md §17. Test Vectors (l.1468) 
ISSUE: The distribution channel for the conformance test vectors is a repository-relative path in the author's private repo; an implementer holding only the published document cannot obtain the fixture set that Sections 17 and 18 make load-bearing for conformance.
QUOTE: Static vectors live in the repository (`fixtures/rem-parity-1/`), each with
FIX: Replace with standalone distribution language: "Static test vectors are distributed with this specification as a companion archive (the REM-PARITY 1.0 test-vector set); each vector includes a manifest recording inputs, the expected values, and — for negative vectors — the expected Section 15 error name." State where the archive is published (same channel as the specification) and bind its integrity, e.g. by printing the archive's SHA-256 in this section.

### [CRITICAL] rao/consistency — rao-1.0-specification.md §4.4.1 (l.517) 
ISSUE: The uniqueness claim for the self-referential pax record length is mathematically false, and the mandated computation ('fixed-point iteration over its own digit count') does not specify a starting point, so two conformant writers can emit different bytes. Counterexample: for base length b = 8 (record content excluding the length digits, e.g. 'size=1'), both len=9 ('9 size=1\n', 9 bytes) and len=10 ('10 size=1\n', 10 bytes) satisfy every §4.4.1 constraint (len = total record length, newline at offset len−1). Double fixed points exist exactly at b ∈ {8, 97, 996, 9995, …} (b = 10^d − 1 − d). Iterating len ← b + digits(len) from len=b converges to 9, but iterating down from a large guess converges to 10. This breaks design goal 6 (§1.1: 'every conformant writer produces the identical byte stream'), infects the §4.6.3 pad-sizing algorithm ('solving each candidate record's self-referential length per Section 4.4.1'), and the §13.1 mandated one-byte-file vector ('size=1', b=8) sits directly in the ambiguous case — an implementer trusting 'the value is unique' has a coin-flip chance of producing wrong bytes.
QUOTE: the value is unique; crossing a decimal-digit boundary — 9→10, 99→100, 999→1000 — changes `<len>` by one and the iteration converges
FIX: Replace the parenthetical with a rule that pins the choice, e.g.: 'When two self-consistent lengths exist (base length 8, 97, 996, 9995, …), writers MUST emit the smaller; equivalently, iterate len ← base + digits(len) starting from len = base, which converges to the smallest fixed point.' Confirm the chosen rule against the reference implementation's actual output before pinning the one-byte-file vector.

### [CRITICAL] rao/implementability — rao-1.0-specification.md §4.3.1 (l.428) 
ISSUE: The writer-normative header table gives no `mode` value for typeflag `g` and `x` records, so two careful implementers produce different bytes (e.g. one emits `0000644\0` following common tar practice, another `0000000\0`), yielding different `plaintext_digest` values for identical inputs and failing the pinned full-stream vectors — despite design goal 6 requiring identical byte streams.
QUOTE: | 100 | 8 | `mode` | Regular entries: `0000644\0`, or `0000755\0` when `REMANENCE.executable` is `true`; hardlinks: `0000644\0`; symlinks: `0000777\0`; directories: `0000755\0` |
FIX: Add explicit writer-normative `mode` values for the `g` and `x` header records to the Section 4.3.1 table (e.g. 'pax header records (`g`, `x`): `0000644\0`').

### [CRITICAL] rao/implementability — rao-1.0-specification.md §4.3.1 / 4.6.1 (l.435) 
ISSUE: For a symlink/hardlink target that does not fit in the 100-byte ustar `linkname`, 4.3.1 permits either 'all NUL or the pax-backed placeholder' while 4.6.1 (line 636) mandates 'put a placeholder in `linkname`' without ever defining the placeholder value (PAX_PATH_PLACEHOLDER is defined only for `name`); reading A writes all-NUL linkname, reading B writes `remanence/pax-path` (or any string) — different bytes for identical inputs, and no 13.1 vector exercises a >100-byte target to pin it.
QUOTE: otherwise all NUL or the pax-backed placeholder (Section 4.6.1)
FIX: Pick one rule: define the linkname placeholder as a named constant (or mandate all-NUL), state it as a single MUST in 4.3.1, make 4.6.1 agree, and add a >100-byte link-target test vector to Section 13.1.

### [CRITICAL] rao/standalone — rao-1.0-specification.md §13 / 14 / Appendix C (l.2214) 
ISSUE: The conformance anchor is a private repository path: Section 13 says vectors 'live in the repository (fixtures/rao/)', pinned values are 'produced by the reference implementation' (line 2220), Section 14 criterion 2 requires fixtures to 'exist in-repo' (line 2394), and Appendix C item 1 says values are 'frozen into fixtures/rao/' (line 2655) — an independent implementer cannot obtain the test vectors or the pinned cryptographic values from the document alone.
QUOTE: Static vectors live in the repository (`fixtures/rao/`), each with a manifest
FIX: Replace every repository reference with a publication-relative distribution statement, e.g. Section 13 opening: 'The static test vectors are distributed as a companion archive alongside this specification; each vector carries a manifest recording its inputs, the expected values pinned below, and — for negative vectors — the expected Section 11 error name.' Section 14 criterion 2: 'The Section 13 test vectors are published with this specification with every [pinned-at-generation] value generated, frozen, and passing…'. Before publication the pinned-at-generation values must actually exist in the distributed vectors (the document itself acknowledges this as a freeze criterion).

### [CRITICAL] rao/structure — rao-1.1-specification.md §whole document (missing Security Considerations) (l.89) 
ISSUE: No Security Considerations section, although 1.1 introduces a genuinely new security surface: restore-time reapplication of arbitrary extended attributes (setxattr), which on Linux includes privilege-bearing namespaces (security.capability grants file capabilities; trusted.* and security.* affect MAC/ima labels), applied from untrusted removable-media bytes; a Security Considerations section is mandatory in every RFC-style publication.
QUOTE: A 1.1 reader surfaces an entry's preserved xattrs and, on restore, reapplies them (`setxattr`).
FIX: Add a Security Considerations section: state that xattr values are untrusted input from the medium; require or strongly recommend restore-side namespace filtering (e.g. never reapply security.*/trusted.* without explicit operator opt-in); note interaction with RAO 1.0 Section 12.9/12.10 hostile-input and restore-hardening rules and explicitly inherit the rest of RAO 1.0 Section 12.

### [MAJOR] parity/bcp14 — rem-parity-1.0-specification.md §8.2.1 (l.759) 
ISSUE: The final-bootstrap clause imposes an unconditional MUST to carry key-30 rows, contradicting the Section 8.2 payload table (line 722) which declares key 30 OPTIONAL and the later conditioning on 'a Writer that implements RAO object rows' (line 766) — a self-contradiction about required wire content.
QUOTE: the final bootstrap MUST include one row for every committed object tape file
FIX: Condition the clause the same way as the checkpoint clause: 'and a final bootstrap that carries key 30 MUST include one row for every committed object tape file in the final digest scope', or open the paragraph with 'When a Writer implements key 30: ...'.

### [MAJOR] parity/bcp14 — rem-parity-1.0-specification.md §8.4 (l.789) 
ISSUE: The chapeau's MUST-attempt-everything-in-order conflicts with item 3 ('Heuristic fractional probing MAY be used', line 795) and item 5 ('an implementation SHOULD offer an explicit opt-in full filemark-walk scan', line 799) — the same procedure is simultaneously mandatory, optional, and recommended, and item 5 also shifts the subject from 'A Scanner' to 'an implementation'.
QUOTE: A Scanner with no off-tape state MUST attempt, in order, until a bootstrap is found
FIX: Restate the chapeau to scope the MUST to the mandatory strategies only, e.g.: 'A Scanner with no off-tape state MUST attempt strategies 1, 2, and 4 in the order listed until a bootstrap is found. Strategy 3 is OPTIONAL. A Scanner SHOULD additionally offer strategy 5 as an explicit opt-in.'

### [MAJOR] parity/bcp14 — rem-parity-1.0-specification.md §1.4 (l.152) 
ISSUE: A capitalized MUST appears in the Introduction before the Section 2.1 BCP 14 declaration, duplicating Section 11.4 with broader scope: 11.4 requires verification only 'before any parity write' and Sections 8.2/16.3 reject drive_compression=true only on a parity bootstrap (implying no-parity tapes may record true), while this blanket statement covers all REM-PARITY tapes including no-parity ones — same requirement, two different scopes.
QUOTE: **Drive hardware compression MUST be off** for REM-PARITY tapes
FIX: Demote to non-normative text scoped to parity tapes: 'Drive hardware compression is required to be off for parity-protected tapes (Section 11.4): block bytes must map 1:1 to media ...', leaving the sole normative statement in Section 11.4.

### [MAJOR] parity/bcp14 — rem-parity-1.0-specification.md §8.3 (l.783) 
ISSUE: Three MUSTs bind checkpoint-policy parameters the same sentence declares 'operator-tunable and not normative'; the policy structure (floors, taper fractions, divisors) is defined nowhere in the document and has no on-tape reflection, so no implementation of the format could be conformance-tested against these requirements.
QUOTE: not normative; their *validity* rules are: floors MUST be non-zero; taper fractions MUST lie in (0, 1]
FIX: Delete the validity rules or restate as implementation guidance ('A Writer that implements a taper policy of this shape should validate that floors are non-zero and taper fractions lie in (0, 1], descending, with strictly increasing divisors'); alternatively define the policy structure normatively before constraining it.

### [MAJOR] parity/bcp14 — rem-parity-1.0-specification.md §15 (l.1406) 
ISSUE: A security-critical prohibition is carried by lowercase 'may' in a normative section; under the document's own BCP 14 boilerplate ('when, and only when, they appear in all capitals') lowercase words are non-normative, so the no-panic/no-crash rule has no binding force.
QUOTE: No code path reachable from tape bytes may panic, crash, or allocate unboundedly
FIX: Rewrite with the keyword: 'Code paths reachable from tape bytes MUST NOT panic, crash, or allocate unboundedly: every length that drives an allocation MUST be cross-checked ...'.

### [MAJOR] parity/bcp14 — rem-parity-1.0-specification.md §8.4 (l.805) 
ISSUE: Lowercase 'must' in a normative section imposes a requirement on 'Deployments', which is not a conformance role and is out of scope — the reader cannot tell whether this is normative, nor which testable party it binds.
QUOTE: Deployments using other block sizes must supply the size as a hint.
FIX: Rebind to the Scanner or restate as consequence: 'Block sizes outside the candidate set are not discoverable without an out-of-band hint; a Scanner MUST accept an operator-supplied block-size hint and treat it as a configured read size.'

### [MAJOR] parity/bcp14 — rem-parity-1.0-specification.md §8.2 (l.716) 
ISSUE: The condition 'minimal no-parity' is undefined (key 1 says only 'REQUIRED unless no-parity'), so an implementer cannot determine whether a non-minimal no-parity bootstrap is required to carry a digest record — an ambiguous conditional REQUIRED in the wire-format table.
QUOTE: REQUIRED unless minimal no-parity
FIX: Use one defined condition for both rows, e.g. 'REQUIRED on parity bootstraps; OPTIONAL on no-parity bootstraps (see the no-parity rules below)', and delete 'minimal'.

### [MAJOR] parity/bcp14 — rem-parity-1.0-specification.md §8.2 (l.719) 
ISSUE: Key 5's Presence cell abandons the REQUIRED/OPTIONAL vocabulary used by every other row, so whether omitting drive_compression is a Writer conformance violation is undecidable — even though the Section 16.3 self-incrimination defense depends on the key being present.
QUOTE: written always; absent ⇒ false
FIX: Replace with keyword language: 'REQUIRED (a Writer MUST write key 5; readers MUST treat absence as false)'.

### [MAJOR] parity/consistency — rem-parity-1.0-specification.md §11.1 vs 3.4 (l.1142) 
ISSUE: The Section 11.1 cycle emits queued sidecars — 'each its own full cycle', i.e. each ending in its own off-tape commit record — BEFORE the closing object's commit record, so sidecar file N+1 begins and commits while object file N is still uncommitted; this directly contradicts Section 3.4 (line ~333): 'Tape files begin and commit strictly sequentially (next = last committed + 1, first = 0, at most one in flight)', and it makes Section 14 step 1's committed-prefix derivation ill-defined after a crash in the emission window (commit records exist for N+1..N+j but not N, so the 'prefix' is non-contiguous).
QUOTE: → [object close only] emit queued sidecars (each its own full cycle)
→ off-tape commit record   (THE commit point, Section 3.4)
FIX: Resolve the ordering explicitly: either move the object's off-tape commit record before the '[object close only] emit queued sidecars' step, or add an explicit exception to Section 3.4's strictly-sequential rule for the object-close bundle and state in Section 14 step 1 how the committed prefix is derived when an object's record is absent but later sidecar records exist (e.g. the prefix ends before the uncommitted object and all later records are discarded). Also note that the placement of 'durable-boundary advance' two steps before 'THE commit point' needs a clarifying sentence (in-memory boundary vs durable commit).

### [MAJOR] parity/consistency — rem-parity-1.0-specification.md §14 (step 3 vs step 2) (l.1353) 
ISSUE: This normative sentence is unreachable given step 2, enforced immediately above: step 2 requires 'T − W < S × k' (strict) AND 'W MUST be epoch-aligned', so [W, T) is a strict prefix of a single epoch and can never contain a complete epoch — an implementer cannot tell whether the strict bound is wrong (and multiple/complete open epochs are legal in some crash state) or the sentence is dead text.
QUOTE: Any *complete* epochs encountered in `[W, T)` are re-encoded as rebuilt sidecars.
FIX: Delete the sentence, or if a crash state with T − W ≥ S × k is actually intended to be resumable (see the Section 11.1/3.4 finding), relax step 2 accordingly (e.g. '≤' or 'at most j open epochs') and keep the sentence; the two steps must agree either way.

### [MAJOR] parity/editorial — rem-parity-1.0-specification.md §17 (l.1468) 
ISSUE: A standalone international specification directs implementers to "the repository" via a repository-relative path that a reader of the published document cannot resolve.
QUOTE: Static vectors live in the repository (`fixtures/rem-parity-1/`)
FIX: Publish the static vectors as an appendix of this document or as a separately citable companion artifact with a stable public locator, and reword to e.g. "Static test vectors are published in the companion test-vector distribution [REM-PARITY-VECTORS]". Apply the same fix to Appendix C item 1 (line 1759), which repeats the `fixtures/rem-parity-1/` path.

### [MAJOR] parity/editorial — rem-parity-1.0-specification.md §19.2 (l.1562) 
ISSUE: The [RAO] reference entry cites the companion specification by a repository-relative file path instead of a citable publication locator, so an external implementer cannot retrieve it.
QUOTE: (`specs/rao-1.0-specification.md`)
FIX: Replace the path with a proper citation: title, version, date, and a stable public URL or document identifier for the RAO specification (e.g. "[RAO] Rem Archive Object (RAO) Format, Version 1.0, <stable locator>, June 2026").

### [MAJOR] parity/editorial — rem-parity-1.0-specification.md §8.4 (l.809) 
ISSUE: The per-block scan rules — normative reader behavior — are written as an arrow-shorthand chain with colloquial, undefined outcomes ("keep scanning", "abort everything"), leaving the scope of the abort action unspecified and the notation unconvertible to RFC prose.
QUOTE: any other transport error → abort discovery; `drive_compression = true` on a parity bootstrap → abort everything
FIX: Convert the chain to a numbered list with one condition per item and explicit BCP 14 actions, e.g. "If the magic does not match, the Scanner MUST advance to the next block. ... If `drive_compression` is true on a parity bootstrap, the implementation MUST abort discovery and reject the tape (Section 11.4)." Apply the same list conversion to the Section 8.1 parse-order chain (line 707) and the Section 12.4 overlay-priority chain (line 1233).

### [MAJOR] parity/editorial — rem-parity-1.0-specification.md §8.4 (l.789) 
ISSUE: The lead-in imposes a blanket MUST over a list whose own items are keyed MAY (item 3), keyword-less (item 4), and SHOULD (item 5), making it undecidable which discovery strategies are actually mandatory.
QUOTE: MUST attempt, in order, until a bootstrap is found
FIX: Reword the lead-in descriptively ("A Scanner with no off-tape state attempts the following strategies in order, stopping when a bootstrap is found:") and give each item its own requirement keyword: items 1 and 4 MUST, item 2 SHOULD (when hints exist), item 3 MAY, item 5 SHOULD.

### [MAJOR] parity/editorial — rem-parity-1.0-specification.md §13.5 (l.1327) 
ISSUE: Bold typography is used to carry or amplify normative requirements; RFC publication formats render no bold in the plain-text output, so the emphasis is lost and requirement force must not depend on it.
QUOTE: **Every reconstructed data block MUST be verified against its sidecar data CRC before release.**
FIX: Remove bold from all sentences containing requirement keywords, letting the BCP 14 keyword carry the force; also at Section 1.4 ("**Drive hardware compression MUST be off**", line 152), the Section 8.4 medium-error rule (line 809), Section 9.1 (line 845), and Section 14 steps 3-5 (lines 1350, 1356, 1366). Reserve bold for first definitions of terms.

### [MAJOR] parity/editorial — rem-parity-1.0-specification.md §3.2 (l.278) 
ISSUE: Normative formulas and rules rely throughout on non-ASCII symbols (Σ, ×, ⊗, ≤, ≥, ≠, ⁸, →, ⇒, ·, ✓) that RFC publication formats restrict or garble in the plain-text rendering.
QUOTE: LBA(f, b) = Σ_{g<f}(block_count(g) + 1) + b
FIX: Recast formulas in ASCII ("SUM over g < f of (block_count(g) + 1)", "<=", ">=", "!=", "*", "2^8", "implies") or add a notation subsection to Section 2.4 defining an ASCII-safe convention; remove the decorative check mark at line 1587.

### [MAJOR] parity/editorial — rem-parity-1.0-specification.md §19.1 (l.1551) 
ISSUE: Reference entries are shorthand rather than full citations (no authors, titles, series numbers, dates, or DOIs), and RFC 3339 — invoked normatively in the Section 8.2 key-4 row (line 718) — does not appear in the references at all.
QUOTE: [RFC2119] / [RFC8174] — BCP 14 requirement keywords.
FIX: Format every entry per RFC 7322 citation style (e.g. "Bradner, S., \"Key words for use in RFCs to Indicate Requirement Levels\", BCP 14, RFC 2119, DOI ..., March 1997.") and add an entry for RFC 3339.

### [MAJOR] parity/editorial — rem-parity-1.0-specification.md §Front matter (l.7) 
ISSUE: The document opens with a headerless two-column Markdown layout table holding status/version/magic metadata — a construct with no equivalent in the RFC document skeleton, which requires structured front matter instead.
QUOTE: | Status | Draft for review |
FIX: Replace the pseudo-table with the customary RFC front matter (document name, intended status, date, abstract placement) and move wire constants such as the bootstrap magic solely into the Section 2.5 constants table, where they already appear.

### [MAJOR] parity/implementability — rem-parity-1.0-specification.md §11.1 vs 3.4 (l.1142) 
ISSUE: The per-file commit cycle contradicts Section 3.4's sequencing rule: the cycle places 'emit queued sidecars (each its own full cycle)' — each ending in its own off-tape commit record — before the object's 'off-tape commit record (THE commit point)', so sidecar tape files (higher numbers) begin and commit while the lower-numbered object file is uncommitted, violating 'Tape files begin and commit strictly sequentially (next = last committed + 1 … at most one in flight)' (line 322). The 'durable-boundary advance' step preceding the commit record is also undefined relative to 3.4, where the boundary IS the commit record. The two readings leave different crash states: object-committed-first can leave a committed prefix with T − W ≥ S × k, which Section 14 step 2 then rejects as ResumeAppend.
QUOTE: → [object close only] emit queued sidecars (each its own full cycle)
→ off-tape commit record   (THE commit point, Section 3.4)
FIX: State the intended ordering unambiguously (e.g., the object's commit record is written at boundary advance, before sidecar cycles begin, and the 11.2 bounded-restart rule is a post-bundle invariant only; or define the close-and-emit bundle as one atomic commit) and amend 3.4 to match, including what 'durable-boundary advance' means as a distinct step.

### [MAJOR] parity/implementability — rem-parity-1.0-specification.md §8.2.1 vs 19.2 (l.747) 
ISSUE: Section 8.2.1 imposes MUST-level requirements on fields whose meaning exists only in [RAO] ('RAO encrypted-header key_id', 'metadata_frame_len', the manifest anchors), but [RAO] is listed as an informative reference — an implementer of key 30 cannot validate rows from this document plus normative references alone (RFC 7322/BCP normative-reference discipline).
QUOTE: | 20 | bytes .size 16 | encrypted only | RAO encrypted-header `key_id`; all-zero is invalid |
FIX: Either move [RAO] to the normative references with a condition ('normative only for implementations of bootstrap key 30'), or define the key-30 field semantics and bounds self-containedly in 8.2.1 so [RAO] can stay informative.

### [MAJOR] parity/implementability — rem-parity-1.0-specification.md §8.2 (l.716) 
ISSUE: The digest record's presence rule 'REQUIRED unless minimal no-parity' uses the term 'minimal no-parity', which is never defined; key 1 says 'REQUIRED unless no-parity'. It is undecidable whether a non-'minimal' no-parity bootstrap must carry a digest record, so writer conformance and validator strictness diverge.
QUOTE: | 2 | map | REQUIRED unless minimal no-parity | digest record: …
FIX: Define the term or reword, e.g. 'REQUIRED on parity bootstraps; OPTIONAL on no-parity bootstraps (a no-parity bootstrap MAY omit everything except the fixed frame)', matching the prose below the table.

### [MAJOR] parity/implementability — rem-parity-1.0-specification.md §8.3 (l.783) 
ISSUE: Normative validity rules are stated over a policy parameter structure that is never defined: 'taper fractions MUST lie in (0, 1], be descending, and have strictly increasing divisors' — nothing in the document says what an end-of-medium taper fraction or its 'divisor' is; only the author's implementation gives these words meaning.
QUOTE: floors MUST be non-zero; taper fractions MUST lie in (0, 1], be descending, and have strictly increasing divisors
FIX: Either define the taper parameter model (list of (fraction, divisor) pairs and how they gate checkpoint placement) in enough detail to check the MUSTs, or drop the normative validity sentence entirely since the policy itself is declared non-normative.

### [MAJOR] parity/implementability — rem-parity-1.0-specification.md §17 (l.1468) 
ISSUE: The test-vector corpus is located by a repository-relative path ('fixtures/rem-parity-1/') available only in the author's codebase; an external implementer of this 'standalone' spec cannot obtain the image-level vectors, the negative vectors, or the manifests naming Section 15 errors. (Appendix C acknowledges the images are pending, but the published document must point at a public artifact.)
QUOTE: Static vectors live in the repository (`fixtures/rem-parity-1/`), each with a manifest
FIX: Replace the repo path with a citable public artifact (a versioned archive with a stated SHA-256, or an appendix embedding the minimal image in hex) and state that the fixture archive is published alongside the specification.

### [MAJOR] parity/implementability — rem-parity-1.0-specification.md §12.3 (l.1216) 
ISSUE: Rung-failure semantics are inconsistent: item 2 (parity_map) says a measured/declared block-count mismatch 'is a hard error', but item 3 (sidecar) states the same MUST with no disposition — implementations diverge between hard-erroring the scan and falling through to the footer probe / object-by-elimination, i.e. they accept different damaged tapes and emit different errors.
QUOTE: the measured block count MUST equal the header's `sidecar_total_block_count`.
FIX: State the disposition for every rung explicitly, e.g. 'a primary sidecar header whose sidecar_total_block_count disagrees with the measured length is a hard error / causes fall-through to step 4' — one choice, written down.

### [MAJOR] parity/implementability — rem-parity-1.0-specification.md §12.4 (l.1240) 
ISSUE: Overlay behavior is specified for a directory entry conflicting with a scanned bootstrap/parity_map (hard error) but not for the converse gap: a tape file the walk structurally classified as a sidecar that is absent from the authoritative directory within its scope. Implementations will diverge between hard error, keeping the scan classification, and demoting the file — each yielding different ordinals and digest outcomes.
QUOTE: a directory entry conflicting with a *scanned* bootstrap or parity_map classification is a hard error
FIX: Add one sentence: e.g. 'a tape file classified as a sidecar by the walk but absent from the directory within the directory's scope is a hard error (FilemarkMapReconstruct)' — or whichever disposition is intended.

### [MAJOR] parity/standalone — rem-parity-1.0-specification.md §Front matter (l.13) 
ISSUE: The front-matter table cites the reference implementation by a repository-relative crate path meaningful only inside the author's repo.
QUOTE: | Reference implementation | Remanence (`crates/remanence-parity`) |
FIX: Drop the path: "| Reference implementation | Remanence |" — or better, remove the row from the front matter entirely and add an RFC 7942-style "Implementation Status" section (explicitly marked to be removed at final publication) that names Remanence as the originating implementation and its coverage of the five roles. A published spec's front matter should carry only document identity (status, version, date, magics/identifiers).

### [MAJOR] parity/standalone — rem-parity-1.0-specification.md §Front matter (editorial blockquote) (l.15) 
ISSUE: An internal process artifact sits on page one: a remove-at-freeze editorial note referencing "an internal conformance backlog" that no external reader can see or act on.
QUOTE: **Editorial note (remove at freeze):** ... known spec-over-implementation gaps are tracked in an internal conformance backlog
FIX: Delete the blockquote before publication. Its one durable idea — "this document is the normative fixed point; an implementation is validated against it, not the reverse" — belongs as the opening sentence of Section 18. The pinned-at-generation caveat it repeats is already stated in Section 17 and should be resolved there (see the Section 17 pinned-at-generation finding).

### [MAJOR] parity/standalone — rem-parity-1.0-specification.md §Appendix C. Open Items Before Freeze (l.1774) 
ISSUE: Appendix C is an internal work tracker, not specification content: it carries an internal milestone code with an owner ("Owner milestone: PAR-KEY30-RECOVERY"), a repo path ("frozen into `fixtures/rem-parity-1/`", line 1759), a narration of the author's implementation's specific deficiency ("implementations that classify filemarks from fixed-format sense only must close the gap before freeze", line 1763), and engineering-roadmap jargon ("kernels must land", line 1768).
QUOTE: Owner milestone: PAR-KEY30-RECOVERY before format freeze.
FIX: Remove Appendix C at publication and keep pre-freeze tracking in a separate, unpublished process document. Fold the one reader-relevant item (C.3) into Section 8.4 as: "The operational parameters of the full-tape filemark-walk scan (geometry hints, abort conditions, progress reporting) are implementation-defined." If the appendix must survive the draft stage, strip milestone codes, owners, repo paths, and implementation-specific gap narration.

### [MAJOR] parity/standalone — rem-parity-1.0-specification.md §18. Conformance and Freeze Criteria (l.1523) 
ISSUE: The conformance section conditions the specification's maturity on the author's private artifacts: a "conformance backlog" (criterion 1) no reader can inspect, and fixtures that "exist in-repo and pass" (criterion 2) in a repository the reader cannot access — a published spec cannot define conformance or maturity in terms of things only the author holds.
QUOTE: A reference implementation implements this document in every role ... with its conformance backlog against this document closed.
FIX: Split the section. (a) "Conformance" (published, normative): "An implementation is conformant in a role if it satisfies every MUST/MUST NOT requirement of the sections defining that role and reproduces the Section 17 test vectors." No named implementation. (b) The freeze/maturity criteria move to an RFC 7942-style "Implementation Status" section marked "to be removed before publication", rephrased implementation-neutrally: criterion 1 → "at least one implementation covers every role with no known deviations from this document"; criterion 2 → "the Section 17 vectors are published alongside this specification and pass". Retain the errata sentence (lines 1519–1521) as a permanent "Versioning and Errata" paragraph — it is good published-spec language.

### [MAJOR] parity/standalone — rem-parity-1.0-specification.md §8.3. Placement (Writer) (l.783) 
ISSUE: MUST-level validity rules are imposed on a checkpoint-policy configuration structure ("floors", "taper fractions", "divisors") that the document never defines — this is the author's implementation's config schema; a stranger implementing from scratch cannot act on it.
QUOTE: floors MUST be non-zero; taper fractions MUST lie in (0, 1], be descending, and have strictly increasing divisors
FIX: Either define the policy parameters (an informative table naming byte floor, object-count floor, taper fraction/divisor pairs, minimum separation, and what each means) so the validity rules are actionable, or delete the validity rules and state only the wire-observable constraints: "Checkpoint placement policy is writer-defined and not normative. This format constrains only its observable consequences: checkpoint bootstraps MUST be written only at object boundaries, and sequence numbers MUST strictly increase across all bootstrap copies on one tape."

### [MAJOR] parity/standalone — rem-parity-1.0-specification.md §19.2. Informative References (l.1562) 
ISSUE: The companion RAO specification is cited by a repository-relative path, and it is classified Informative even though Section 8.2.1 places normative MUSTs on RAO-defined fields (key_id, metadata_frame_len, the four manifest anchors) whose semantics are unobtainable from this document alone.
QUOTE: (`specs/rao-1.0-specification.md`): the reference payload format
FIX: Cite it as an independent publication: "[RAO] Rem Archive Object (RAO) Format, Version 1.0, <date>, published alongside this specification at <stable URL or identifier>." Move [RAO] to Normative References, scoped with a note that it is required only by implementations of the optional bootstrap key 30 (Section 8.2.1); alternatively state in 8.2.1 that the semantics of row keys 10–13 and 20–21 are defined by [RAO] and cite it normatively there.

### [MAJOR] parity/structure — rem-parity-1.0-specification.md §Front matter (l.7) 
ISSUE: There is no RFC-style "Status of This Memo" statement: the document never states its intended maturity/stream, what "Draft for review" means to an external reader, how it can change, or where the authoritative copy lives; the only maturity discussion is an editorial note marked "remove at freeze" that cites an unpublished internal backlog.
QUOTE: | Status | Draft for review |
FIX: Add a "Status of This Document" section immediately after the title block stating the document's maturity (draft), the change policy (Section 18's freeze rule can be summarized here), the versioning scheme, and the canonical retrieval location; delete the parenthetical dependence on the internal editorial note before publication.

### [MAJOR] parity/structure — rem-parity-1.0-specification.md §19. References (l.1547) 
ISSUE: There is no IANA Considerations section (customary even when empty), and this format actually defines extensible registries in all but name — the scheme identifier `rs-cauchy-gf256-v1`, the parity_map format identifier, tape-file kind codes 0–3, bootstrap CBOR keys (with keys explicitly reserved for future minor versions), and sidecar directory flag bits — with no statement of who assigns future values.
QUOTE: ## 19. References
FIX: Add a numbered "IANA Considerations" section before References; either state "This document has no IANA actions" and explicitly declare that scheme identifiers, CBOR map keys, kind codes, and flag bits are assigned solely by this specification's revision process, or define the registration policy for each extensible namespace.

### [MAJOR] parity/structure — rem-parity-1.0-specification.md §End of document (l.1775) 
ISSUE: The document has no Authors' Addresses section — no author, affiliation, or contact point appears anywhere, so an implementer with a question or erratum has no addressee, which is below the bar for a standalone published specification.
QUOTE:    before format freeze.
FIX: Add an "Authors' Addresses" section (name, organization, email) after the appendices, and optionally an Acknowledgements section before it.

### [MAJOR] parity/structure — rem-parity-1.0-specification.md §19.1 (l.1551) 
ISSUE: Every entry in the References section is a one-line gloss rather than a full RFC 7322-style citation: no authors, no full titles for RFC 2119/8174, no dates, no DOIs/URLs, two anchors crammed into one entry, and [LTO-SCSI] gives only a vendor doc number (GA32-0928) with no title/edition/URL — a stranger cannot unambiguously retrieve several of these.
QUOTE: - [RFC2119] / [RFC8174] — BCP 14 requirement keywords.
FIX: Expand each entry to full citation form, e.g. "[RFC2119] Bradner, S., \"Key words for use in RFCs to Indicate Requirement Levels\", BCP 14, RFC 2119, DOI 10.17487/RFC2119, March 1997, <https://www.rfc-editor.org/info/rfc2119>." — one entry per anchor — and give [LTO-SCSI] its full title, edition, publisher, and retrieval URL.

### [MAJOR] parity/structure — rem-parity-1.0-specification.md §8.2 (l.718) 
ISSUE: RFC 3339 is invoked normatively to define the bootstrap key-4 timestamp syntax but has no citation anchor and does not appear in the References section at all; inconsistently, the equivalent parity_map field (key 7 `write_timestamp`, Section 10.4, line 1056) specifies no format whatsoever.
QUOTE: | 4 | tstr | OPTIONAL | RFC 3339 write timestamp |
FIX: Cite as "[RFC3339] timestamp" at key 4, add RFC 3339 (Klyne & Newman, "Date and Time on the Internet: Timestamps", RFC 3339, July 2002) to Normative References, and state in Section 10.4 that parity_map key 7 uses the same [RFC3339] format.

### [MAJOR] parity/structure — rem-parity-1.0-specification.md §2.4 (l.223) 
ISSUE: UTF-8 is declared as the normative encoding for on-tape text fields (scheme_id, format identifiers, version strings, timestamps) but RFC 3629 is neither cited here nor listed in the References.
QUOTE: identifiers, version strings, timestamps) are UTF-8.
FIX: Change to "are UTF-8 [RFC3629]" and add "[RFC3629] Yergeau, F., \"UTF-8, a transformation format of ISO 10646\", STD 63, RFC 3629, November 2003" to Normative References.

### [MAJOR] parity/structure — rem-parity-1.0-specification.md §3.5 (l.332) 
ISSUE: This MUST depends on external SCSI definitions (fixed/descriptor sense formats, sense key 0x03 in Section 8.4, READ POSITION) that are covered only by the informative [LTO-SCSI] entry — and the [LTO-SCSI] anchor is never actually cited anywhere in the body, making it an orphan reference; an implementer cannot satisfy the MUST from the normative reference set.
QUOTE: Boundary classification MUST work for both fixed-format and descriptor-format SCSI sense data.
FIX: Cite [LTO-SCSI] (or better, the T10 standards SPC-4/SSC-4, which define sense data formats vendor-neutrally) at Sections 3.5 and 8.4, and move the SCSI reference to Normative References since a normative requirement depends on it — or rewrite the requirement so the sense-format details are explicitly delegated to the I/O layer's own specification.

### [MAJOR] parity/structure — rem-parity-1.0-specification.md §19.2 (l.1562) 
ISSUE: The [RAO] citation locates the companion specification by a repository-relative file path, which is meaningless to any reader outside the author's repo and violates the standalone-document goal.
QUOTE: (`specs/rao-1.0-specification.md`)
FIX: Replace the path with a proper citation: full title, version, author/publisher, date, and a stable public retrieval URL or document identifier for the RAO 1.0 specification.

### [MAJOR] parity/structure — rem-parity-1.0-specification.md §17 (l.1468) 
ISSUE: Section 17 (normative: error names are "normative for the test-vector manifests") anchors the conformance vectors to a repository-relative path in the author's private codebase, which an external implementer cannot access.
QUOTE: Static vectors live in the repository (`fixtures/rem-parity-1/`)
FIX: Point to a public, versioned distribution location for the vector set (URL or archive identifier published alongside the specification), or state explicitly that the vector archive is distributed with the specification and name it; keep the in-document arithmetic vectors as the always-available baseline.

### [MAJOR] parity/structure — rem-parity-1.0-specification.md §Front matter (l.13) 
ISSUE: The metadata table cites the reference implementation by an internal repository crate path; "Remanence" as the implementation name is fine, but `crates/remanence-parity` is meaningless outside the author's repo.
QUOTE: | Reference implementation | Remanence (`crates/remanence-parity`) |
FIX: Replace with the implementation's public name and a public URL (or drop the row and describe the reference implementation in the Status/Introduction text).

### [MAJOR] parity/structure — rem-parity-1.0-specification.md §Appendix C (l.1774) 
ISSUE: Appendix C is the only appendix not marked "(Informative)" — in the ToC (line 72) and its heading — leaving its normative status ambiguous, and item 5 leaks an internal project-management milestone identifier and "Owner milestone" phrasing that has no meaning to an external reader.
QUOTE: Owner milestone: PAR-KEY30-RECOVERY
FIX: Mark Appendix C "(Informative)" in both the heading and the ToC (or state it will be deleted at freeze, matching the editorial note), and delete the "Owner milestone: PAR-KEY30-RECOVERY" sentence, restating item 5 as a neutral open item ("scanner/recovery-reader validation of key-30 rows is not yet specified/verified").

### [MAJOR] rao/bcp14 — rao-1.1-specification.md §1 (l.31) 
ISSUE: The central normative rule of the entire 1.1 delta — when a writer must emit schema_version 1.1 versus 1.0 — is stated purely declaratively, with no BCP 14 keyword, so an implementer cannot tell it is a binding writer requirement.
QUOTE: An object that preserves any xattr sets `REMANENCE.schema_version = 1.1`.
FIX: Restate with keywords bound to the Writer role: 'A Writer that emits a non-empty metadata_preservation_data on any entry MUST set REMANENCE.schema_version to 1.1; a Writer that preserves no xattrs MUST emit REMANENCE.schema_version 1.0 and an empty container (byte-stability with RAO 1.0).'

### [MAJOR] rao/bcp14 — rao-1.0-specification.md §7.3 (l.1760) 
ISSUE: A MUST is imposed on 'the deployment' — an entity Section 1.4 places above/outside the format and that no Section 2.2 conformance role covers — so no implementation of this format can be tested against it (the section itself concedes it is 'a deployment (workflow) obligation rather than a property of the bytes').
QUOTE: the
deployment MUST re-read the copy via the object read path and re-verify it
FIX: Remove BCP 14 force: either restate as deployment guidance ('deployments are expected to re-read...'), or bind the testable part to the named Verifier role ('a copy is discharged by a conformant Verifier per Section 7.4') and keep the when-to-run-it as non-normative operational guidance.

### [MAJOR] rao/bcp14 — rao-1.0-specification.md §6 (l.1624) 
ISSUE: Normative MUSTs bind 'a catalog-backed PFR index' and declare 'a catalog ... nonconformant', but Section 1.5 says the format 'defines no catalog format' and Section 1.4 assigns catalogs to out-of-scope orchestration; no conformance role of Section 2.2 owns a catalog, so the requirement is untestable as written. Same pattern at line 1613 ('PFR indexes MUST treat plaintext offsets ... as the source of truth').
QUOTE: A **catalog-backed** PFR index (per-file rows, not the full manifest) MUST
preserve the ability to resolve a hardlink
FIX: Rebind to the named Restorer role: 'A Restorer resolving ranges from a per-file index MUST be able to resolve a hardlink to its primary's first_chunk_lba/size_bytes; an index that records only the literal null/0 values does not support conformant restore.' Drop 'nonconformant' as applied to catalogs, or explicitly define a 'PFR index' conformance artifact in Section 2.2.

### [MAJOR] rao/bcp14 — rao-1.0-specification.md §12.8 (l.2165) 
ISSUE: A MUST is imposed on the key registry, which Sections 1.4 and 5.3 declare an external, out-of-scope system ('The key registry is external'); no format implementation can be tested against it. The same sentence also mixes a lowercase 'must' ('must remain readable'), leaving its own strength ambiguous.
QUOTE: Old key epochs MUST remain recoverable in the registry for as long
as objects sealed under them must remain readable.
FIX: Restate as deployment guidance without BCP 14 force, e.g. 'Old key epochs need to remain recoverable in the registry for as long as objects sealed under them are to remain readable; this is a registry/deployment obligation outside this format.'

### [MAJOR] rao/bcp14 — rao-1.0-specification.md §12.1 (l.2075) 
ISSUE: BCP 14 keywords repeatedly bind the external catalog, declared out of scope in Sections 1.4/1.5: 'the catalog SHOULD run a consistency check' and 'A repeat ... SHOULD be rejected loudly' (12.1, lines 2075/2080), 'catalogs MAY store it once' (3.3, line 328), 'MUST NOT be referenced by any durable catalog' (5.8 line 1484 and 8.3 line 1860, both passive with no bound actor). None is testable against an implementation of this format.
QUOTE: As a deployment belt, the catalog SHOULD run a consistency check on
`(key_id, hkdf_salt)` at insert.
FIX: Recast the catalog-facing sentences as operational guidance ('as a deployment belt, a catalog can enforce...'), and rewrite the passive MUST NOTs onto the writing role: 'A Writer/Sealer MUST NOT report an unfooters/partial output as complete' (already stated elsewhere), leaving catalog behavior as guidance.

### [MAJOR] rao/bcp14 — rao-1.0-specification.md §8.2 (l.1833) 
ISSUE: MUST NOT requirements are imposed on the parity layer's bootstrap rows ('An encrypted object's row MUST NOT carry manifest anchors', repeated in 9 and 12.5 line 2131) — i.e. on an implementation of REM-PARITY, not of this format — while [REMPARITY] is listed only as an informative reference in 15.2. A normative requirement whose subject is another specification's implementation cannot rest on an informative reference.
QUOTE: An **encrypted** object's row MUST NOT carry manifest
anchors
FIX: Either move the bootstrap-content requirement into REM-PARITY and state it here descriptively ('the REM-PARITY binding carries no manifest anchors for encrypted objects; see [REMPARITY]'), or promote [REMPARITY] to a normative reference and name the bound role explicitly.

### [MAJOR] rao/bcp14 — rao-1.0-specification.md §10 (l.1913) 
ISSUE: All-caps 'REQUIRES' is not a BCP 14 keyword (only 'REQUIRED' is defined); per the Section 2.1 boilerplate it therefore has no defined meaning, yet its capitalization visually claims keyword force — exactly the ambiguity BCP 14 exists to prevent.
QUOTE: REQUIRES a new `format_id` — which is, by definition, RAO version 2.
FIX: Rephrase with a defined keyword and clear subject, e.g. 'is a new-format_id change: such a revision MUST use a new format_id and is, by definition, RAO version 2', or use lowercase 'requires' if no BCP 14 force is intended.

### [MAJOR] rao/bcp14 — rao-1.0-specification.md §4.3 (l.418) 
ISSUE: The same reader obligation on the reader-ignored ustar fields is stated at three different strengths: 'SHOULD be liberal in the fields this section marks reader-ignored' (4.3, line 418), 'Readers MUST ignore these fields' (4.3.1, line 451), and 'Readers MUST NOT base acceptance on mode, uid, gid, ...' (4.3.2, line 455). An implementer reading only 4.3 could treat strict validation of these fields as a mere SHOULD-level deviation.
QUOTE: and SHOULD be liberal in the fields this section marks
reader-ignored
FIX: Make the 4.3 sentence a non-keyword pointer ('and are subject to the reader-ignored-field rules of Section 4.3.2'), leaving 4.3.2's MUST NOT as the single normative statement; align 4.3.1's 'MUST ignore' with 4.3.2's 'MUST NOT base acceptance' wording or state which of the two obligations is intended.

### [MAJOR] rao/bcp14 — rao-1.0-specification.md §4.5.1 (l.581) 
ISSUE: The REMANENCE.chunk_size table row states an unconditional MUST referencing 'the containing tape file', but Section 4.2 states the same requirement conditioned 'On tape'; for a file or object-store copy there is no containing tape file, so the table's constraint is unsatisfiable as written — the same requirement stated twice with different scope (also restated in 8.2 and as lowercase 'must' in Appendix C item 3).
QUOTE: MUST equal the body-block size of the containing tape file
FIX: Condition the table text: 'Decimal chunk_size in bytes; on tape, MUST equal the containing tape file's block size (Sections 4.2, 8.2)', and keep Section 4.2 as the single normative statement.

### [MAJOR] rao/bcp14 — rao-1.0-specification.md §12.10 (l.2197) 
ISSUE: A block of security-critical MUSTs binds 'Restore implementations', but Section 1.4 assigns 'restore-time path sanitization' to the out-of-scope orchestration layer, and no Section 2.2 role covers filesystem materialization (Restorer is defined only as range-mapping; Consumer only interprets a manifest) — so these MUSTs bind no defined conformance role and contradict the scope statement.
QUOTE: Restore implementations MUST therefore keep their own sanitization.
FIX: Define the restoring actor (e.g. extend the Consumer role, or add a 'Restoring Consumer' role in 2.2 covering filesystem materialization) and bind the 12.10 MUSTs to it; reconcile Section 1.4's scope sentence to say path sanitization requirements on that role are given in 12.10.

### [MAJOR] rao/bcp14 — rao-1.1-specification.md §2.3 (l.89) 
ISSUE: The 1.1 reader's defining behavior is stated with no requirements language: an implementer cannot tell whether surfacing preserved xattrs and reapplying them on restore is MUST, SHOULD, or descriptive — in a two-page normative delta whose only new behavior this is.
QUOTE: A 1.1 reader surfaces an entry's preserved xattrs and, on restore, reapplies
them (`setxattr`).
FIX: State the intended strength on named roles, e.g. 'A 1.1 Reader MUST surface an entry's preserved xattrs; a restoring Consumer SHOULD reapply them (setxattr) and MUST report any xattr it could not reapply' (adjust to the intended strengths).

### [MAJOR] rao/bcp14 — rao-1.1-specification.md §2.1 (l.54) 
ISSUE: For non-UTF-8 xattr names the only stated behavior belongs to the out-of-scope ingest layer ('the ingest layer drops-and-records it, or wraps the file'); the normative wire-format section never says what a Writer of THIS format does if handed such a name — reject, drop, or undefined.
QUOTE: the ingest layer
drops-and-records it, or wraps the file.
FIX: Add a Writer requirement, e.g. 'A Writer MUST reject (InvalidInput) an xattrs key that is not valid UTF-8'; keep the ingest-layer sentence as an informative note about how such names are expected to be avoided upstream.

### [MAJOR] rao/bcp14 — rao-1.0-specification.md §14 (l.2419) 
ISSUE: Conformance/freeze criterion 9 is bound to the author's internal hardware and deployment ('QuadStor VTL', 'MSL3040'); no independent implementer can evaluate it from the document, and it mixes the author's project process into what a published spec presents as conformance criteria.
QUOTE: A live round-trip passes on the QuadStor VTL and on the MSL3040
FIX: Generalize to equipment classes ('a physical LTO library and a virtual tape library, at two distinct chunk_size values') or move criteria 1–11 into an explicitly informative 'Implementation status / pre-freeze checklist' appendix, keeping Section 14 to implementation conformance statements only.

### [MAJOR] rao/consistency — rao-1.0-specification.md §6.4 (l.1696) 
ISSUE: The k+2 bound is wrong for k > C/16. Recomputation: k consecutive chunks occupy L = k(C+16) contiguous stored bytes; a byte range of length L spans at most ceil(L/C) + 1 blocks of size C = k + ceil(16k/C) + 1. This equals k+2 only when 16k ≤ C. At the default C = 262144 an 8 GiB file has k = 32768 chunks, 16k/C = 2, so up to k+3 blocks; at the test-vector C = 4096 the bound already fails at k = 257 chunks (~1 MiB file). An implementer sizing tape reads from this bound under-reads on large PFR ranges.
QUOTE: a run of `k` consecutive chunks occupies one contiguous stored-block range of at most `k + 2` blocks
FIX: Replace 'at most k + 2 blocks' with 'at most k + ceil(16·k / C) + 1 blocks (k + 2 whenever 16·k ≤ C)', or otherwise restate the bound correctly for arbitrary k.

### [MAJOR] rao/consistency — rao-1.0-specification.md §Appendix A (l.2501) 
ISSUE: Direct contradiction of the normative text. §5.5.2 states 'The zero nonce is byte-identical to the nonce of a non-final payload chunk 0', and §12.2 repeats it. In RAO-TV-E1 chunk_count = 5, so chunk 0 IS non-final and its nonce (11 zero counter bytes || 0x00 final flag) is exactly the 12 zero bytes of the metadata nonce — the collision does occur, and only the metadata/payload key separation makes it harmless. The appendix sentence asserts the opposite of what its own parenthetical (and §§5.5.2/12.2) says.
QUOTE: The metadata zero nonce does not collide with any payload nonce here (chunk 0 is non-final, nonce `00…00 00` — the collision the key separation guards, Section 12.2)
FIX: Rewrite to: 'The metadata zero nonce IS byte-identical to payload chunk 0's nonce here (chunk 0 is non-final, nonce 00…00 00) — exactly the collision that the metadata/payload key separation renders harmless (Sections 5.5.2, 12.2).'

### [MAJOR] rao/editorial — rao-1.0-specification.md §Front matter (l.13) 
ISSUE: The document header points implementers at a repository-relative crate path that does not exist outside the author's codebase.
QUOTE: Reference implementation | Remanence (`crates/remanence-format`)
FIX: Replace with a self-contained statement, e.g. "Reference implementation | Remanence" (no path), or drop the row; a standalone spec must not depend on repository layout.

### [MAJOR] rao/editorial — rao-1.0-specification.md §13 (Test Vectors) (l.2214) 
ISSUE: Normative test-vector location is given as a repository-relative path (also 'live in' is colloquial); an external implementer has no repository.
QUOTE: Static vectors live in the repository (`fixtures/rao/`)
FIX: Rewrite as "Static test vectors are published alongside this specification" and define how they are obtained; remove `fixtures/rao/` here and in Appendix C item 1.

### [MAJOR] rao/editorial — rao-1.0-specification.md §15.2 (Informative References) (l.2450) 
ISSUE: The [REMPARITY] reference cites a repository-relative filename instead of a publishable citation.
QUOTE: (`specs/rem-parity-1.0-specification.md`)
FIX: Cite by title, version, and publication venue only: "[REMPARITY] Rem Tape Parity (REM-PARITY) Format, Version 1.0, published alongside this document." Delete the path.

### [MAJOR] rao/editorial — rao-1.0-specification.md §14 (Conformance and Freeze Criteria), criterion 9 (l.2419) 
ISSUE: A conformance criterion names the author's internal lab hardware (QuadStor VTL, MSL3040), which no external implementer can satisfy or even identify.
QUOTE: passes on the QuadStor VTL and on the MSL3040
FIX: Generalize: "A live round-trip passes on a virtual tape library and on a physical tape library for both representations at two distinct chunk_size values...", or move deployment-specific validation out of the published document.

### [MAJOR] rao/editorial — rao-1.0-specification.md §5.10 (also 5.3) (l.1598) 
ISSUE: Normative text references internal tool surfaces: the `rem archive inspect` CLI here and the `--key-file` flag in Section 5.3 (line 1136); these are implementation details of an unpublished tool.
QUOTE: the operational `rem archive inspect` surface
FIX: Replace with role language: "Keyless inspection (an operation a conformant implementation SHOULD expose) reveals exactly the header fields..."; in 5.3 delete or generalize the parenthetical "(in the reference implementation, a 32-byte key file named by `--key-file`...)".

### [MAJOR] rao/editorial — rao-1.0-specification.md §Abstract (l.36) 
ISSUE: "AEAD" is used in the Abstract and throughout (~40 occurrences) but is never expanded anywhere in the document, contrary to RFC 7322 abbreviation rules.
QUOTE: AEAD chunks coincide one-to-one with the object's body blocks
FIX: Expand at first use in the Abstract and again in Section 2: "authenticated encryption with associated data (AEAD)". Also expand "AAD" at its first use (Section 5.4).

### [MAJOR] rao/editorial — rao-1.0-specification.md §12.2 (also 5.4.1) (l.2092) 
ISSUE: A prohibition is phrased with scare quotes and lowercase "may" instead of BCP 14 language; Section 11 line 1941 has the same defect ("No code path ... may panic").
QUOTE: No future revision may "simplify" the key schedule by merging them.
FIX: Rewrite: "Future revisions MUST NOT merge the metadata and payload keys." In Section 11: "Code paths reachable from object bytes MUST NOT panic, crash, or allocate unboundedly." Also retitle the Section 12.2 heading (see the 'load-bearing' finding).

### [MAJOR] rao/editorial — rao-1.0-specification.md §12.2 heading and 5.4.1 (l.1213) 
ISSUE: "Load-bearing" is a colloquial construction-site metaphor used both in a section heading and in normative prose introducing MUST-level properties.
QUOTE: Properties, each load-bearing:
FIX: Heading: "12.2. Key Separation Is Required for Nonce Safety". Prose at 5.4.1: "Properties, each load-bearing:" -> "The following properties are normatively relied upon:".

### [MAJOR] rao/editorial — rao-1.0-specification.md §12.1 (also Appendix C.6) (l.2075) 
ISSUE: "Belt" (belt-and-suspenders slang) appears three times, including in a SHOULD-level recommendation and an appendix item title.
QUOTE: As a deployment belt, the catalog SHOULD run a consistency check
FIX: "As a defense-in-depth measure, the catalog SHOULD..."; Appendix C.6 title "Catalog salt-audit check"; "...enforce that check directly."

### [MAJOR] rao/editorial — rao-1.0-specification.md §12.5 (l.2130) 
ISSUE: Two colloquialisms in one normative paragraph: the bootstrap is "deliberately starved" and trust domains are "named so nobody is surprised".
QUOTE: The on-tape **parity-layer bootstrap** is deliberately starved
FIX: "...are separate trust domains, out of scope of the format but identified here for completeness." and "The on-tape parity-layer bootstrap is deliberately minimal: for encrypted objects it carries only...".

### [MAJOR] rao/editorial — rao-1.0-specification.md §5.8, step 5 (pattern throughout) (l.1482) 
ISSUE: Inline bold is used to carry normative force inside BCP 14 sentences; bold does not survive conversion to RFC publication formats, and the emphasis pattern recurs (e.g. lines 649, 1363, 1558, and "MUST be *accepted*" at 2358).
QUOTE: **On any failure the Sealer MUST NOT write the footer**
FIX: Remove emphasis from normative sentences document-wide and let the capitalized keywords carry the requirement: "On any failure the Sealer MUST NOT write the footer;". Reserve bold for defined-term introductions only.

### [MAJOR] rao/editorial — rao-1.0-specification.md §5.6.2 (pattern throughout) (l.1389) 
ISSUE: Conformance role names defined with initial capitals in Section 2.2 appear inconsistently lowercased in normative sentences: "a reader MUST" (1389), "a reader MAY" (1122), "the writer MUST" (466), "a writer MUST fail" (713), "A keyless verifier MAY" (1584).
QUOTE: A reader MUST verify each chunk's tag
FIX: Audit the document and capitalize every use that denotes a Section 2.2 conformance role (Reader, Writer, Sealer, Keyless Verifier, ...), reserving lowercase for generic senses.

### [MAJOR] rao/editorial — rao-1.0-specification.md §4.9 (l.968) 
ISSUE: A RECOMMENDED and a MUST are buried inside parentheticals within an em-dash pair, obscuring the normative content of the two I/O profiles.
QUOTE: Two I/O profiles exist — **streaming** (RECOMMENDED; O(`chunk_size` + one pax header) memory) and **materializing** (compatibility; MUST bound up-front allocation with a fallible reservation)
FIX: Split: "Two I/O profiles exist, with identical acceptance rules. The streaming profile is RECOMMENDED; it requires memory proportional to chunk_size plus one pax header. The materializing profile exists for compatibility; a materializing Reader MUST bound its up-front allocation with a fallible reservation."

### [MAJOR] rao/editorial — rao-1.0-specification.md §5.2 (l.1124) 
ISSUE: Cross-reference error: the single-fault property of conformance test vectors is defined in Section 13.5 (Negative Vectors), not 13.4 (Default Chunk Size).
QUOTE: conformance test vectors contain exactly one fault each (Section 13.4)
FIX: Change "(Section 13.4)" to "(Section 13.5)".

### [MAJOR] rao/editorial — rao-1.1-specification.md §Front matter (l.13) 
ISSUE: The header cites an internal, unpublished design document by repository path.
QUOTE: Design record | `docs/rao-1.1-metadata-preservation-design-v0.1.md`
FIX: Delete the "Design record" row; a published spec must be self-contained (design rationale belongs in an informative appendix, as RAO 1.0 does with Appendix B).

### [MAJOR] rao/editorial — rao-1.1-specification.md §2.2 (l.80) 
ISSUE: The informative policy section cites a second internal, unpublished design document by repository path.
QUOTE: (`docs/ingest-archive-deferred-items-design-v0.1.md`)
FIX: Delete the parenthetical citation and make the one-paragraph policy summary self-contained.

### [MAJOR] rao/editorial — rao-1.1-specification.md §2.2 (also 2.1) (l.84) 
ISSUE: The term "wrapped" (also "wraps the file", line 56) is never defined in either specification, and "the resource fork" assumes unexplained Apple-platform context; an implementer cannot determine what behavior is meant.
QUOTE: causes the **file to be wrapped** rather than annotated
FIX: Either define wrapping (e.g. "the file is stored with its attribute carried as ordinary payload in a container file, outside this format's scope") or state simply that oversized attributes are excluded from native preservation by ingest policy; generalize or gloss "resource fork" (e.g. "a large platform attribute such as the macOS resource fork").

### [MAJOR] rao/implementability — rao-1.0-specification.md §4.4.4 (l.554) 
ISSUE: No canonical emission form for the pax `mtime` value is defined and the input model (string vs. timestamp) is unstated; an implementer whose input is a numeric timestamp could emit `1700000000.5` while another emits `1700000000.500000000` — different bytes for the same logical inputs, unpinnable by the 13.1 'full metadata' vector.
QUOTE: non-negative decimal seconds since the epoch, optionally followed by `.` and fractional digits. Writers MUST validate this shape
FIX: Either state that `mtime` is a caller-supplied string emitted verbatim after shape validation (making the string itself the input for determinism purposes), or define a canonical encoding (e.g. no leading zeros, no trailing fractional zeros, no bare trailing '.').

### [MAJOR] rao/implementability — rao-1.0-specification.md §6.4 (l.1697) 
ISSUE: The stated bound is mathematically false once 16k >= C + 2: for C = 512 a run of k = 33 chunks (33 x 528 = 17,424 bytes starting at block offset 511) spans 36 = k + 3 stored blocks, and the excess grows with k (a 4 GiB file at the default C already exceeds it); an implementer sizing reads or buffers from this bound under-reads.
QUOTE: a run of `k` consecutive chunks occupies one contiguous stored-block range of at most `k + 2` blocks
FIX: Replace with the correct bound — a range of l bytes spans at most floor((l-2)/C) + 2 blocks, so k consecutive chunks span at most k + floor((16k - 2)/C) + 2 stored blocks — or qualify 'k + 2' as valid only while 16k < C + 2.

### [MAJOR] rao/implementability — rao-1.0-specification.md §13 (l.2214) 
ISSUE: The conformance vectors are anchored to the author's repository by a repo-relative path, and every cryptographic expected value is '[pinned-at-generation]' and absent from the document; an independent implementer working from the published document alone cannot obtain the fixtures, contradicting the Abstract's claim of 'long-term recovery from this document and its static test vectors alone' and Section 14 criterion 2's 'exist in-repo'.
QUOTE: Static vectors live in the repository (`fixtures/rao/`)
FIX: Publish the frozen vectors (inputs, pinned byte streams or digests, expected error names) inside the document or as a companion artifact with a stable public citation, and replace 'live in the repository (`fixtures/rao/`)' and 'in-repo' with references to that published artifact.

### [MAJOR] rao/implementability — rao-1.0-specification.md §14 (l.2419) 
ISSUE: Freeze criterion 9 names the author's internal deployment hardware (a specific VTL product and an HP StoreEver MSL3040 library), which no independent implementer can satisfy or even interpret from the document, and which leaks deployment detail into an internationally publishable conformance section.
QUOTE: A live round-trip passes on the QuadStor VTL and on the MSL3040
FIX: Restate hardware-neutrally (e.g. 'on one virtual tape library and one physical tape library/drive, for both representations at two distinct chunk_size values') or move the deployment-specific checklist out of the published document.

### [MAJOR] rao/implementability — rao-1.1-specification.md §3 (l.95) 
ISSUE: The 1.1 test-vector section states no concrete inputs at all — no object parameters, no file contents, no exact xattr name/value bytes — so the vectors it declares 'byte-pinned' are not reproducible by an independent implementer, far below the standard set by RAO 1.0 Section 13.2's complete input tables.
QUOTE: a regular entry with a small xattr (e.g. a Finder color tag)
FIX: Specify each vector's complete inputs in the 13.2 style (chunk_size, object_id, caller_object_id, write_timestamp, file spec, exact xattr name and value bytes) and the derivable expected layout/manifest deltas.

### [MAJOR] rao/implementability — rao-1.1-specification.md §Header table (l.13) 
ISSUE: The document's front matter cites an unpublished internal design document by repository-relative path; a standalone published specification cannot depend on or advertise documents the reader cannot obtain.
QUOTE: | Design record | `docs/rao-1.1-metadata-preservation-design-v0.1.md` |
FIX: Delete the 'Design record' row (or replace it with a published citation if the design record is itself published).

### [MAJOR] rao/implementability — rao-1.1-specification.md §2.2 (l.79) 
ISSUE: Section 2.2's policy description is anchored to a second unpublished internal design document, so the sentence 'a built-in junk baseline is always dropped; the rest is governed by a ruleset-selected denylist...' cannot be interpreted from the published corpus at all.
QUOTE: (`docs/ingest-archive-deferred-items-design-v0.1.md`)
FIX: Remove the internal-document citation and rewrite 2.2 to be self-contained: state only that xattr selection is an ingest-side policy outside this format and that the format stores whatever surviving xattrs it is handed.

### [MAJOR] rao/standalone — rao-1.0-specification.md §Front matter (l.13) 
ISSUE: The front-matter 'Reference implementation' row cites a repository-relative crate path meaningless to any reader without the author's codebase.
QUOTE: | Reference implementation | Remanence (`crates/remanence-format`) |
FIX: Either delete the row, or reduce it to '| Originating implementation | Remanence |' with no path. Better RFC practice: move implementation/maturity claims out of the front matter into an informative 'Implementation Status' note (RFC 7942 style), e.g. 'Remanence, the originating implementation, implements both representations of this specification in full; the Section 13 test vectors distributed with this document were generated by it and independently re-derived by a second implementation.' Correspondingly reword Section 14 criterion 1 from 'A reference implementation implements this document' to 'At least one complete implementation of this document exists, covering both representations.'

### [MAJOR] rao/standalone — rao-1.0-specification.md §Front matter (editorial note) (l.15) 
ISSUE: An internal process artifact ('remove at freeze') sits in the published front matter, referencing the author's freeze workflow rather than telling an outside reader anything actionable.
QUOTE: > **Editorial note (remove at freeze):** this document is the normative fixed
FIX: Remove the block quote at publication. The one externally useful sentence ('this document is the normative fixed point; an implementation is validated against it, not the reverse') belongs in Section 14's opening paragraph; the pinned-at-generation explanation already exists in Section 13 and need not be duplicated here.

### [MAJOR] rao/standalone — rao-1.0-specification.md §14 (freeze criterion 9) (l.2419) 
ISSUE: A conformance/maturity criterion names the author's internal test hardware (QuadStor VTL, HPE MSL3040 library) — no outside reader can evaluate or reproduce a criterion defined by private lab equipment.
QUOTE: A live round-trip passes on the QuadStor VTL and on the MSL3040 for both
FIX: Rephrase hardware-neutrally: 'A live round-trip passes on at least one virtual tape library and at least one physical LTO tape library, for both representations at two distinct chunk_size values, including standard-`tar -b` extraction of the plaintext copy.'

### [MAJOR] rao/standalone — rao-1.0-specification.md §5.10 (l.1598) 
ISSUE: A normative section defines keyless inspection by pointing at the author's CLI subcommand, which the reader does not have.
QUOTE: Keyless **inspect** (the operational `rem archive inspect` surface) reveals
FIX: Drop the tool name: 'A keyless inspect operation reveals exactly the header fields: magic, version, suite, chunk_size, key_id, hkdf_salt, metadata_frame_len, object_id — plus the stored length and the derived plaintext_size/chunk_count above. … Keyless inspection is how an operator recovers which key epoch to materialize (key_id → registry) without holding any key.'

### [MAJOR] rao/standalone — rao-1.0-specification.md §5.3 (l.1135) 
ISSUE: A normative key-handling paragraph cites the author's CLI flag as if the reader could consult that tool; '--key-file' is meaningless in a standalone document.
QUOTE: (in the reference implementation, a 32-byte key file named by `--key-file`, with the `key_id` supplied alongside)
FIX: Replace the parenthetical with an implementation-neutral example or delete it: 'callers supply root key material through an in-memory interface (for example, 32 bytes read from an operator-provided file, with the key_id supplied alongside).'

### [MAJOR] rao/standalone — rao-1.0-specification.md §5.8 and 9 (l.1495) 
ISSUE: Commit semantics are defined by a code-level API name, `finish_object()`, presented as if the reader has the author's parity-layer library; it recurs in Section 9 item 3 (line 1889: 'committed when the parity layer's `finish_object()` returns').
QUOTE: the parity layer's `finish_object()` for tape
FIX: If [REMPARITY] normatively defines an operation of that name, cite it as such ('the object-commit operation defined by [REMPARITY]'); otherwise reword both occurrences: Section 5.8 — 'temp-file + fsync + rename for file outputs; the parity layer's durable object-commit barrier for tape ([REMPARITY])'; Section 9 item 3 — 'An object is committed when the parity layer's object-commit operation completes ([REMPARITY]); neither representation defines an in-band commit marker.'

### [MAJOR] rao/standalone — rao-1.0-specification.md §Appendix C (l.2651) 
ISSUE: Appendix C is the author's internal pre-freeze backlog: item 2 says 'confirm no orchestrator will mint longer identifiers before freeze' (line 2664), item 3 says 'Confirm against the parity-layer and tape block-size configuration' (line 2668), item 4 opens 'The reference implementation must expose…' (line 2670), and item 6 ends 'Add the schema/API field before freeze if the catalog is to enforce that belt directly' (line 2682) — none of it is actionable by an outside implementer.
QUOTE: ## Appendix C. Open Items Before Freeze
FIX: Delete Appendix C at publication (as the editorial note intends). Preserve the two externally useful caveats inside the body first: fold item 2 into Section 5.2 ('systems assigning object identifiers must ensure they fit in 64 UTF-8 bytes if encrypted copies will be produced') and item 3 into Section 8.2 (already stated there; add 'deployments typically fix one fleet-wide chunk_size'). Items 1, 4, 5, 6 are implementation/deployment work items and should not survive in the published text.

### [MAJOR] rao/standalone — rao-1.1-specification.md §Front matter (l.13) 
ISSUE: The front matter cites an unpublished internal design document by repository-relative path; a reader cannot obtain it and it carries no normative weight.
QUOTE: | Design record | `docs/rao-1.1-metadata-preservation-design-v0.1.md` |
FIX: Delete the 'Design record' row entirely. If provenance is wanted, an informative acknowledgment sentence without a path suffices; nothing in the document should depend on the design record.

### [MAJOR] rao/standalone — rao-1.1-specification.md §2.2 (l.80) 
ISSUE: Section 2.2 cites a second unpublished internal design document as the source of the xattr-selection policy.
QUOTE: (`docs/ingest-archive-deferred-items-design-v0.1.md`)
FIX: Remove the citation and let the (already restated) policy stand on its own: '*Which* xattrs are preserved is a policy of the ingesting system, outside the scope of this format: typically a built-in junk baseline is always dropped, with the remainder governed by a denylist or allowlist stance, and all drops recorded. The format stores whatever surviving xattrs it is handed.'

### [MAJOR] rao/standalone — rao-1.1-specification.md §2.1 / 2.2 (l.56) 
ISSUE: 'Wraps the file' / 'the file to be wrapped' (also line 85) is undefined internal jargon from the author's ingest system — a stranger implementing from the document cannot act on it, and 'the ingest layer' presumes the author's stack (also the §2.2 heading 'the rule is ingest-side').
QUOTE: the ingest layer drops-and-records it, or wraps the file
FIX: Either define the behavior generically or remove it: e.g. '…is out of scope for native preservation: the ingesting system may omit the attribute (recording the omission) or store the file together with its attributes in a container format of its choosing before archiving.' Apply the same rewording at line 85 ('causes the file to be wrapped rather than annotated') and generalize 'the ingest layer' to 'the ingesting system'. The macOS-specific aside '(the resource fork is the only routinely large case)' should be softened to an example: '(e.g. macOS resource forks)'.

### [MAJOR] rao/standalone — rao-1.1-specification.md §3 (l.96) 
ISSUE: The 1.1 test-vector additions are not actionable: unlike RAO 1.0 §13.2, no exact inputs are pinned ('a small xattr (e.g. a Finder color tag)' names no xattr name or value bytes, no base object, no expected digests), and no distribution mechanism is stated, so an independent implementer cannot generate or check the same bytes.
QUOTE: a regular entry with a small xattr (e.g. a Finder color tag)
FIX: Specify the vectors in the 1.0 §13.2 style: name a base vector (e.g. RAO-TV-P1 inputs), pin the exact xattr name and value bytes (e.g. name `user.color`, value the 4 bytes `74 65 73 74`), and state the pinned expected outputs (manifest CBOR bytes, manifest_sha256, plaintext_digest). Add: 'These vectors are distributed with this specification alongside the RAO 1.0 vectors.'

### [MAJOR] rao/standalone — rao-1.1-specification.md §Front matter (editorial note) (l.15) 
ISSUE: Same internal process artifact as in the 1.0 document: a remove-at-freeze editorial note in the published front matter.
QUOTE: > **Editorial note (remove at freeze):** a normative additive delta on RAO 1.0;
FIX: Delete the note at publication. Its two useful clarifications survive elsewhere: 'BCP 14 language and all conventions are inherited from RAO 1.0 §2' belongs as a normal sentence in Section 1, and the hardlink/entry-scope pointer is already covered by the RAO 1.0 cross-references.

### [MAJOR] rao/structure — rao-1.0-specification.md §15.1 (Normative References) / 4.6.2 (l.664) 
ISSUE: SHA-256 is the format's core primitive (file_sha256, manifest_sha256, plaintext_digest, stored_digest, HKDF-SHA-256, header_hash) and is invoked normatively throughout, yet there is no reference for it at all — neither FIPS 180-4 nor RFC 6234 appears in Section 15.
QUOTE: Exactly 64 lowercase hex digits: SHA-256 of the exact payload bytes
FIX: Add a normative reference, e.g. [FIPS-180-4] NIST, "Secure Hash Standard (SHS)", FIPS PUB 180-4, August 2015, DOI 10.6028/NIST.FIPS.180-4 (or [RFC6234]), and cite it at the first normative use of SHA-256.

### [MAJOR] rao/structure — rao-1.0-specification.md §4.5.1 / 15.1 (l.587) 
ISSUE: REMANENCE.write_timestamp is normatively defined as "RFC 3339 timestamp" but RFC 3339 is named inline without a citation anchor and is absent from the References section, so a required wire-value syntax has no retrievable definition.
QUOTE: `REMANENCE.write_timestamp` | RFC 3339 timestamp of object creation
FIX: Add [RFC3339] to Normative References (Klyne, G. and C. Newman, "Date and Time on the Internet: Timestamps", RFC 3339, DOI 10.17487/RFC3339, July 2002) and change the inline mention to the [RFC3339] anchor.

### [MAJOR] rao/structure — rao-1.0-specification.md §2.4 / 4.4.1 / 15.1 (l.523) 
ISSUE: UTF-8 validity is a MUST-level acceptance criterion in many places (pax values, entry paths, the envelope object_id field, manifest text strings) but RFC 3629 (UTF-8) is never cited and is absent from the References.
QUOTE: `<value>` MUST be valid UTF-8 and MUST NOT contain any byte < 0x20
FIX: Add [RFC3629] (Yergeau, F., "UTF-8, a transformation format of ISO 10646", STD 63, RFC 3629, November 2003) to Normative References and cite it at Section 2.4's text conventions (optionally also RFC 20 for ASCII).

### [MAJOR] rao/structure — rao-1.0-specification.md §15.1 (l.2442) 
ISSUE: The [POSIX-PAX] citation names no edition, year, publisher detail, or URL — "IEEE Std 1003.1" has materially different revisions (2008, 2017, 2024), and pax interchange details are exactly what an implementer must retrieve; the citation is not unambiguous.
QUOTE: [POSIX-PAX] — IEEE Std 1003.1, `pax` Interchange Format (ustar and pax
FIX: Pin an edition with a full citation, e.g. "IEEE Std 1003.1-2024 / The Open Group Base Specifications Issue 8, Shell and Utilities, 'pax — portable archive interchange' (ustar and pax Interchange Formats), IEEE/The Open Group" with a URL.

### [MAJOR] rao/structure — rao-1.0-specification.md §15.1 / 15.2 (l.2438) 
ISSUE: Every reference entry is a bare anchor plus a phrase — no authors, no full titles, no dates, no DOIs/URLs, no BCP/STD series formatting — which is systematically below RFC 7322 reference style and leaves several entries hard to retrieve unambiguously.
QUOTE: - [RFC2119] / [RFC8174] — BCP 14 requirement keywords.
FIX: Rewrite all entries in full RFC citation form, e.g. "[RFC2119] Bradner, S., \"Key words for use in RFCs to Indicate Requirement Levels\", BCP 14, RFC 2119, DOI 10.17487/RFC2119, March 1997." and similarly for RFC 8174, 5869, 8439, 8949 (STD 94, Bormann & Hoffman, December 2020).

### [MAJOR] rao/structure — rao-1.0-specification.md §15.2 / 5.6 / Appendix B.4 (l.2447) 
ISSUE: The [AGE] citation gives no URL, version, or C2SP document identifier, so it is not retrievable as cited; worse, its informative classification is ambiguous because Section 5.6 defines the payload frame as "the age-style STREAM construction [AGE]" and B.4 says the construction is "used unchanged", which reads as a normative dependency.
QUOTE: - [AGE] — "The age format specification", C2SP: the payload STREAM
FIX: Cite fully (e.g. Valsorda, F. and B. Cartwright-Cox, "The age file encryption format", C2SP, https://c2sp.org/age, with a pinned version/commit) and either state explicitly that Sections 5.6.1–5.6.3 are self-contained and [AGE] is background only, or move [AGE] to Normative References.

### [MAJOR] rao/structure — rao-1.0-specification.md §15.2 / 8.2 (l.2450) 
ISSUE: The [REMPARITY] reference cites a repository-relative file path — unusable outside the author's repo — and is classified informative even though Section 8.2 states MUST-level interoperability requirements against REM-PARITY's bootstrap CBOR keys (key 30, keys 10–13, keys 20–21), making it a normative dependency for the tape binding.
QUOTE: (`specs/rem-parity-1.0-specification.md`): the parity layer of
FIX: Cite the co-published REM-PARITY specification by title, version, date, and a stable public locator (no repo path), and move it to Normative References (or restate in Section 8.2 that the bootstrap row encoding is normatively defined by REM-PARITY and only summarized here).

### [MAJOR] rao/structure — rao-1.0-specification.md §whole document (missing IANA Considerations) (l.2434) 
ISSUE: There is no IANA Considerations section; an RFC-style publication carries one even when empty, and this format actually has registrable-looking artifacts (a file extension, magic bytes, a keyword namespace) whose non-registration should be stated deliberately.
QUOTE: ## 15. References
FIX: Add "## 15. IANA Considerations — This document has no IANA actions." (renumbering References), optionally noting that the .rao extension, RAO1 magic, and REMANENCE. namespace are defined by this document and not registered.

### [MAJOR] rao/structure — rao-1.0-specification.md §front matter / end of document (l.7) 
ISSUE: Neither document identifies an author, editor, or publishing organization anywhere, and there is no Authors' Addresses section — an implementer or errata reporter has no contact, and an internationally published standalone spec is not credible without attribution (the same defect exists in rao-1.1-specification.md).
QUOTE: | Status | Draft for review |
FIX: Add an Authors' Addresses section (name, organization, email) at the end of both documents and an author/editor row in the front-matter table; add an optional Acknowledgements section if contributors exist.

### [MAJOR] rao/structure — rao-1.0-specification.md §13 (Test Vectors) (l.2214) 
ISSUE: The conformance-critical test vectors are located only by a repository-relative path, so a stranger building from "the document alone" cannot obtain them — yet the Abstract promises recovery "from this document and its static test vectors alone" and Section 14 makes them the conformance anchor.
QUOTE: Static vectors live in the repository (`fixtures/rao/`), each with a manifest
FIX: Publish the fixture set as a citable companion artifact with a stable public URL/identifier, add it to the References, and replace the repo path with that citation (or inline the pinned vectors in an appendix at freeze).

### [MAJOR] rao/structure — rao-1.0-specification.md §14 (Conformance and Freeze Criteria) (l.2419) 
ISSUE: Freeze criterion 9 names the author's internal deployment hardware ("QuadStor VTL", "MSL3040") inside the published conformance criteria; an external implementer cannot satisfy or even interpret this, and internal hardware names do not belong in an international spec.
QUOTE: 9. A live round-trip passes on the QuadStor VTL and on the MSL3040 for both
FIX: Restate vendor-neutrally ("on at least one virtual tape library and one physical LTO tape library") or move deployment-specific validation items out of the published document into an informative implementation-status note that is removed at publication (RFC 7942 style).

### [MAJOR] rao/structure — rao-1.1-specification.md §front matter / whole document (l.10) 
ISSUE: The 1.1 document lacks the entire RFC skeleton: no Abstract, no Introduction/Conventions boilerplate, no References section, no IANA Considerations, no Authors' Addresses — and its normative base is cited by bare filename rather than by document title/version, which breaks once the documents are published outside the repo.
QUOTE: | Base | `rao-1.0-specification.md` (normative for everything not restated here) |
FIX: Add an Abstract, a Conventions section restating the BCP 14 boilerplate with [RFC2119][RFC8174], a References section citing "Rem Archive Object (RAO) Format, Version 1.0" by title/date/locator (plus RFC 8949 for the CBOR wire text in Section 2.1), an empty IANA Considerations, and Authors' Addresses.

### [MAJOR] rao/structure — rao-1.1-specification.md §front matter / 2.2 (l.80) 
ISSUE: The document cites two unpublished internal design documents by repository path — the front-matter "Design record" row and the ingest-policy pointer in Section 2.2 — which a reader of the published spec can never retrieve.
QUOTE: (`docs/ingest-archive-deferred-items-design-v0.1.md`): a built-in junk baseline
FIX: Delete the "Design record" front-matter row (line 13) and the docs/ path in Section 2.2; Section 2.2 already summarizes the policy adequately, so replace the citation with "is an ingest/orchestration policy outside this format" or publish the design record and cite it properly.

### [MINOR] parity/bcp14 — rem-parity-1.0-specification.md §3.4 (l.317) 
ISSUE: Lowercase 'required' carries the load-bearing rule for commit-record content (which Section 14 step 1 depends on), so the reader cannot tell whether the content constraint is normative.
QUOTE: its required content is the tape file's filemark-map entry (Section 7.1) plus enough state to seed a Resumer
FIX: Restate with a keyword bound to a role: 'The commit record MUST contain the tape file's filemark-map entry (Section 7.1) and the state needed to seed a Resumer (Section 14).'

### [MINOR] parity/bcp14 — rem-parity-1.0-specification.md §2.2 (l.726) 
ISSUE: Many keywords bind subjects that are not Section 2.2 conformance roles: 'readers' (lines 553, 726, 771, 847, 911, 955, 1041, 1441), 'Decoders' (463), 'A conformant codec' (582), lowercase 'a writer' (1185), and abstract nouns 'Validation' (669), 'Recovery' (672), 'Classification' (1220), 'Boundary classification' (331) — leaving ambiguous which role each requirement tests.
QUOTE: readers MUST NOT require the scheme or digest records on it
FIX: Define 'Reader' in Section 2.2 as the collective of Scanner, Recoverer, Resumer, and Verifier (or of any role that parses on-tape structures), capitalize role names consistently, and re-anchor abstract-subject MUSTs to a named role.

### [MINOR] parity/bcp14 — rem-parity-1.0-specification.md §3.5 (l.331) 
ISSUE: The MUST presupposes a SCSI transport, so implementations over file-backed images or non-SCSI virtual tape cannot satisfy or be tested against it; likewise line 803's 'MUST each be applied as a real drive reconfiguration' mandates an internal technique that is not black-box observable.
QUOTE: Boundary classification MUST work for both fixed-format and descriptor-format SCSI sense data.
FIX: Condition on transport: 'When the transport is SCSI, boundary classification MUST handle both fixed- and descriptor-format sense data'; restate line 803 in terms of observable behavior ('a parsed bootstrap MUST be accepted only if its block_size_bytes equals the size actually configured for the read').

### [MINOR] parity/bcp14 — rem-parity-1.0-specification.md §8.1 (l.691) 
ISSUE: The schema_minor forward-compatibility acceptance rule — load-bearing for the 1.x extension model — is stated without a keyword in a Constraint column whose sibling cells use MUST, inviting implementers to add a stricter (interop-breaking) check.
QUOTE: readers accept any value
FIX: Change the cell to 'readers MUST accept any value'.

### [MINOR] parity/bcp14 — rem-parity-1.0-specification.md §10.5 (l.1092) 
ISSUE: The invariant list is declarative except its final clause, which alone carries MUST (line 1095), inviting the inference that the earlier invariants (ascending order, scope bound, non-empty ranges, non-zero counts) are weaker than the watermark equality.
QUOTE: Invariants, validated on every decode: entries strictly ascending
FIX: Put the keyword in the chapeau and drop the lone inline MUST: 'Decoders MUST validate all of the following on every decode: ... ; max(protected_ordinal_end_exclusive) over the entries (0 if none) equals scope_highest_protected_ordinal.'

### [MINOR] parity/bcp14 — rem-parity-1.0-specification.md §10.7 (l.1115) 
ISSUE: The inline-vs-external placement rule is normative ('if and only if') yet keyword-free and role-less, uses deployment language ('production slack margin', 'test-geometry allowance'), and its only MUST (line 1118, 'a real framing attempt, never an estimate') mandates an internal technique no conformance test can observe.
QUOTE: fits the block with a production slack margin of 4096 bytes
FIX: Bind to roles with keywords: 'A Writer MUST place the directory inline if and only if the fully framed bootstrap fits within block_size − 4096 bytes (the margin does not apply for block sizes below 8 KiB); readers MUST accept either placement', and demote the framing-attempt sentence to implementation advice.

### [MINOR] parity/bcp14 — rem-parity-1.0-specification.md §15 (l.1375) 
ISSUE: The taxonomy is SHOULD-level, yet its names are declared 'normative for the test-vector manifests' and line 1404 requires MUST-level distinguishability — three different strengths for the same mechanism, leaving the conformance status of the error names unclear.
QUOTE: Implementations SHOULD expose typed errors equivalent to the taxonomy below. Names are normative
FIX: Unify: 'Implementations MUST keep the error classes below distinguishable; the names are normative identifiers used by the Section 17 manifests; surface syntax and exposure mechanism are implementation-defined.'

### [MINOR] parity/bcp14 — rem-parity-1.0-specification.md §4.3 (l.385) 
ISSUE: The numbered MUST/SHOULD list binds 'a payload format', which is not among the Section 2.2 conformance roles, and item 3 (line 390) fuses a vague MUST ('MUST tolerate that a reader is handed whole blocks') to a SHOULD via a colon so the reader cannot tell whether self-framing is mandatory.
QUOTE: 1. MUST produce objects whose stored length is a positive exact multiple of
FIX: State that these are requirements on payload-format specifications (bindings) and add that conformance target to Section 2.2; split item 3 into a testable MUST (payload readers accept block-granular delivery) and a separate SHOULD (self-framing).

### [MINOR] parity/bcp14 — rem-parity-1.0-specification.md §16.4 (l.1450) 
ISSUE: The SHOULD binds 'deployments', an out-of-scope party per Section 1.5, so no implementation of this format could be tested against it.
QUOTE: payload confidentiality matters SHOULD store objects in an encrypted representation
FIX: Recast as operational guidance ('deployments for which payload confidentiality matters are advised to store objects in an encrypted representation') or rebind ('payload-format bindings for confidential content SHOULD specify an encrypted representation').

### [MINOR] parity/bcp14 — rem-parity-1.0-specification.md §16.2 (l.1430) 
ISSUE: The SHOULD binds a development practice (fuzzing) rather than observable implementation behavior; conformance of a shipped implementation cannot be tested against whether it was fuzzed.
QUOTE: Implementations SHOULD fuzz the bootstrap, sidecar, and parity_map parsers
FIX: Demote to guidance: 'Implementers are strongly encouraged to fuzz the bootstrap, sidecar, and parity_map parsers and the scan walk', keeping Section 18 criterion 3 as the process gate.

### [MINOR] parity/bcp14 — rem-parity-1.0-specification.md §17 (l.1472) 
ISSUE: The MUST binds the content of the author's fixture repository (itself identified only by the repository-relative path 'fixtures/rem-parity-1/'), not any implementation of the format — no conformance role can satisfy or violate it.
QUOTE: at least one header-level vector MUST use the default geometry parameters
FIX: Move to Section 18 as a freeze criterion without BCP 14 keywords ('the fixture set includes at least one header-level vector at the default geometry'), and replace the repository path with a citable fixture-distribution reference.

### [MINOR] parity/bcp14 — rem-parity-1.0-specification.md §18 (l.1544) 
ISSUE: The SHOULD binds the specification's own freeze process rather than an implementation — a process recommendation dressed as a conformance requirement.
QUOTE: freeze SHOULD wait for it
FIX: Lowercase it: 'Not a format change — but freeze should wait for it, so adopting an accelerator never silently changes emitted bytes.'

### [MINOR] parity/consistency — rem-parity-1.0-specification.md §7.3 (and A.3) (l.639) 
ISSUE: The normative digest vector's map is physically impossible under the format's own rules: (a) a sidecar tape file has total = 2H + P + 1 blocks with H ≥ 1 and P = S×m ≥ 1, so minimum 2·1+1+1 = 4 blocks — a 2-block sidecar cannot exist; (b) Section 9.2 requires protected_ordinal_start = epoch_id × S × k, so epoch_id 7 with range [0, 3) would require S × k = 0, which violates Section 6.6 validity (the 25-byte encoding and its SHA-256 do reproduce as stated; the defect is the example's realizability).
QUOTE: `[bootstrap(#0, 1 blk), object(#1, 3 blk, first ordinal 0), sidecar(#2, 2 blk, epoch 7, range [0, 3))]`
FIX: Regenerate the vector from a realizable map (e.g. epoch_id 0, sidecar block_count ≥ 4 consistent with a small (k, m, S)), updating the 7.3 byte listing, the SHA-256, and the Appendix A.3 derivation together — or, at minimum, add a sentence stating the vector exercises the digest function only and is deliberately not a valid tape map.

### [MINOR] parity/consistency — rem-parity-1.0-specification.md §15 (vs 13.5, 8.2.1, 17) (l.1392) 
ISSUE: Two failure modes the body requires to be typed have no matching taxonomy entry: (a) Section 13.5's post-reconstruction CRC mismatch is 'an unrecoverable result even though the matrix algebra succeeded', and Section 17 lists 'a reconstructed-block CRC mismatch' as a negative vector whose manifest must carry 'the expected Section 15 error name' — but Unrecoverable is defined only as 'more than m erasures in a stripe', which this is not; (b) Section 8.2.1's mandatory key-30 admission rejection ('MUST reject the write if the resulting mandatory row set could not fit') names no error, and BootstrapPayloadTooLarge is scoped to the Section 10.7 framing signal. (No name used in the body is missing from the taxonomy; the unused-by-name entries all carry section pointers, which is acceptable.)
QUOTE: Unrecoverable{stripe, lost_count, limit}   more than m erasures in a stripe
FIX: Either broaden Unrecoverable's description to cover reconstruction-output CRC mismatch (or add a distinct name, e.g. ReconstructedBlockCrcMismatch), and state which taxonomy name the 8.2.1 admission refusal maps to (extend BootstrapPayloadTooLarge's description to cover Section 8.2.1, or add a name).

### [MINOR] parity/consistency — rem-parity-1.0-specification.md §19.2 (vs 8.2.1) (l.1561) 
ISSUE: [RAO] is classified Informative, but Section 8.2.1 places MUST-level wire constraints on values defined only by RAO ('RAO encrypted-header key_id; all-zero is invalid'; 'metadata_frame_len; bounds [17, 16 MiB]'; 'manifest chunk range MUST fit...'), so an implementer of key 30 cannot conform without the RAO document — a normative dependency on an informative reference (the stated bounds do match RAO 1.0 §5.5.3/§9, verified).
QUOTE: - [RAO] — Rem Archive Object (RAO) Format, Version 1.0
FIX: Either move [RAO] to Section 19.1 Normative with a note that it is required only when bootstrap key 30 is implemented, or restate the two constraints self-containedly in Section 8.2.1 (16-byte nonzero identifier; uint in [17, 16777216]) so [RAO] can stay informative.

### [MINOR] parity/consistency — rem-parity-1.0-specification.md §12.3 item 1 (l.1208) 
ISSUE: `block_size_bytes` is a fixed-frame header field at offset 0x20 (Section 8.1), not a CBOR payload field; calling it 'the payload's' contradicts the frame table and could send an implementer looking for a payload key that does not exist (Section 8.4 states the same check correctly as 'its block_size_bytes').
QUOTE: the payload's `block_size_bytes` equals the read size
FIX: Replace 'the payload's `block_size_bytes`' with 'the frame's `block_size_bytes` (Section 8.1)'.

### [MINOR] parity/consistency — rem-parity-1.0-specification.md §8.3 (l.781) 
ISSUE: Normative MUSTs are imposed on policy parameters the same sentence declares 'operator-tunable and not normative', and the parameters themselves (byte/object-count floors, taper fractions, their 'divisors') are never defined anywhere in the document — 'strictly increasing divisors' is uninterpretable from the document alone, so the validity rules cannot be implemented or tested.
QUOTE: floors MUST be non-zero; taper fractions MUST lie in (0, 1], be descending, and have strictly increasing divisors
FIX: Either define the checkpoint-policy parameter model (what a floor, taper fraction, and divisor each is, and what they control) before stating validity MUSTs, or delete the validity clause and leave placement policy wholly out of scope as Section 1.5 does for other policies.

### [MINOR] parity/consistency — rem-parity-1.0-specification.md §9.2 vs 9.6/10.5 (l.868) 
ISSUE: The same quantity H is named `shard_index_block_count` in the sidecar header (9.2, offset 0x58) but `sidecar_header_block_count` in the footer (9.6, offset 0x38) and in the epoch-directory entry (10.5 key 6) — two names for one field across structures that Section 13.3 requires readers to cross-verify field-for-field.
QUOTE: shard_index_block_count u32 (H)
FIX: Pick one name (e.g. `sidecar_header_block_count`, since 9.4 calls H the 'header/index copy block count') and use it in all three tables, keeping '(H)' as the shorthand.

### [MINOR] parity/consistency — rem-parity-1.0-specification.md §Header table, 17, 19.2, Appendix C (l.1468) 
ISSUE: Repository-relative paths and internal artifact names appear in a document meant to stand alone: `crates/remanence-parity` (line 13), `fixtures/rem-parity-1/` (Section 17, in normative text about where conformance vectors live), `specs/rao-1.0-specification.md` (19.2), and the internal milestone 'Owner milestone: PAR-KEY30-RECOVERY' (Appendix C item 5); none is resolvable by an external implementer.
QUOTE: Static vectors live in the repository (`fixtures/rem-parity-1/`)
FIX: Cite the fixtures as a companion publication ('the REM-PARITY 1.0 test-vector archive, distributed with this specification'), give [RAO] a proper document citation without a filesystem path, drop the crate path from the header table, and remove the internal milestone identifier from Appendix C (the item's substance can stay).

### [MINOR] parity/editorial — rem-parity-1.0-specification.md §Front matter (l.13) 
ISSUE: The front matter cites the reference implementation by an internal repository-relative crate path that is meaningless to an external reader.
QUOTE: | Reference implementation | Remanence (`crates/remanence-parity`) |
FIX: Name the implementation as "Remanence" only, or point to a public locator; if the implementation pointer is kept, move it to an informative reference entry.

### [MINOR] parity/editorial — rem-parity-1.0-specification.md §10.7 (l.1118) 
ISSUE: A normative requirement is phrased in implementation-internal jargon ("typed too-large signal", "real framing attempt") and the surrounding text uses deployment-speak ("production slack margin", "a test-geometry allowance") below specification register.
QUOTE: The fit check MUST be the typed too-large signal from a real framing attempt, never an estimate.
FIX: Reword: "The Writer MUST determine fit by actually framing the complete bootstrap payload and observing whether it exceeds the block; it MUST NOT estimate the size." Replace "production slack margin" with "a slack margin of 4096 bytes" and "(a test-geometry allowance)" with "the margin does not apply for block sizes below 8 KiB".

### [MINOR] parity/editorial — rem-parity-1.0-specification.md §8.2 (l.716) 
ISSUE: The key-2 presence rule introduces an undefined category "minimal no-parity" that differs, without explanation, from key 1's "REQUIRED unless no-parity", leaving the digest record's presence rule ambiguous.
QUOTE: REQUIRED unless minimal no-parity
FIX: Use one consistent condition matching the body text ("REQUIRED unless the no-parity flag is set (see below)") for both keys, or explicitly define what distinguishes a "minimal" no-parity bootstrap.

### [MINOR] parity/editorial — rem-parity-1.0-specification.md §8.2 (l.719) 
ISSUE: The key-5 presence cell uses non-BCP-14 table shorthand that both mandates the field and defines absence semantics, an internally tense formulation that an implementer cannot map to requirement levels.
QUOTE: written always; absent ⇒ false
FIX: Reword: "Writers MUST include key 5; readers MUST treat an absent key 5 as false."

### [MINOR] parity/editorial — rem-parity-1.0-specification.md §15 (l.1405) 
ISSUE: A clearly normative prohibition is phrased with lowercase "may", which under RFC 8174 reads as plain prose and will be missed by an implementer scanning for requirement keywords.
QUOTE: No code path reachable from tape bytes may panic, crash, or allocate unboundedly
FIX: Reword with BCP 14 keywords: "Implementations MUST NOT panic, crash, or allocate unboundedly on any code path reachable from tape bytes."

### [MINOR] parity/editorial — rem-parity-1.0-specification.md §9.2 (l.852) 
ISSUE: Normative content (the endianness of all fields in the section) is carried only by an em-dash clause in the section heading, where it is invisible in the table of contents and against heading conventions.
QUOTE: ### 9.2. The Header Block (block 0 of each copy) — all little-endian
FIX: Shorten the heading to "The Header Block" and open the body with "All fields in this structure are little-endian; the header occupies block 0 of each copy." Apply the same to headings 9.6 (line 957) and 10.3 (line 1012).

### [MINOR] parity/editorial — rem-parity-1.0-specification.md §11.1 (l.1139) 
ISSUE: Several abbreviations are never expanded at first use: EOM (line 1139), LBA (line 276), SIMD (line 494), UUID (line 452), LTO (line 1564); and per RFC 7322 the Abstract's CBOR and HMAC (line 43) must also be expanded on first use there.
QUOTE: (any short write / EOM / completion-unknown ⇒ abandon)
FIX: Expand each at first use, e.g. "end of medium (EOM)", "logical block address (LBA)", "single instruction, multiple data (SIMD)", "universally unique identifier (UUID)", "Linear Tape-Open (LTO)", and "Concise Binary Object Representation (CBOR)" / "hashed message authentication code (HMAC)" in the Abstract.

### [MINOR] parity/editorial — rem-parity-1.0-specification.md §11.2 (l.1157) 
ISSUE: The defined conformance role "Writer" (Section 2.2) appears inconsistently in lowercase throughout Sections 11 and 14 (also "a writer MUST treat hard end-of-medium", line 1185; "seed the writer", line 1363), blurring whether the defined role or a generic writer is meant.
QUOTE: the writer holds `S × m` block-sized accumulators
FIX: Capitalize "Writer" wherever the Section 2.2 role is intended, and do a full pass for the other role names (Scanner, Recoverer, Resumer, Verifier).

### [MINOR] parity/editorial — rem-parity-1.0-specification.md §13.3 (l.1287) 
ISSUE: Scare quotes are used inside normative sentences to signal irony, which is below standards register and ambiguous in plain-text rendering.
QUOTE: a "valid" footer that contradicts the map is treated as an invalid footer
FIX: Reword without quotes: "a footer that parses successfully but contradicts the map entry is treated as invalid"; likewise replace 'MUST NOT "normalize" it' (line 706) with "MUST NOT reorder the fields to a uniform endianness" and 'some "trusted" input was wrong' (line 1330) with "an input classified as trusted was wrong".

### [MINOR] parity/editorial — rem-parity-1.0-specification.md §13.3 (l.1295) 
ISSUE: The coined adverb "footerlessly" and the rhetorical tag "that is its purpose" fall below specification register in a normative recovery procedure.
QUOTE: find and verify the tail copy footerlessly — that is its purpose
FIX: Reword: "The directory carries exactly the counts and hash needed to locate and verify the tail copy when the footer is unavailable (Section 10.1)."

### [MINOR] parity/editorial — rem-parity-1.0-specification.md §3.3 (l.306) 
ISSUE: "Load-bearing" is a colloquial architectural metaphor below standards-document register, in a sentence stating a core normative property.
QUOTE: The interleave is load-bearing:
FIX: Reword: "This interleave is essential to the damage-tolerance guarantee: `N ≤ S` physically consecutive data blocks land in `N` distinct stripes, ..."

### [MINOR] parity/editorial — rem-parity-1.0-specification.md §12.5 (l.1250) 
ISSUE: The parenthetical heading label "(normative guarantee)" is inconsistent with the document's "(Informative)" convention and redundant in a document where text is normative by default.
QUOTE: ### 12.5. Epoch Isolation (normative guarantee)
FIX: Retitle to "Epoch Isolation" and, if emphasis is wanted, state in the body "This section states a normative guarantee." Reserve parenthetical heading labels for "(Informative)".

### [MINOR] parity/editorial — rem-parity-1.0-specification.md §Abstract (l.33) 
ISSUE: The Abstract uses bold/italic markup and narrative flourishes ("the first block of the very file that describes it", line 46) that RFC abstracts — plain, self-contained prose — do not support.
QUOTE: a fixed **bootstrap block** written at the beginning of tape
FIX: Strip all markup from the Abstract, expand abbreviations at first use, and flatten the flourish to "including the case where the damaged block is the first block of the file describing it".

### [MINOR] parity/editorial — rem-parity-1.0-specification.md §16.3 (l.1441) 
ISSUE: "Compression-tainted" and "self-incriminating" are figurative courtroom language below register for the Security Considerations section.
QUOTE: makes a compression-tainted tape self-incriminating
FIX: Reword: "ensures that a tape written with compression enabled records that fact, so readers MUST reject it."

### [MINOR] parity/editorial — rem-parity-1.0-specification.md §18 (l.1536) 
ISSUE: "Chaos transport" is internal tooling jargon presented without definition in a conformance criterion.
QUOTE: write with injected damage (a chaos transport)
FIX: Reword: "write with injected damage (using a transport layer that deterministically injects media faults)".

### [MINOR] parity/editorial — rem-parity-1.0-specification.md §Appendix C (l.1774) 
ISSUE: An internal project-tracking milestone identifier appears in the specification text, tying the document to unpublished internal planning artifacts.
QUOTE: Owner milestone: PAR-KEY30-RECOVERY
FIX: Delete the milestone sentence and state the requirement generically: "Such a reader must exist before format freeze."

### [MINOR] parity/editorial — rem-parity-1.0-specification.md §2.3 (l.204) 
ISSUE: The watermark definition is grammatically off — ordinals do not emit sidecars — momentarily obscuring a load-bearing definition.
QUOTE: ordinals `< W` have emitted sidecars
FIX: Reword: "ordinals `< W` are covered by emitted sidecars."

### [MINOR] parity/implementability — rem-parity-1.0-specification.md §14 (step 3) (l.1353) 
ISSUE: Unreachable normative text: step 2 enforces T − W < S × k with W epoch-aligned, so the range [W, T) lies strictly inside one epoch and can never contain a complete epoch — yet step 3 says 'Any complete epochs encountered in [W, T) are re-encoded as rebuilt sidecars', implying a state step 2 just rejected. Readers cannot tell whether this is dead text or a hint that the bound is soft.
QUOTE: Any *complete* epochs encountered in `[W, T)` are re-encoded as rebuilt sidecars
FIX: Delete the sentence, or reword to cover the only real case ('the single open partial epoch is re-accumulated; if re-reading shows it complete at S × k ordinals, it is encoded as a rebuilt sidecar').

### [MINOR] parity/implementability — rem-parity-1.0-specification.md §12.1 (l.1195) 
ISSUE: The catalog fast-path is self-inconsistent as written: it requires 'tape UUID matches' yet says 'no physical scan occurs' — the tape's UUID can only come from reading a bootstrap block; and 'its recorded W' refers to a field the filemark map (7.1) does not have (W is derived per 7.2; the recorded value belongs to the accompanying digest record).
QUOTE: that map is authoritative and no physical scan occurs
FIX: Say 'no physical walk occurs beyond reading one bootstrap to obtain the tape UUID', and replace 'its recorded W' with 'the highest_protected_ordinal recorded in the catalog's digest record'.

### [MINOR] parity/implementability — rem-parity-1.0-specification.md §2.4 / 8.1 (l.697) 
ISSUE: The byte-range notation used for every CRC coverage ('CRC-64/XZ over bytes 0x00..0x2C', 0x00..0xB0, 0x00..0x78, 0..block_size−8) is never defined as end-exclusive; Section 9.5 switches to 'through 0x7F inclusive' phrasing, highlighting that two conventions are in play. All instances happen to be resolvable (inclusive would be self-referential), but an international audience should not have to deduce it.
QUOTE: | 0x2C | 8 | crc64_header | **u64 LE** | CRC-64/XZ over bytes 0x00..0x2C |
FIX: Add to Section 2.4: 'a byte range a..b denotes offsets a through b−1 inclusive (half-open)', and use it uniformly (rephrase 9.5 as 0x00..0x80).

### [MINOR] parity/implementability — rem-parity-1.0-specification.md §7.3 / A.3 (l.639) 
ISSUE: The normative digest vector's map is structurally impossible on a conformant tape: a sidecar of 2 blocks (minimum real sidecar is 2H+P+1 ≥ 4 blocks) with epoch_id 7 yet range [0, 3) (9.2 requires protected_ordinal_start = epoch_id × S × k). The SHA-256 is correct (independently verified), but an implementer whose pipeline validates entries before digesting will reject the vector's input and may conclude their validation is wrong.
QUOTE: sidecar(#2,
2 blk, epoch 7, range [0, 3))
FIX: Either label the vector explicitly as exercising the digest function only ('the entries are deliberately not a valid tape'), or substitute structurally plausible values (epoch 0, realistic block counts) and update the bytes and hash.

### [MINOR] parity/implementability — rem-parity-1.0-specification.md §5.2 / 9.5 (l.443) 
ISSUE: There are in-document vectors for CRC, Reed–Solomon, and the canonical digest, but none for the two remaining derived values: the HMAC-derived magics (no sample tape_uuid → 8-byte magic) and the canonical metadata hash (domain string + header bytes + entries). Until the image fixtures exist, an implementer cannot confirm either derivation, including the exact label bytes and truncation.
QUOTE: magic = HMAC-SHA-256(key = tape_uuid[16 bytes], message = LABEL)[0..8]
FIX: Add a normative vector to 5.2 (a fixed example UUID and the resulting four 8-byte magics) and a small worked metadata-hash vector to 9.5 or Appendix A.

### [MINOR] parity/implementability — rem-parity-1.0-specification.md §13.4 (l.1315) 
ISSUE: The trusted-shard condition depends on 'the durable boundary', which is defined only via off-tape commit records (3.4) and is therefore unknowable in the catalog-less recovery case the format is designed for; the reader must guess that the validated map scope stands in for the boundary (true by construction, since a digest is written only over committed files — but the document never says so).
QUOTE: AND — for data peers — its object tape file is inside the durable boundary
FIX: Add one sentence: 'In catalog-less recovery the validated map scope bounds the durable boundary: every tape file inside a validated digest scope was committed when that digest was written, so scope membership satisfies this condition.'

### [MINOR] parity/implementability — rem-parity-1.0-specification.md §12.4 (l.1236) 
ISSUE: The overlay fallback 'a structurally discovered parity_map' does not say how to choose when several parity_map files survive on tape (earlier checkpoint emissions have stale scopes; sequence is declared diagnostic-only in 10.4), nor what happens when the chosen directory's scope differs from the authoritative digest record's tape_file_count.
QUOTE: → a
structurally discovered parity_map → none
FIX: Specify the selection rule (e.g. the parity_map with the greatest scope_tape_file_count whose payload verifies; on tie, greatest sequence) and state that digest validation still runs over the digest record's own tape_file_count after overlay truncation, with mismatch handling.

### [MINOR] parity/implementability — rem-parity-1.0-specification.md §14 (step 5) (l.1364) 
ISSUE: The bootstrap-sequence seeding rule 'at least the count of committed bootstraps' is necessary but not sufficient: 8.1/8.3 require sequences strictly increasing but not dense, so a count-based seed can violate strict increase on a tape with sparse sequences (e.g. committed sequences 0, 5, 9 → count 3).
QUOTE: the next bootstrap sequence (at least the
   count of committed bootstraps)
FIX: Reword to 'strictly greater than the highest committed bootstrap sequence' (the count-based floor can stay as a parenthetical only if density is also mandated somewhere).

### [MINOR] parity/implementability — rem-parity-1.0-specification.md §Front matter / skeleton (l.7) 
ISSUE: The customary RFC skeleton is incomplete for the stated publication path: there is no Status of This Memo, no IANA Considerations section (RFC 7322 expects one even if it states 'this document has no IANA actions'), and no Authors' Addresses/Acknowledgements; the Abstract also contains section cross-references and requirements-flavored prose that RFC style disallows in abstracts.
QUOTE: | Status | Draft for review |
FIX: Add the missing skeleton sections (IANA Considerations: none; Authors' Addresses; Status of This Memo appropriate to the publication venue) and trim the Abstract to a self-contained summary without internal cross-references.

### [MINOR] parity/implementability — rem-parity-1.0-specification.md §Front matter (l.13) 
ISSUE: The front-matter table cites the reference implementation by a repository-relative path ('crates/remanence-parity'), which is meaningless to an external implementer; naming Remanence as provenance is fine, the internal path is not.
QUOTE: | Reference implementation | Remanence (`crates/remanence-parity`) |
FIX: Cite the implementation by name and public URL (or drop the path): 'Reference implementation: Remanence (see <public repository URL>)'.

### [MINOR] parity/implementability — rem-parity-1.0-specification.md §19.2 (l.1562) 
ISSUE: The [RAO] reference locates the companion specification by a repository-relative path ('specs/rao-1.0-specification.md') rather than a public citation, so the reference is unresolvable outside the author's repo.
QUOTE: (`specs/rao-1.0-specification.md`): the reference payload format
FIX: Cite RAO as a published document (title, version, date, and public locator/DOI/URL), consistent with how a standalone spec must be referenced.

### [MINOR] parity/standalone — rem-parity-1.0-specification.md §13.6. Bulk Recovery (Informative) (l.1336) 
ISSUE: Deployment-specific narration of the author's implementation's tuning constants inside the specification body; the reader has no access to the implementation these facts describe.
QUOTE: The reference implementation bounds its planning windows at 1024 stripes and its recovery cache at 8 GiB
FIX: Rephrase implementation-neutrally: "An implementation might, for example, bound its planning windows (e.g. 1024 stripes) and cap its recovery cache; both are quality-of-implementation choices, not format rules." If an Implementation Status section is added, implementation-specific constants belong there.

### [MINOR] parity/standalone — rem-parity-1.0-specification.md §17. Test Vectors (l.1479) 
ISSUE: The pinned-at-generation paragraph narrates the author's internal fixture-generation process in the future tense; in the published document this must read as a statement about normative published values, not a promise about the author's workflow.
QUOTE: produced by the reference implementation when the fixtures are first generated, independently re-derived, then frozen
FIX: At publication replace with: "Image-level byte vectors are normative as published; before publication each was generated by one implementation and independently re-derived by a second implementation in a different language or library." Drop the **[pinned-at-generation]** draft tag once the vectors exist.

### [MINOR] parity/standalone — rem-parity-1.0-specification.md §10.7. Inline versus External (Writer) (l.1117) 
ISSUE: A normative writer rule is framed in the author's deployment vocabulary ("production", "test-geometry allowance"), implying a production/test distinction that has no meaning to an external implementer.
QUOTE: a production slack margin of 4096 bytes; the margin is waived below 8 KiB blocks (a test-geometry allowance)
FIX: Keep the rule, drop the framing: "...fits the block with a slack margin of 4096 bytes; the margin is waived for block sizes below 8 KiB (block sizes that small are expected only in test geometries)."

### [MINOR] parity/standalone — rem-parity-1.0-specification.md §4.2. What the Format Provides to Objects (l.359) 
ISSUE: The operations checkpoint() and finish() are written in code-call syntax and first used (here and in Section 8.2.1, line 770) long before they are defined in Section 11.3, with no forward reference — at first read they look like an API the reader is presumed to have.
QUOTE: `finish()` closes the final epoch, so on a finished tape every object
FIX: At first use write "the finish operation (Section 11.3)" and add the cross-reference at line 770 as well; RFC prose style prefers named operations ("the checkpoint operation", "the finish operation") over function-call syntax, though keeping the code names is acceptable once they carry a forward reference at first use.

### [MINOR] parity/structure — rem-parity-1.0-specification.md §Abstract (l.45) 
ISSUE: The Abstract is not fully self-contained: the variable m and the term "stripe" (and "parity epoch" at line 36) are used without any in-abstract gloss, contrary to RFC abstract conventions.
QUOTE: recover up to *m* damaged blocks per stripe
FIX: Gloss the terms inline, e.g. "recover up to a configured number of damaged blocks per Reed-Solomon codeword (stripe)", or drop the parameterized claim from the abstract.

### [MINOR] parity/structure — rem-parity-1.0-specification.md §18 (l.1525) 
ISSUE: Section 18 is written as the author's internal pre-freeze process (reference-implementation backlog, in-repo fixtures, fuzzing program) rather than implementer-facing conformance criteria, which will read as stale process notes once the document is published as a frozen specification.
QUOTE: with its conformance backlog against this document closed
FIX: At publication, recast Section 18 as "Conformance" stating what an implementation must satisfy (roles, vectors, damage matrix) and move the freeze-process history to the status statement or an informative appendix.

### [MINOR] parity/structure — rem-parity-1.0-specification.md §4.4 (l.413) 
ISSUE: The informative tar binding relies on POSIX tar semantics (end-of-archive records, `tar -b` blocking) with no citation of IEEE Std 1003.1 (and no edition), so the claim is not traceable.
QUOTE: **Plain tar.** A POSIX tar archive zero-padded to a block multiple
FIX: Add an informative reference "[POSIX] IEEE Std 1003.1-2024, The Open Group Base Specifications" (naming the pax/ustar interchange format) and cite it here.

### [MINOR] parity/structure — rem-parity-1.0-specification.md §8.1 (l.693) 
ISSUE: The field is named "uuid" but the document never states whether it must be an RFC 9562 UUID (no citation) nor states any generation/uniqueness requirement, even though the HMAC-derived-magic identity mechanism (Sections 5.2, B.4) depends on tape_uuid values being distinct across tapes.
QUOTE: | 0x10 | 16 | tape_uuid | raw bytes | the tape's identity; the HMAC key of Section 5.2 |
FIX: Either state "tape_uuid is an opaque 16-byte identifier; a Writer MUST generate it with sufficient entropy to be unique per tape (e.g. an [RFC9562] version-4 UUID)" with RFC 9562 in the references, or rename to a neutral term and state the uniqueness requirement.

### [MINOR] rao/bcp14 — rao-1.0-specification.md §13 (l.2229) 
ISSUE: MUSTs in Section 13 bind the author's fixture suite rather than any conformance role ('The plaintext suite MUST include at least...' line 2229; 'at least one vector MUST use DEFAULT_CHUNK_SIZE' line 2218), and the suite lives in a repository ('fixtures/rao/') the reader has no access to — the requirements are untestable from the document alone.
QUOTE: The plaintext suite MUST include at least: an **empty object**
FIX: Restate as document/freeze-process obligations without BCP 14 force (they already appear as freeze criterion 2 in Section 14), or publish the vectors in the document/appendix and reserve MUSTs for implementer-facing assertions ('an implementation MUST produce these bytes for these inputs').

### [MINOR] rao/bcp14 — rao-1.0-specification.md §Appendix C (l.2656) 
ISSUE: Appendix C carries a capitalized MUST while Appendices A and B are explicitly informative and C's status is undeclared; the same re-derivation requirement appears in Section 14 criterion 2 stated without any keyword — the one requirement, two places, two strengths.
QUOTE: They MUST additionally be
re-derived by an independent second implementation before freezing
FIX: Mark Appendix C informative/editorial (or fold it into the removable editorial note), and replace the MUST with a cross-reference: 'per freeze criterion 2 (Section 14)'; if the requirement is normative, keyword it once in Section 14 only.

### [MINOR] rao/bcp14 — rao-1.0-specification.md §3.4 (l.358) 
ISSUE: A SHOULD is scoped to the author's own deployment ('in the Remanence deployment the catalog records ... and readers SHOULD cross-check'), leaving unclear whether the SHOULD binds all conformant readers or only those in that deployment.
QUOTE: in the
Remanence deployment the catalog records each copy's representation and
readers SHOULD cross-check rather than sniff
FIX: Generalize the condition: 'When a catalog records each copy's representation, readers SHOULD cross-check the recorded representation rather than sniff.'

### [MINOR] rao/bcp14 — rao-1.0-specification.md §3.4 (l.350) 
ISSUE: Lowercase 'must' inside the normative representation-detection procedure (itself introduced by 'a Reader MUST decide as follows'), so the reader cannot tell whether the ustar-header/format-gate condition carries BCP 14 force.
QUOTE: the input must begin
with a valid ustar header record
FIX: Capitalize or restructure: 'Otherwise the Reader MUST attempt the plaintext representation: the input MUST begin with a valid ustar header record, typeflag g, and the global header MUST pass the REMANENCE.format_id = rao-v1 gate.'

### [MINOR] rao/bcp14 — rao-1.0-specification.md §12.2 (l.2091) 
ISSUE: A prohibition in a normative security section is expressed with lowercase 'may' ('No future revision may "simplify" ...'), which under RFC 8174 carries no keyword force despite stating a load-bearing constraint.
QUOTE: No future revision may "simplify" the key
schedule by merging them.
FIX: Use the keyword form consistent with the other freeze statements: 'Future revisions MUST NOT unify these keys or derive one from the other.'

### [MINOR] rao/bcp14 — rao-1.0-specification.md §11 (l.1940) 
ISSUE: A prohibition is stated with lowercase 'may' ('No code path reachable from object bytes may panic...') in a normative section, duplicating Section 12.9's 'Reader implementations MUST NOT panic, crash, or invoke undefined behavior' with a different verb set and no keyword — same requirement, two strengths.
QUOTE: No code path reachable
from object bytes may panic, crash, or allocate unboundedly (Section 12.9).
FIX: Replace with a cross-reference carrying no independent force: 'Section 12.9 states the MUST NOT requirements on panic, crash, and unbounded allocation for code paths reachable from object bytes.'

### [MINOR] rao/bcp14 — rao-1.0-specification.md §12.5 (l.2122) 
ISSUE: Lowercase 'must' states a deployment obligation in the normative Security Considerations ('Deployments for which ... is itself sensitive must handle that above the format'), leaving both its strength and its out-of-scope subject ambiguous.
QUOTE: sensitive must handle that above the format
FIX: Make it explicitly non-keyword guidance: 'Deployments for which existence, identifier, or approximate size is sensitive need to handle that above the format (padding payloads before building, opaque object naming); RAO defines no padding mechanism.'

### [MINOR] rao/bcp14 — rao-1.0-specification.md §5.3 (l.1132) 
ISSUE: Passive MUST NOT with an ambiguous subject: 'Human-readable epoch labels MUST NOT be stored in the header' sits in the paragraph describing the external key registry — the reader cannot tell whether it binds the Sealer, the registry minting key_id values, or both.
QUOTE: Human-readable epoch labels MUST NOT be stored in the header.
FIX: Name the actor: 'A Sealer MUST NOT encode human-readable epoch labels in the header; registries are expected to mint opaque key_id values' (or bind entirely to the Sealer if that is the intent).

### [MINOR] rao/bcp14 — rao-1.0-specification.md §5.3 (l.1134) 
ISSUE: An absolute prohibition is stated without a keyword ('Implementations never fetch keys'), directly adjacent to keyworded requirements, so its normative status is unclear; additionally 'SHOULD use exactly 32 uniformly random bytes' (line 1137) binds 'Implementations' although root-key generation is the external registry's job per the preceding paragraph.
QUOTE: Implementations never fetch keys: callers supply root key material
FIX: Keyword the first ('Implementations MUST NOT fetch key material themselves; callers supply it through an in-memory interface') and rebind the second to the generating party ('Root key material SHOULD be exactly 32 uniformly random bytes — a registry/deployment obligation').

### [MINOR] rao/bcp14 — rao-1.0-specification.md §4.9 (l.1006) 
ISSUE: Untestable MUST: 'range-read implementations MUST say so rather than imply hash-verified content' specifies no observable behavior (say what, to whom, through what interface), so conformance cannot be evaluated.
QUOTE: range-read implementations MUST say
so rather than imply hash-verified content
FIX: State the testable obligation: 'a range-read implementation MUST NOT report a partial-range read as hash-verified; it MUST report the range's integrity as block-CRC-level only.'

### [MINOR] rao/bcp14 — rao-1.0-specification.md §3.3 (l.339) 
ISSUE: Keywords bind 'Backends', which Section 8.1 defines as needing 'no format knowledge' — 'Backends MUST be able to scrub any stored copy by stored_digest alone' (3.3 item 4) and 'Backends SHOULD record per copy: location, representation, ...' (8.1, line 1814) are requirements on out-of-scope storage systems rather than on a conformance role of this format.
QUOTE: Backends MUST be able to scrub any stored copy by `stored_digest` alone
FIX: Restate item 4 as a format guarantee ('the format guarantees that any stored copy is scrubbable by stored_digest alone, without keys or format knowledge') and 8.1's list as deployment guidance on what to record per copy.

### [MINOR] rao/bcp14 — rao-1.0-specification.md §5.2 (l.1094) 
ISSUE: Several MUSTs bind the document's own future editors or an implementation's internal refactoring rather than a testable conformance subject: 'Future revisions MUST NOT assign additional valid values' (5.2, line 1094), 'MUST NOT change any rule a 1.0 Reader enforces' (10, line 1907), 'they MUST remain descriptive' (10, line 1922), and 'Three properties are deliberate and MUST survive any refactoring' (5.4, line 1161 — the properties are already individually normative, so this meta-MUST binds nothing testable).
QUOTE: Future revisions
MUST NOT assign additional valid values
FIX: Rephrase as reservation/definition statements: 'This document reserves all other values; a revision assigning them is a new format (RAO2).' Delete or de-keyword the 5.4 meta-sentence ('Three properties are deliberate:').

### [MINOR] rao/bcp14 — rao-1.1-specification.md §2.1 (l.65) 
ISSUE: MUSTs whose grammatical subject is the wire artifact rather than a role: 'An entry with no preserved xattrs MUST emit an empty metadata_preservation_data' (line 65) and 'Hardlink entries MUST emit an empty metadata_preservation_data' (line 68) — entries emit nothing; the Writer does, and reader-side enforcement (reject or ignore a violating object?) is left unstated.
QUOTE: An entry with no preserved xattrs MUST emit an **empty**
`metadata_preservation_data`
FIX: Bind to the Writer: 'A Writer MUST emit an empty metadata_preservation_data for entries with no preserved xattrs and for hardlink entries', and state the reader-side consequence if any (e.g. Verifier reports a non-empty hardlink container as a nonconformity).

### [MINOR] rao/bcp14 — rao-1.0-specification.md §4.10 (l.1039) 
ISSUE: Lowercase 'requires' states a conformance condition with an ambiguous subject: 'Conformance requires demonstrated extraction equality by GNU tar, bsdtar, and Python tarfile' reads as binding every implementation, but the referenced condition is actually document freeze criterion 3 (Section 14).
QUOTE: Conformance requires demonstrated extraction equality by GNU tar, bsdtar, and
Python `tarfile` (Section 14).
FIX: Disambiguate: 'Freeze criterion 3 (Section 14) demonstrates extraction equality with GNU tar, bsdtar, and Python tarfile' — or, if each implementation is meant to demonstrate it, say so with a keyword and a named role.

### [MINOR] rao/bcp14 — rao-1.0-specification.md §11 (l.1937) 
ISSUE: Strength tension: exposing typed errors is only 'SHOULD', yet in the next sentence 'Names are normative for the test-vector manifests' — the negative vectors (Section 13.5) assert specific error names, which an implementation that skipped the SHOULD could not satisfy.
QUOTE: Implementations SHOULD expose typed errors equivalent to the taxonomy below.
Names are normative for the test-vector manifests
FIX: Clarify the binding: 'The taxonomy is RECOMMENDED as an API surface; an implementation validated against the Section 13 negative vectors MUST map its failures to these names (surface syntax is not constrained).'

### [MINOR] rao/consistency — rao-1.0-specification.md §2.2 (l.182) 
ISSUE: Both role cross-references point at Section 4.8 ('End of Archive'), but the Builder workflow and the Planner definition ('The Planner computes the entire layout … without payload bytes') live in Section 4.9 ('Writer, Planner, and Reader Obligations'). Looks like a renumbering artifact; the other role references (Sealer → 5.8, Reader → 4.9/5.9) are correct.
QUOTE: the **Builder** (produces the canonical plaintext stream; Section 4.8) … **Planner**: computes a plaintext object's exact layout and block count without payload bytes (Section 4.8)
FIX: Change both '(Section 4.8)' references in the Writer/Builder and Planner role bullets to '(Section 4.9)'.

### [MINOR] rao/consistency — rao-1.0-specification.md §5.2 (l.1124) 
ISSUE: Wrong cross-reference: Section 13.4 is 'RAO-TV-D1 — Default Chunk Size'. The single-fault negative vectors ('Each contains exactly one fault and asserts the mapped error') are Section 13.5.
QUOTE: conformance test vectors contain exactly one fault each (Section 13.4)
FIX: Change '(Section 13.4)' to '(Section 13.5)'.

### [MINOR] rao/consistency — rao-1.0-specification.md §11.2 (l.1986) 
ISSUE: The taxonomy entry contradicts §5.2. On read, the header object_id field is fixed at 64 bytes, so 'longer than 64 bytes' is unreachable; and §5.2 explicitly assigns the over-length case to the writer at sealing time with a different error: 'an object whose object_id exceeds 64 UTF-8 bytes cannot be stored in the encrypted representation; writers MUST reject such input at sealing time (InvalidInput)' — consistent with §5.8 step 1 and the §13.5 writer-side vector. Since §11 declares these names normative for test-vector manifests, the conflicting attribution is a real ambiguity.
QUOTE: InvalidObjectIdField   object_id field empty, longer than 64 bytes, interior NUL, or invalid UTF-8
FIX: Drop 'longer than 64 bytes' from the InvalidObjectIdField description (leaving: empty/all-NUL, interior NUL, invalid UTF-8 — the read-side faults), leaving the >64-byte sealing input under InvalidInput as §5.2/§5.8 already specify.

### [MINOR] rao/consistency — rao-1.0-specification.md §11.1/11.2 (l.1969) 
ISSUE: Three taxonomy entries are never referenced anywhere in the body: SourceIo and TapeIo (§11.1) and Io (§11.2). Every I/O-failure situation the body describes is either unnamed ('a short block read is a hard error', §4.9 step 1) or mapped to a different name (IncompleteBlockWrite). Conversely, every error name used in the body does appear in the taxonomy, so the mismatch is one-directional.
QUOTE: SourceIo   payload source read failure (not a format violation)
FIX: Either cite these names at the obligations that raise them (e.g. §4.9 step 1 short block read → TapeIo; §4.6.5/§7.2 payload streaming failure → SourceIo; §5.9 read failure → Io), or add a sentence to §11 stating they are the generic I/O buckets that no format rule raises directly.

### [MINOR] rao/consistency — rao-1.0-specification.md §4.2 vs 5.2 (l.404) 
ISSUE: §4.2 says chunk_size has no format-defined maximum, but the §5.2 envelope header encodes chunk_size as a uint32, so an object with chunk_size ≥ 2^32 is valid in the plaintext representation yet unrepresentable in the encrypted one. This exact asymmetry is acknowledged for the 64-byte object_id cap (§5.2 'the cap is an envelope constraint' and Appendix C item 2) but is nowhere acknowledged for chunk_size, and no sealing-time error is assigned.
QUOTE: The format defines no maximum; operational bounds come from drive block-size limits.
FIX: Add the same acknowledgment used for object_id: note in §4.2 or §5.2 that the encrypted representation caps chunk_size at 2^32 − 512 (uint32, multiple of 512) and that sealing an object with a larger chunk_size fails with InvalidInput; optionally add it to Appendix C.

### [MINOR] rao/consistency — rao-1.1-specification.md §2.1/2.2 (l.57) 
ISSUE: The delta twice relies on an operation called 'wrapping' a file (§2.1 for non-UTF-8 xattr names, §2.2 'causes the **file to be wrapped** rather than annotated') that is defined nowhere in either RAO 1.0 or 1.1. An external implementer cannot determine what a 'wrapped' file is or how it appears on the wire; as written it reads as a reference to an unpublished internal mechanism.
QUOTE: the ingest layer drops-and-records it, or wraps the file
FIX: Either define 'wrapping' (even one sentence: e.g. the ingest layer archives the file inside a container file of its own making, outside this format's scope) or delete the term and state only that such names/values are not representable and their handling is an ingest-policy matter outside the format.

### [MINOR] rao/editorial — rao-1.0-specification.md §1.1, goal 3 (and Abstract) (l.83) 
ISSUE: "PFR" is introduced as a parenthetical abbreviation of a differently worded phrase and is not expanded as "partial file restore" until the Section 6 title.
QUOTE: Closed-form byte-range addressing (PFR).
FIX: At line 37 (Abstract) write "partial file restore (PFR)", and in goal 3 write "Closed-form byte-range addressing for partial file restore (PFR)."

### [MINOR] rao/editorial — rao-1.0-specification.md §1.3 (and 5.3, 5.4, 12.1, B.11) (l.125) 
ISSUE: Several abbreviations are never expanded at first use: DR (125), HSM and KMIP (1131), PRF (1168), VM (1224), RNG (2058), CSPRNG (2623), CRC (145).
QUOTE: copy-2 | offsite/DR + cloud blob
FIX: Expand each at first use per RFC 7322: "disaster recovery (DR)", "hardware security module (HSM)", "Key Management Interoperability Protocol (KMIP)", "pseudorandom function (PRF)", "virtual machine (VM)", "random number generator (RNG)", "cryptographically secure pseudorandom number generator (CSPRNG)", "cyclic redundancy check (CRC)".

### [MINOR] rao/editorial — rao-1.0-specification.md §3.3 (also 4.7.2, B.3) (l.335) 
ISSUE: "iff" is mathematical shorthand, not RFC prose; it appears in normative sentences at lines 335, 875, and 2541.
QUOTE: Copies share it **iff** they wrap the identical canonical byte
FIX: Replace all three with "if and only if" (and drop the bold at line 335).

### [MINOR] rao/editorial — rao-1.0-specification.md §3.4 (l.359) 
ISSUE: "Sniff" is colloquial in a SHOULD-level sentence.
QUOTE: readers SHOULD cross-check rather than sniff
FIX: "...readers SHOULD cross-check the cataloged representation rather than rely on content detection."

### [MINOR] rao/editorial — rao-1.0-specification.md §1.3 (l.130) 
ISSUE: "Baskets" is an eggs-in-baskets colloquialism where the referent is the three copies.
QUOTE: cannot corrupt all three baskets identically
FIX: "...a latent bug in the RAO writer cannot corrupt all three copies identically..."

### [MINOR] rao/editorial — rao-1.0-specification.md §1.1, goal 1 (l.73) 
ISSUE: "Non-negotiable" is negotiation rhetoric, below standards register for the lead design goal.
QUOTE: **Plaintext longevity is non-negotiable.**
FIX: "**Plaintext longevity.** A plaintext RAO object is a fully valid POSIX pax tar archive..." (the following sentences already carry the substance), or "Plaintext longevity is the primary design requirement."

### [MINOR] rao/editorial — rao-1.0-specification.md §4.9 (l.1016) 
ISSUE: "Rejects where silence would lie" is a rhetorical flourish summarizing normative behavior; "slightly-foreign" in the same sentence is an informal coinage (and -ly adverbs should not be hyphenated).
QUOTE: and rejects where silence would lie
FIX: "A conformant Reader tolerates specified foreign-archive variations where safe (...) and rejects deviations whose silent acceptance would misrepresent the archive's content (...)."

### [MINOR] rao/editorial — rao-1.0-specification.md §5.4.1 (l.1208) 
ISSUE: "Astronomically improbable" is an editorializing intensifier in a normative sentence that already states the probability.
QUOTE: In the astronomically improbable case (probability 2^−128)
FIX: "If the result is all zero (probability 2^-128) — a value reserved as invalid in the header — the Sealer MUST increment `ctr` and re-derive."

### [MINOR] rao/editorial — rao-1.0-specification.md §5.4.1 (l.1198) 
ISSUE: One sentence defines five derivation inputs with two levels of nested parentheticals and an em-dash aside, obscuring the definition of `metadata_hash`.
QUOTE: where `"rao1-salt-v1"` is `LABEL_SALT` (12 ASCII bytes, no terminator), `ctr`
FIX: Split into one sentence per input; move the rationale aside ("the input exists so that anything the metadata_key will ever encrypt is bound into the key derivation") to its own sentence or to Appendix B.

### [MINOR] rao/editorial — rao-1.0-specification.md §5.4.1, property 1 (also 12.1) (l.1218) 
ISSUE: Property 1 is a single ~70-word sentence with a parenthetical, two em-dash insertions, and a trailing causal clause; it also uses the crypto-slang "grind" (again at line 2055, "let alone grind for").
QUOTE: Objects improperly *sharing* an `object_id` (an orchestrator bug; identifiers are unique by system invariant) still derive distinct salts
FIX: Split into three sentences (identifier reuse premise; distinct-salt consequence; residual 2^-128 bound), and replace "compute, let alone grind, salt values" with "compute salt values, let alone search for collisions among them".

### [MINOR] rao/editorial — rao-1.0-specification.md §5.10 (l.1583) 
ISSUE: The geometry-derivation bullet packs MUST-level requirements, a bolded advisory rule, and a multi-clause worked example into one list item spanning 16 lines, obscuring the normative content.
QUOTE: **Keyless error classification is advisory**: the derivation consumes the input length itself
FIX: Promote the bullet to prose: one paragraph for the MUST (derive geometry, reject inconsistency, verify footer and fill); one paragraph for the advisory-classification rule; the appended-block example as a separate informative sentence without bold.

### [MINOR] rao/editorial — rao-1.1-specification.md §Front matter (l.10) 
ISSUE: The base specification is cited by repository filename rather than by document title/reference, which breaks once the documents are published standalone.
QUOTE: Base | `rao-1.0-specification.md` (normative for everything not restated here)
FIX: "Base | Rem Archive Object (RAO) Format, Version 1.0 (normative for everything not restated here)" with a matching entry in a short references section.

### [MINOR] rao/editorial — rao-1.1-specification.md §1 (l.39) 
ISSUE: Sentence-fragment rhetoric ("No misinterpretation, no rejection — which is exactly why...") and "a minor" used as a bare noun are below standards register.
QUOTE: No misinterpretation, no rejection — which is exactly why this is a minor and not
FIX: "A 1.0 Reader neither misinterprets nor rejects such an object; this is why the change is a minor version rather than a new `format_id` (RAO 1.0, Section 10)."

### [MINOR] rao/editorial — rao-1.1-specification.md §2.2 (also 2.1) (l.82) 
ISSUE: Telegraphic prose with internal policy jargon: "built-in junk baseline", "ruleset-selected denylist ... stance", and the coined verb "drops-and-records" (line 56).
QUOTE: a built-in junk baseline is always dropped
FIX: Rewrite as complete sentences: "A built-in baseline of attributes known to carry no archival value is always dropped. The remainder is governed by a per-ruleset denylist or allowlist; the fail-safe default is the denylist. Every dropped attribute is recorded." At line 56: "the ingest layer omits it and records the omission, or wraps the file".

### [MINOR] rao/editorial — rao-1.1-specification.md §2.1 (l.60) 
ISSUE: Bold-as-emphasis inside normative bullets ("**no base64**", "**empty**", "**purely additive and fully backward-compatible**") will not survive RFC-format conversion and carries the requirement typographically.
QUOTE: so raw bytes are stored directly — **no base64**
FIX: Remove the emphasis and state plainly: "the raw attribute bytes are stored directly, with no intermediate text encoding"; "MUST emit an empty `metadata_preservation_data`"; "It is additive and backward-compatible:".

### [MINOR] rao/editorial — rao-1.1-specification.md §2.1, scope paragraph (l.74) 
ISSUE: "mode-beyond-`executable`" is an awkward ad hoc coinage, and "ACLs" is not expanded at first use.
QUOTE: mode-beyond-`executable` and ACLs are out of scope for 1.1
FIX: "...and file mode bits other than the executable bit, together with access control lists (ACLs), are out of scope for 1.1."

### [MINOR] rao/implementability — rao-1.1-specification.md §2.1 (l.57) 
ISSUE: 'Wraps the file' (also 'causes the file to be wrapped' at line 85) is an undefined operation from the author's internal ingest design; an implementer cannot determine what a 'wrapped' file is or whether it has any format-level footprint.
QUOTE: the ingest layer drops-and-records it, or wraps the file
FIX: Either define wrapping (if it has any RAO-visible representation) or drop the term and state only the format-level fact: xattr names that are not valid UTF-8 are not representable in `metadata_preservation_data` and their handling is out of scope.

### [MINOR] rao/implementability — rao-1.0-specification.md §11.2 / 5.2 (l.1986) 
ISSUE: Self-contradiction in the error taxonomy: Section 5.2 says an `object_id` exceeding 64 bytes is rejected at sealing time with `InvalidInput` (and the field is fixed-width, so a reader can never observe >64 bytes), yet 11.2 assigns 'longer than 64 bytes' to `InvalidObjectIdField`; since names are normative for test-vector manifests, the 13.5 'object_id > 64 bytes' writer vector has two candidate expected errors.
QUOTE: InvalidObjectIdField            object_id field empty, longer than 64 bytes,
FIX: Remove 'longer than 64 bytes' from the `InvalidObjectIdField` description (it is reader-side and structurally impossible) and note that the oversize case is writer-side `InvalidInput` per 5.2/5.8 step 1.

### [MINOR] rao/implementability — rao-1.0-specification.md §5.2 (l.1124) 
ISSUE: Wrong cross-reference: single-fault negative vectors are Section 13.5 ('Each contains exactly one fault'); 13.4 is the RAO-TV-D1 default-chunk-size positive vector.
QUOTE: conformance test vectors contain exactly one fault each (Section 13.4)
FIX: Change '(Section 13.4)' to '(Section 13.5)'.

### [MINOR] rao/implementability — rao-1.0-specification.md §4.7.2 (l.873) 
ISSUE: The top-level key table is explicitly '(shown in encoded sort order)' but the per-entry table is not and does not say so — the true encoded order interleaves the conditional keys (`entry_type` sorts between `file_id` and `executable`; `link_target` between `file_sha256` and `first_chunk_lba`), so an implementer who hard-codes the table order for non-regular entries emits maps every conformant decoder rejects with `Cbor`.
QUOTE: | `entry_type` | text | OPTIONAL; absent means `regular`;
FIX: Add a note to the per-entry table that it is grouped base-then-conditional and NOT in encoded sort order, and state the actual encoded order for non-regular entries (path, file_id, entry_type, executable, size_bytes, chunk_count, [file_sha256,] link_target, first_chunk_lba, metadata_preservation_data).

### [MINOR] rao/implementability — rao-1.0-specification.md §4.9 (l.1009) 
ISSUE: Reader behavior when a stream reaches valid tar EOF without ever containing a manifest entry is unspecified: one implementation reports success (delivering all entries), another rejects with `Parse` because 4.1 makes the manifest mandatory — divergent acceptance of the same input.
QUOTE: Capture the entry whose effective path is `_remanence/manifest.cbor` as the manifest bytes.
FIX: State explicitly whether a Reader MUST reject a stream whose final entry is not `_remanence/manifest.cbor` (presumably `Parse`, restore mode) or may deliver it, and which error applies.

### [MINOR] rao/implementability — rao-1.0-specification.md §4.6.6 / 4.7.2 (l.772) 
ISSUE: Dangling obligation: 4.6.6 delegates duplicate-`path`/`file_id` detection to Section 4.7, but 4.7.2's Consumer obligations and schema constraints nowhere require checking that `file_entries` contains no duplicate `path` or `file_id` — the invariant is unverifiable from the rules as written.
QUOTE: Verifiers and Consumers catch duplicates via the manifest (Section 4.7)
FIX: Add to the 4.7.2 constraints (or Consumer obligation 2) that `path` values and `file_id` values in `file_entries` MUST be pairwise distinct and the manifest's own `file_id` distinct from all of them, rejected with `ManifestInvalid`.

### [MINOR] rao/implementability — rao-1.0-specification.md §4.6.2 / 4.6.3 (l.666) 
ISSUE: Whether zero-payload entries (empty files, hardlinks, symlinks, directories) carry a `REMANENCE.pad` record is never stated — 'As needed' invites omission while the 4.6.3 algorithm always includes one for non-empty entries; a writer that emits an empty pad record on zero-payload entries produces different bytes from one that omits it, and the empty-file/symlink vectors are pinned only at generation.
QUOTE: | `REMANENCE.pad` | As needed | Alignment filler (Section 4.6.3);
FIX: State explicitly: non-empty entries always carry exactly one `REMANENCE.pad` record (possibly zero spaces) per the 4.6.3 algorithm; zero-payload entries MUST NOT carry one.

### [MINOR] rao/implementability — rao-1.0-specification.md §4.6.2 (l.662) 
ISSUE: Whether `REMANENCE.executable` may appear on non-regular entries is unspecified, and its interaction with the fixed ustar modes of 4.3.1 (hardlinks always `0000644` even if the primary is executable; symlinks `0000777`) and with the manifest `executable` field for hardlink/symlink/directory entries is left to guesswork.
QUOTE: | `REMANENCE.executable` | OPTIONAL | `true` or `false` |
FIX: Scope `REMANENCE.executable` to regular entries (writers MUST NOT emit it elsewhere; manifest `executable` is `null` for non-regular entries), or specify its meaning per entry type.

### [MINOR] rao/implementability — rao-1.0-specification.md §5.2 / 4.2 (l.1082) 
ISSUE: The envelope's uint32 `chunk_size` field silently caps the encrypted representation at chunk_size <= 2^32 - 512 while 4.2 says 'The format defines no maximum'; unlike the analogous 64-byte `object_id` cap, this envelope-only constraint is never stated as a sealing-time rejection rule.
QUOTE: | `0x08` | 4 | `chunk_size` | `uint32` |
FIX: Add a sentence parallel to the `object_id` field rules: a plaintext object whose `chunk_size` exceeds the uint32 range cannot be sealed and writers MUST reject it (`InvalidInput`), and mention the cap in 4.2 or Appendix C.

### [MINOR] rao/implementability — rao-1.0-specification.md §4.5.1 / 15 (l.587) 
ISSUE: RFC 3339 is used normatively for `REMANENCE.write_timestamp` but does not appear in Section 15's references; the reference list generally lacks RFC 7322-style full citations (titles, authors, DOI/URL) for the RFCs and gives no edition for IEEE Std 1003.1.
QUOTE: RFC 3339 timestamp of object creation
FIX: Add [RFC3339] to the normative references and expand all entries to full citation form (including the POSIX edition year).

### [MINOR] rao/implementability — rao-1.0-specification.md §15.2 / 8.2 (l.2449) 
ISSUE: [REMPARITY] is cited by repository-relative path and classified informative, yet Sections 8.2/9 impose interface requirements (bootstrap CBOR key 30, plaintext keys 10-13, encrypted keys 20-21, 'MUST NOT carry manifest anchors') that cannot be implemented for the tape binding without it — a reference required to implement normative behavior is normative.
QUOTE: (`specs/rem-parity-1.0-specification.md`)
FIX: Cite the published REM-PARITY specification by title/identifier (no file path) and move it to the normative references, or scope it as normative-for-the-tape-binding.

### [MINOR] rao/implementability — rao-1.0-specification.md §5.10 (l.1598) 
ISSUE: An internal CLI tool name is used to define the keyless-inspect surface; a standalone specification should define the capability without naming the author's tooling.
QUOTE: the operational `rem archive inspect` surface
FIX: Delete the parenthetical and describe the operation generically ('A keyless inspection operation reveals exactly the header fields...').

### [MINOR] rao/implementability — rao-1.0-specification.md §5.3 (l.1137) 
ISSUE: A reference-implementation CLI flag is embedded in normative key-handling text; this is implementation detail that dates the document and is uninterpretable without the author's tooling.
QUOTE: in the reference implementation, a 32-byte key file named by `--key-file`
FIX: Drop the parenthetical; the normative requirement (callers supply root key material via an in-memory interface, only `key_id` persisted) stands alone.

### [MINOR] rao/implementability — rao-1.0-specification.md §Header table (l.13) 
ISSUE: The front-matter names a repository-relative crate path; 'Remanence' as provenance is fine, but the internal path is meaningless to external readers and will rot.
QUOTE: | Reference implementation | Remanence (`crates/remanence-format`) |
FIX: Reduce the row to 'Reference implementation | Remanence' or cite a published artifact/URL.

### [MINOR] rao/implementability — rao-1.1-specification.md §2.1 (l.68) 
ISSUE: Only hardlinks are addressed; whether symlink and directory entries may carry preserved xattrs (both can have xattrs on POSIX systems — directories routinely do) is unspecified, so writers and readers can diverge on producing/accepting `xattrs` on those entry types.
QUOTE: Hardlink entries MUST emit an empty `metadata_preservation_data`.
FIX: State explicitly which entry types may carry a non-empty `xattrs` map (e.g. regular files and directories; symlinks per policy) and that all others MUST emit the empty container.

### [MINOR] rao/standalone — rao-1.0-specification.md §3.4 (l.358) 
ISSUE: A normative section grounds a SHOULD in the author's specific deployment ('the Remanence deployment', 'the catalog') rather than stating it generically.
QUOTE: in the Remanence deployment the catalog records each copy's representation
FIX: Reword: 'This rule is for self-identification and tooling convenience; deployments that record each copy's representation in a catalog SHOULD cross-check the recorded representation rather than sniff.'

### [MINOR] rao/standalone — rao-1.0-specification.md §4.5.1 (l.580) 
ISSUE: The keyword table describes `REMANENCE.caller_object_id` as 'orchestrator-assigned', presuming the author's orchestrator; 'orchestrator' recurs as an assumed system component (lines 1217, 2035) without ever being defined.
QUOTE: Non-empty opaque UTF-8; orchestrator-assigned identifier
FIX: In the table: 'Non-empty opaque UTF-8; identifier assigned by the calling system (e.g. an archival orchestrator)'. Optionally add 'calling system / orchestrator' to the Section 2.3 definitions so later informal uses have an anchor.

### [MINOR] rao/standalone — rao-1.0-specification.md §5.4.1 / 12.1 (l.1217) 
ISSUE: 'Unique by system invariant' (also Section 12.1, line 2035) appeals to an invariant of the author's system rather than a requirement stated on whoever assigns identifiers.
QUOTE: an orchestrator bug; identifiers are unique by system invariant
FIX: Reword both occurrences to place the requirement on the caller: 'a bug in the assigning system — object identifiers are required to be unique by the system that assigns them (Section 4.5.1)'. Consider adding an explicit sentence to Section 4.5.1: 'The system assigning REMANENCE.object_id values MUST ensure they are unique across all objects sealed under one root key.'

### [MINOR] rao/standalone — rao-1.0-specification.md §15.2 (l.2450) 
ISSUE: The [REMPARITY] reference cites a repository-relative file path instead of a publication citation; additionally, Sections 8.2 and 9 place normative requirements on the bootstrap row ('MUST NOT carry manifest anchors', keys 10–13/20–21/30) while [REMPARITY] is classed informative.
QUOTE: (`specs/rem-parity-1.0-specification.md`): the parity layer of
FIX: Cite by title only: '[REMPARITY] — Rem Tape Parity (REM-PARITY) Format, Version 1.0. Published alongside this specification.' And either move [REMPARITY] to Normative References or add a sentence in Section 8.2 clarifying that the key assignments are restated here informatively and are normative in [REMPARITY].

### [MINOR] rao/standalone — rao-1.1-specification.md §Front matter (l.10) 
ISSUE: The base specification is identified by a repository filename rather than by title/citation, and the same file-style cross-references ('RAO 1.0 §4.7.2') pervade the delta without a formal reference entry.
QUOTE: | Base | `rao-1.0-specification.md` (normative for everything not restated here) |
FIX: Change the row to '| Base | Rem Archive Object (RAO) Format, Version 1.0 [RAO-1.0] (normative for everything not restated here) |' and add a short References section defining [RAO-1.0] as 'published alongside this specification'; the §-style cross-references can then stand.

### [MINOR] rao/structure — rao-1.0-specification.md §5.2 (l.1124) 
ISSUE: Broken internal cross-reference: single-fault conformance vectors are defined in Section 13.5 ("Negative Vectors"), but the text points to Section 13.4, which is the RAO-TV-D1 default-chunk-size positive vector.
QUOTE: one error whose condition holds; conformance test vectors contain exactly one fault each (Section 13.4).
FIX: Change "(Section 13.4)" to "(Section 13.5)".

### [MINOR] rao/structure — rao-1.0-specification.md §front matter (l.13) 
ISSUE: The front-matter table cites a repository-relative crate path for the reference implementation; "Remanence" as the implementation name is fine, but the internal path is meaningless to external readers.
QUOTE: | Reference implementation | Remanence (`crates/remanence-format`) |
FIX: Reduce to "Remanence" or add a public project URL; drop `crates/remanence-format`.

### [MINOR] rao/structure — rao-1.0-specification.md §5.3 / 5.10 / 9 (l.1598) 
ISSUE: Internal tool surfaces leak into the body text: the `--key-file` CLI flag (Section 5.3, line 1137), the `rem archive inspect` command name (Section 5.10, line 1598), and the parity layer's `finish_object()` API name (Sections 5.8 and 9); a stranger's implementation has none of these.
QUOTE: Keyless **inspect** (the operational `rem archive inspect` surface) reveals
FIX: Describe capabilities generically ("a keyless inspect operation", "callers supply 32 bytes of root key material through an in-memory interface", "the parity layer's object-commit operation") and keep implementation-specific names out or in clearly-marked implementation notes.

### [MINOR] rao/structure — rao-1.0-specification.md §Abstract / 2 (Conventions) (l.35) 
ISSUE: Abbreviations are never expanded at first use, contrary to RFC 7322: AEAD appears unexpanded in the Abstract, and AAD, PFR (expanded only implicitly by the Section 6 title), DR, HSM, KMIP, CRC, VTL, and CSPRNG are all used without expansion; the Abstract also relies on code-literal field names (`plaintext_digest`, `stored_digest`, `tar`).
QUOTE: restore: AEAD chunks coincide
FIX: Expand each abbreviation at first use (e.g. "Authenticated Encryption with Associated Data (AEAD)", "Partial File Restore (PFR)") or add them to Section 2.3; render the Abstract as plain self-contained prose without markup literals.

### [MINOR] rao/structure — rao-1.0-specification.md §12.1 (l.2071) 
ISSUE: The security analysis leans on HMAC-SHA-256's dual-PRF assumption and compares to the TLS 1.3 key schedule, but neither HMAC (RFC 2104) nor TLS 1.3 (RFC 8446) is cited anywhere.
QUOTE: on the dual-PRF assumption for HMAC-SHA-256 — that it behaves as a PRF when
FIX: Add [RFC2104] (HMAC) and [RFC8446] (TLS 1.3) to the Informative References and anchor them here; consider also citing a dual-PRF analysis paper (e.g. Backendal–Bellare et al.) for the stated assumption.

### [MINOR] rao/structure — rao-1.0-specification.md §Appendix C (l.2651) 
ISSUE: Appendices A and B are marked "(Informative)" but Appendix C is not marked either way, and it contains an all-caps MUST ("They MUST additionally be re-derived..."), leaving its normative status ambiguous — and as an "open items" list it cannot survive into the published document at all.
QUOTE: ## Appendix C. Open Items Before Freeze
FIX: Mark Appendix C's status explicitly and recast its MUST in plain prose (the binding requirement already lives in Section 14 criterion 2); plan for the appendix and both "Editorial note (remove at freeze)" blocks to be deleted at publication.

### [MINOR] rao/structure — rao-1.0-specification.md §front matter (l.7) 
ISSUE: There is no status-of-this-document statement beyond the two-word table cell "Draft for review": no statement of the publication venue, change control (beyond the freeze rule buried in Section 14), or where to send comments/errata.
QUOTE: | Status | Draft for review |
FIX: Add a short "Status of This Document" paragraph after the front matter stating the document's maturity, its change-control rule (pointing to Section 14), and a feedback/errata contact; update it at publication.

### [NIT] parity/bcp14 — rem-parity-1.0-specification.md §12.1 (l.1195) 
ISSUE: Normative Scanner behavior is stated declaratively without a keyword; it is unclear whether scanning anyway is prohibited or merely unnecessary.
QUOTE: that map is authoritative and no physical scan occurs
FIX: '... the Scanner MUST treat that map as authoritative and need not perform a physical scan' (or 'MAY skip the physical scan').

### [NIT] parity/bcp14 — rem-parity-1.0-specification.md §10.7 (l.1126) 
ISSUE: MUST is attached to a mathematically guaranteed outcome — the parenthetical itself proves the property cannot vary, so no requirement is being levied.
QUOTE: the resulting block count MUST be unchanged (the digest is fixed-size, so re-encoding cannot change the payload length)
FIX: State as fact ('the resulting block count is unchanged: the digest is fixed-size ...') or as an assertion ('a Writer MUST verify the block count is unchanged').

### [NIT] parity/bcp14 — rem-parity-1.0-specification.md §Appendix C (l.1765) 
ISSUE: A capitalized keyword is used as a noun-compound in Appendix C, which — unlike Appendices A and B — carries no informative/normative designation, so the keyword's status there is unclear.
QUOTE: SHOULD-offer
FIX: Rephrase as 'is a SHOULD-strength requirement (Section 8.4 step 5)' and label Appendix C's status (it is editorial and slated for removal at freeze).

### [NIT] parity/bcp14 — rem-parity-1.0-specification.md §17 (l.1497) 
ISSUE: The keyword MUST is used as a countable noun, which BCP 14 does not define and the RFC Editor would flag.
QUOTE: (one vector per MUST)
FIX: Rephrase: '(one vector per Section 9.2 header constraint)'.

### [NIT] parity/consistency — rem-parity-1.0-specification.md §8.2 (l.716) 
ISSUE: Key 2's presence column says 'REQUIRED unless minimal no-parity' while key 1's says 'REQUIRED unless no-parity', but the prose below grants ANY no-parity bootstrap the right to omit both ('readers MUST NOT require the scheme or digest records on it') — the extra word 'minimal' implies a non-minimal no-parity bootstrap must carry the digest record, which nothing else supports.
QUOTE: REQUIRED unless minimal no-parity
FIX: Change key 2's presence to 'REQUIRED unless no-parity', matching key 1 and the prose.

### [NIT] parity/consistency — rem-parity-1.0-specification.md §17 (l.1469) 
ISSUE: The manifest rule says every negative vector records an expected Section 15 error name, but several vectors listed under 'Negative vectors' assert success outcomes instead: 'a corrupt peer counted as an erasure and then recovered around', and the whole damage matrix ('recovered / copy-health downgrade / one-epoch unavailability, never whole-tape failure').
QUOTE: for negative vectors — the expected Section 15 error name
FIX: Reword to 'the expected outcome — the Section 15 error name where the outcome is an error, otherwise the asserted recovery result', or move the recovery-around and damage-matrix vectors under a separate 'behavioral vectors' heading.

### [NIT] parity/implementability — rem-parity-1.0-specification.md §Appendix C (item 5) (l.1774) 
ISSUE: An internal milestone/tracker identifier leaks into the document ('Owner milestone: PAR-KEY30-RECOVERY'); meaningless to external readers even in a pre-freeze open-items appendix.
QUOTE: Owner milestone: PAR-KEY30-RECOVERY
   before format freeze.
FIX: Remove the tracker ID (keep the open item's technical description), or mark all of Appendix C remove-at-freeze like the editorial note at the top.

### [NIT] parity/implementability — rem-parity-1.0-specification.md §12.3 (item 1) (l.1208) 
ISSUE: The bootstrap classification rung says "the payload's `block_size_bytes`", but block_size_bytes is a fixed-frame header field (offset 0x20 in 8.1), not part of the CBOR payload.
QUOTE: payload's `block_size_bytes` equals the read size
FIX: Change to "the frame's `block_size_bytes`" (or "the header's").

### [NIT] parity/implementability — rem-parity-1.0-specification.md §6.2 (l.502) 
ISSUE: 'j in 0..m' and 'i in 0..k' use an undefined (Rust-style half-open) range notation; the stated m × k dimension disambiguates, but an RFC audience may read the endpoint as inclusive.
QUOTE: X_j = k + j   (j in 0..m)        Y_i = i   (i in 0..k)
FIX: Write '0 ≤ j < m' and '0 ≤ i < k', matching the explicit style already used in 3.3 ('0 ≤ data_index < k').

### [NIT] parity/implementability — rem-parity-1.0-specification.md §10.2 (l.1005) 
ISSUE: 'The payload's bytes run contiguously from offset 0xB8 of the copy's first block' is literally impossible at the stated minimum parity_map block size of 0xB8 (offset 0xB8 is past the end of the first block); the intended meaning is byte offset 0xB8 of the copy's contiguous byte stream.
QUOTE: The payload's bytes run contiguously from offset 0xB8 of the copy's first
block
FIX: Reword: 'the payload occupies bytes 0xB8 onward of the copy treated as one contiguous byte string of M blocks (at block sizes ≤ 0xB8 the payload therefore begins in the copy's second block)'.

### [NIT] parity/standalone — rem-parity-1.0-specification.md §18. Conformance and Freeze Criteria, criterion 4 (l.1536) 
ISSUE: "A chaos transport" is the author's internal tool jargon; the surrounding phrase already says what is meant.
QUOTE: write with injected damage (a chaos transport), scan catalog-less, recover
FIX: Replace the parenthetical with "(a fault-injecting transport layer)" or delete it — "write with injected damage" is self-sufficient.

### [NIT] parity/structure — rem-parity-1.0-specification.md §8.4 (l.806) 
ISSUE: Lowercase "must" in an otherwise normative paragraph is ambiguous under the document's BCP 14 convention (lowercase carries no normative force per RFC 8174), leaving unclear whether the hint is a requirement.
QUOTE: Deployments using other block sizes must supply the size as a hint.
FIX: Change to "MUST supply the size as a hint" if normative, or rephrase descriptively ("can only be discovered if the size is supplied as a hint").

### [NIT] rao/bcp14 — rao-1.0-specification.md §4.6.3 (l.705) 
ISSUE: Lowercase 'must' in the normative alignment algorithm ('Because the byte stream must be deterministic, the pad length is uniquely determined') — it is a reference to design goal 6, but in a section otherwise dense with keyworded requirements the unmarked 'must' invites second-guessing.
QUOTE: Because the
byte stream must be deterministic, the pad length is uniquely determined
FIX: Rephrase without the modal: 'Because the byte stream is deterministic (Section 1.1 goal 6), the pad length is uniquely determined:'

### [NIT] rao/consistency — rao-1.0-specification.md §4.5.1 (l.574) 
ISSUE: Stated as a property of 'every object', but §10 designates 'new REMANENCE.* pax keywords (global or per-entry)' as a legitimate 1.x minor-revision surface — a future 1.x object with an added global keyword would violate this sentence as written. (No live contradiction with the 1.1 delta, which only changes the schema_version value, but the tension is latent.)
QUOTE: a global pax header (typeflag `g`) whose payload carries exactly these eight keywords
FIX: Scope the sentence to the writer of this version: 'whose payload carries, for a 1.0 writer, exactly these eight keywords' (readers already ignore unknown keywords per §4.4.3).

### [NIT] rao/consistency — rao-1.0-specification.md §13.2 (l.2262) 
ISSUE: Ambiguous phrasing: the quoted string is 25 bytes, so '26 bytes' only works if the LF is included in the count, but 'X + LF' can be read as 26 bytes plus a 27th. The layout arithmetic and the pinned digest (verified: sha256 of the 26-byte string including LF = 0ea7e9ec…) confirm 26 total, so the value is recoverable — but a vector input should not need disambiguating via its own expected output.
QUOTE: The 26 ASCII bytes `hello, rem archive object` + LF
FIX: Reword to 'the 26 ASCII bytes `hello, rem archive object\n` (25 characters followed by one LF)'.

### [NIT] rao/editorial — rao-1.1-specification.md §3 (l.95) 
ISSUE: "A Finder color tag" is a platform-colloquial example that assumes macOS familiarity.
QUOTE: (e.g. a Finder color
FIX: "(e.g., a short user-namespace attribute such as a macOS Finder tag)" or a platform-neutral example.

### [NIT] rao/editorial — rao-1.1-specification.md §4 (l.107) 
ISSUE: Article inconsistency with the base document: RAO 1.0 consistently writes "An RAO ..."; this document writes "A RAO 1.1 implementation".
QUOTE: A RAO 1.1 implementation implements all of RAO 1.0
FIX: "An RAO 1.1 implementation implements all of RAO 1.0 plus Section 2 of this document."

### [NIT] rao/editorial — rao-1.1-specification.md §2.1 heading (document-wide) (l.45) 
ISSUE: Heading and cross-reference conventions diverge from the base document: headings use sentence case without a trailing period on the number ("2.1 Wire format" vs 1.0's "4.3.1. Header Layout"), and section references use "§" where RAO 1.0 and RFC style write "Section N".
QUOTE: ### 2.1 Wire format
FIX: Align with the 1.0 conventions: "### 2.1. Wire Format" and "RAO 1.0, Section 4.7.1" throughout.

### [NIT] rao/implementability — rao-1.0-specification.md §4.8 (l.934) 
ISSUE: `roundup` and `roundup512` are used throughout (4.6.3, 4.8, 5.7, 4.10) but never formally defined, including the already-a-multiple case.
QUOTE: total_size_bytes      = roundup(offset_after_EOF, chunk_size)
FIX: Define once in Section 2.4: roundup(x, m) = the smallest multiple of m that is >= x (so roundup(x, m) = x when x is already a multiple); roundup512(x) = roundup(x, 512).

### [NIT] rao/implementability — rao-1.1-specification.md §Header table (l.10) 
ISSUE: The base specification is cited by repository filename rather than by the published document's title/identifier.
QUOTE: | Base | `rao-1.0-specification.md` (normative for everything not restated here) |
FIX: Cite as 'Rem Archive Object (RAO) Format, Version 1.0' with its publication identifier once assigned.

### [NIT] rao/standalone — rao-1.0-specification.md §12.1 (l.2075) 
ISSUE: 'The catalog' (definite article) assumes the author's catalog system in a SHOULD-level recommendation.
QUOTE: As a deployment belt, the catalog SHOULD run a consistency check
FIX: Reword: 'As a deployment belt, a deployment that maintains a catalog SHOULD run a consistency check on (key_id, hkdf_salt) at insert.'

### [NIT] rao/standalone — rao-1.0-specification.md §1.3 (l.118) 
ISSUE: Section 1.3 is properly marked informative, but 'the intended deployment' (and 'the intended cloud copy' in Section 8.4, line 1869) still reads as the author's singular deployment rather than an illustrative model.
QUOTE: In the intended deployment each archival master is kept as three copies
FIX: Reword to 'In a representative deployment, each archival master is kept as three copies…' and in Section 8.4 'The encrypted representation is the natural choice for an offsite or cloud copy; storing plaintext copies on shared infrastructure is a deployment policy question, not a format one.'

### [NIT] rao/standalone — rao-1.0-specification.md §12.9 (l.2181) 
ISSUE: 'The production path' is deployment narration about the author's system inside a security-considerations requirement.
QUOTE: streaming Readers are immune by construction and are the production path
FIX: Reword: 'streaming Readers are immune by construction and are RECOMMENDED (Section 4.9).'

### [NIT] rao/standalone — rao-1.0-specification.md §4.7 (l.808) 
ISSUE: The SCSI tape positioning command LOCATE is used without expansion or reference (also Appendix B.13, line 2648, 'direct LOCATE-to-manifest'); a non-tape reader has no anchor for it.
QUOTE: which enables direct LOCATE-to-manifest reading without scanning the archive
FIX: Expand on first use: 'which enables positioning the tape directly to the manifest (e.g. via the SCSI LOCATE command) without scanning the archive.'

### [NIT] rao/structure — rao-1.0-specification.md §2.2 (l.182) 
ISSUE: The Builder and Planner role definitions cross-reference Section 4.8 ("End of Archive"), but the Writer/Planner obligations they describe are specified in Section 4.9 ("Writer, Planner, and Reader Obligations") — the pointers look stale.
QUOTE: canonical plaintext stream; Section 4.8)
FIX: Point the Builder and Planner definitions at Section 4.9 (or "Sections 4.8–4.9").

### [NIT] rao/structure — rao-1.0-specification.md §4.7.1 (l.833) 
ISSUE: Citation-style inconsistency: RFC 8949 is cited via the [RFC8949] anchor elsewhere but appears here as a bare inline "RFC 8949 §4.2.1" (and "RFC 8949 preferred serialization" at line 831), mixing anchored and unanchored forms.
QUOTE: their **deterministic encodings** (RFC 8949 §4.2.1) — for text keys this
FIX: Use "(Section 4.2.1 of [RFC8949])" consistently for both inline mentions.

### [NIT] rao/structure — rao-1.1-specification.md §whole document (style) (l.17) 
ISSUE: Style diverges from the base document and RFC conventions: section references use "§" while RAO 1.0 uses "Section N"; headings drop the trailing period ("### 2.1 Wire format" vs 1.0's "4.3.1."); the BCP 14 boilerplate is inherited by reference in an editorial note instead of being restated; and the informative macOS examples ("Finder color tag", "the resource fork is the only routinely large case") assume platform context never established.
QUOTE: inherited from RAO 1.0 §2.
FIX: Normalize to "Section N" prose and the 1.0 heading style, restate the standard BCP 14 paragraph in a Conventions section, and gloss the macOS examples ("e.g. a macOS Finder color-tag xattr; the macOS resource fork com.apple.ResourceFork").

