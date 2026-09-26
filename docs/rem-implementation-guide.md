# REM Implementation and Operations Guide

This revision: 26 September 2026. The history at the end records what each
revision changed.

## 1. About this guide

The REM specifications (REM-OBJECT, REM-ENCRYPT and REM-PARITY) define
formats. Each says how its bytes are laid out, what they mean, and what may be
concluded from them. Each also says, near its start, what it leaves out: how a
tool should hold keys, stage and publish what it writes, protect the computer
it restores onto, organise its work after a crash, and report progress to its
operator. Those matters decide whether a tool is safe and dependable to run.
They do not decide whether its bytes can be read, so they are not conformance
requirements, and this guide collects what we recommend about them instead.

This guide is informative. It is not a specification, and it has no DOI.
Conformance to REM-OBJECT, REM-ENCRYPT and REM-PARITY is defined by those
documents alone, and a tool can conform without following anything written
here. The guide is revised independently of the specifications; each revision
is dated, and the history at the end records what changed.

It is written for anyone who builds or operates a tool that reads or writes
these formats. The reference implementation, Remanence, appears in examples,
because it is the implementation we know best, but the recommendations do not
depend on it. Where a recommendation describes what Remanence does, it says so.

This first revision holds the practice that REM-OBJECT 1.0.0-draft.4 moved out
of that specification. The practice that REM-ENCRYPT and REM-PARITY move out
will be added in later revisions. Chapters are numbered in the order they will
finally take, so the numbering has gaps until then: chapters 8 to 11 do not
exist yet, and several chapters here will gain sections.

### How to read the recommendations

Each recommendation starts with the risk it addresses, then gives the practice
we recommend, and ends by naming the specification section it serves, by
number and title, for example "Serves REM-OBJECT §12.10, Path Traversal." The
section is named by its title as well as its number so that the reference
still makes sense if a later revision renumbers the section.

The guide's own advice is written with "should" and "we recommend", in lower
case. It does not use the capitalised requirement keywords of the
specifications, which are reserved for conformance requirements. Where a
recommendation rests on a requirement of a specification, the guide quotes
that requirement and cites it rather than restating it in its own words, so
that nothing here can be mistaken for a new or changed requirement. A quoted
requirement comes from REM-OBJECT 1.0.0-draft.4, the revision in preparation,
or, where the citation says so, from REM-ENCRYPT 1.0.0-draft.4.

Each chapter ends with a short summary in plain terms.

### How chapters are named

The specifications point to this guide by chapter title. A chapter title,
once published, is therefore not renamed or removed without a note under the
old title saying where its material went.

## 2. Handling hostile media

A cartridge, a disk file or an object in a store can hold bytes that no
conformant writer produced. Damage produces some of them; a person who wants
to harm the machine that reads them produces others. The specifications
define which inputs are valid and require a reader to reject the rest. They do
not say how a reader should survive the attempt, because a reader that
crashes on a hostile object still never misreads a valid one. This chapter
collects the practice that keeps a reader running.

### 2.1. Treat every stored byte as untrusted

Stored bytes come off removable media and networks, in both representations.
An encrypted copy's header and key frame are parsed before anything has been
authenticated, and a plaintext copy is never authenticated at all (REM-OBJECT
§12.6). Even the values a catalog or bootstrap supplies, `chunk_size` and the
block count, are only as trustworthy as that catalog.

We recommend that a reader treat every stored byte as untrusted input, and
treat `chunk_size` and block count as semi-trusted: they decide how the bytes
are read, and a wrong value must lead to a rejection rather than to a wrong
reading. The acceptance checks that REM-OBJECT §12.9 lists, each required by
the section it cites, are the minimum. The rest of this chapter is about
performing them safely.

Serves REM-OBJECT §12.9, Hostile-Input Posture.

### 2.2. Check a size before it drives an allocation

A length read from the medium can claim anything up to 2^64 − 1. A reader
that allocates a buffer of the declared size, or reserves room for a declared
number of entries, before it has checked the value can be made to request
more memory than the host has. The allocation fails or the process is killed,
and the read ends without a result.

We recommend checking every size read from the medium against the bytes that
remain before it drives an allocation. A payload size should be checked
against the remaining declared blocks before any buffer is sized from it. A
manifest decoder should size its buffers from the manifest's declared length,
and never from counts read out of the CBOR stream. It should enforce the entry
and depth limits as it reads, rather than after it has built the whole
structure. The limits themselves are a requirement of the specification:
"Decoders MUST enforce both limits." (REM-OBJECT §4.7.1). Enforcing them
incrementally changes no result. It only keeps the decoder's memory bounded
while it reaches that result.

Serves REM-OBJECT §4.7.1, Deterministic CBOR, and REM-OBJECT §12.9,
Hostile-Input Posture.

### 2.3. Read in a stream; if you must hold an object whole, size the allocation so that a hostile declaration fails as an error

An object can be hundreds of gigabytes long. A reader that loads a whole
object before parsing it needs memory in proportion to the object, and a
hostile object can declare a size chosen to exhaust it.

We recommend reading in a stream. A streaming reader needs memory in
proportion to `chunk_size` plus one pax header, and allocates a constant
amount per chunk, whatever the object's size. A reader that materializes the
whole object, for example to serve an interface that needs it whole, should
reserve its up-front allocation fallibly, so that an oversized declaration
produces an error rather than an abort. It should also enforce a size ceiling
set for the deployment. Both approaches accept and reject exactly the same
objects; the difference lies only in the resources they use. Remanence, for
example, restores through its streaming reader.

Serves REM-OBJECT §4.9, Writer, Planner, and Reader Obligations, and
REM-OBJECT §12.9, Hostile-Input Posture.

### 2.4. Never panic on any byte sequence

A reader that panics, crashes or invokes undefined behaviour on a malformed
object produces no result. In an unattended restore, or a scrub over
thousands of objects, one such object stops the whole run. In a language
without memory safety, undefined behaviour on attacker-chosen bytes can become
code execution on the reading machine.

A reader should never panic, crash or invoke undefined behaviour on any byte
sequence. We recommend enforcing this mechanically rather than by review. On
every path that input bytes can reach, avoid operations that abort on bad data:
in Rust, `unwrap`, unchecked indexing and unchecked arithmetic, and forbid
`unsafe` where it can be avoided; the same discipline applies in other
languages under other names. Arithmetic is the part the specification itself
fixes, because a wrapped offset would misread the object: "implementations
MUST use checked arithmetic and MUST NOT wrap silently" (REM-OBJECT §2.4).

Serves REM-OBJECT §12.9, Hostile-Input Posture.

### 2.5. Test the parser with coverage-guided fuzzing

Parsers fail on inputs that nobody thought to write down, and a test suite
written by hand covers only the inputs its authors imagined.

We recommend validating the property of section 2.4 with coverage-guided
fuzzing. For REM-OBJECT, fuzz at least three targets: the pax record loop, the
manifest CBOR decoder, and whole-object open and verify for plaintext inputs.
Each should run long enough for its coverage to stop growing. Remanence, for
example, keeps these targets in the `fuzz/` directory of its repository.

Serves REM-OBJECT §12.9, Hostile-Input Posture.

### In plain terms

A reader has two jobs: read valid objects correctly, and stay standing when it
is handed something that is not one. The specification takes care of the
first. This chapter is about the second: check every number before you act on
it, never hold more than you have checked, and test the parser with inputs
designed to break it.

## 3. Restoring onto a host

A restore writes files onto a computer that the object knows nothing about.
The specification says what an object contains and what a restore may claim
to have reproduced. It does not govern the destination, because the
destination lies outside the object; REM-OBJECT §1.4 already places restore
policy and restore-time path sanitisation with the tools above the format.
This chapter recommends how a restoring tool should protect the host.

The conformance vectors record what a restore that follows the defaults
recommended in sections 3.3 to 3.6 reports, in their `expected.default_restore`
fields (`skipped_xattrs`, `applied_privileged_xattrs`, `carried_extensions`
and `reported_values`). These fields are informative for conformance: "A conformant Restoring Consumer need not
reproduce them." (REM-OBJECT §13.1).

### 3.1. Keep your own sanitisation, and never follow a symlink in the destination

Entry paths in a conformant object are clean relative paths, but symlink
targets are opaque strings that may be absolute or contain `..`. The classic
archive attack uses that difference. An earlier symlink entry creates
`dir -> /outside`, and a later regular entry writes through `dir/file`, so the
file lands outside the restore root. The same thing happens when a symlink
that was already present in the destination directory is followed.

A restoring tool should keep its own sanitisation rather than rely on the
format's path rules alone. While it materializes any entry, it should never
follow a symlink that is already present in the destination tree, including
one that an earlier entry of the same object created. We recommend opening
each path component with `openat` and `O_NOFOLLOW`, or an equivalent
component-by-component discipline that re-checks every component. The
specification requires the fidelity half of this work: "A Restoring Consumer
MUST create symlink entries as symlinks, without dereferencing their
targets." and "A Restoring Consumer MUST materialize a hardlink's primary
before creating the hardlink (`link(2)`) to the already-restored primary."
(REM-OBJECT §12.10).

Serves REM-OBJECT §12.10, Path Traversal.

### 3.2. Map every path onto the native filesystem before writing anything

The entry-path grammar is checked against POSIX semantics only. On Windows, a
component such as `..\outside` contains a separator that the grammar never
inspected, and a value like `C:\x` or `\\host\share\x` is a drive-relative or
UNC absolute path. Case folding and Unicode normalisation can merge two
distinct entries onto one native file.

Before it materializes any entry, a tool restoring onto a non-POSIX or
case-folding filesystem should resolve every entry's native destination,
applying the target's separator, case-folding and Unicode-normalisation rules.
It should reject or report, and never silently overwrite, any entry whose
destination escapes the restore root or resolves to an absolute,
drive-relative or UNC path. This preflight is in addition to the symlink
discipline of section 3.1, not a replacement for it. The collision case is a
requirement of the specification: "A Restoring Consumer that maps entry paths
onto a native filesystem MUST reject or report, never silently overwrite, any
entry whose native destination collides with a destination that another entry
in the same object has already produced." (REM-OBJECT §12.10). Running the
preflight before anything is written is the practical way to meet it.

Serves REM-OBJECT §12.10, Path Traversal.

### 3.3. Apply only `user.` attributes by default

Extended attributes can carry privilege. A restored `security.capability`
attribute turns an ordinary binary into a privileged one, and `security.*`,
`trusted.*` and POSIX ACL attributes change access control. An object from an
untrusted source can carry any of them.

By default a restore should apply only the portable core, the `user.`
namespace, and should leave the extension tier unapplied. That tier is every
attribute that is not in the `user.` namespace, including one whose name has
no namespace, and every extension. A tool should apply a
further namespace prefix only when explicit operator policy names it.
Attributes outside the effective allow-list should be skipped and reported by
name, and never applied. No registered disposition and no external list
should cause an extension-tier item to be applied by default. Remanence, for
example, restores only `user.` attributes unless the operator names further
prefixes with `--xattr-namespace`, and its restore report lists the
attributes it skipped and the privileged attributes it applied.

Serves REM-OBJECT §4.7.3, Extended-Attribute Preservation, and REM-OBJECT
§12.10, Path Traversal.

### 3.4. Hand preserved attributes to the caller, and treat a skip as an outcome

A reader that preserves attributes but keeps them to itself leaves its caller
unable to apply them, or to say that they were not applied.

A reader that implements attribute preservation should hand the preserved
attributes to its caller. When a restore then reapplies them, it should
follow sections 3.3 and 3.5. A skip made under policy is an outcome of that
policy, not an error, and it is reported as such. A genuine failure to apply
an attribute is different, and the specification requires it to be visible:
"A Restoring Consumer MUST surface any attribute application failure rather
than silently declaring success." (REM-OBJECT §4.7.3).

Serves REM-OBJECT §4.7.3, Extended-Attribute Preservation.

### 3.5. Write attributes without following links

An attribute written through an interface that follows symbolic links lands
on the link's target, and the target is chosen by whoever wrote the object.

A restoring tool should never write an attribute through an interface that
follows a symbolic link at the final path component. For an entry that is
itself a symbolic link, it should use a link-targeting interface, such as
`lsetxattr` on Linux, or skip the attribute and report it.

Serves REM-OBJECT §12.10, Path Traversal.

### 3.6. Apply extensions only when operator policy names them

An `ext` member can carry platform metadata, such as a record from another
operating system's file model, whose application changes the host in the same
ways that a privileged attribute does.

A restoring tool should apply no extension to system state unless explicit
operator policy names it. Every extension, recognised or not, should be
carried, and when it is reported it should be reported by name only. An
extension the tool does not recognise should always remain carry-only:
carried, and not acted on. A short name recorded in the community list that
REM-OBJECT §15 describes confers no default application on restore.

Serves REM-OBJECT §4.7.5, Extension Containers; REM-OBJECT §12.10, Path
Traversal; and REM-OBJECT §15, IANA Considerations.

### In plain terms

The format guarantees that an object's contents match its manifest. The
destination computer is your responsibility: keep every write inside the
restore root, do not follow symlinks that are already there, apply privileged
metadata only when the operator has named it, and report what you did not
apply.

## 4. Staging, commit and durability

A writer produces bytes that another tool may read decades later. The
specification fixes what those bytes must be. This chapter is about the order
of work that gets them onto the medium intact, and about when it is safe to
say that a copy exists.

### 4.1. Build an object in a fixed order, and verify as you go

A writer that discovers a problem late, after tape has moved or a file has
been published, leaves a partial object behind or reports one that is wrong.

We recommend building each object in this order. First validate the options
(`chunk_size`, `object_id`, `caller_object_id`, `write_timestamp` and the
manifest entry's `file_id`, `manifest_file_id`) against the constraints of REM-OBJECT §4.2 and §4.5.1,
and every member specification against REM-OBJECT §4.6.6. Then plan the complete layout from the member specifications
alone, which serialises the manifest and computes `manifest_sha256` before any
payload byte is read. Then emit the global header, each member entry and the
manifest entry, passing every payload byte through a running SHA-256 check.
Emit tar EOF and the final zero fill, and confirm that the number of blocks
written equals the plan. Only then report the layout, for the catalog
(section 5.1). The specification requires the plan and the output to agree,
and requires honesty about failure: "Planner and Writer MUST share the same
sizing rules such that the planned layout is byte-exact." and "A failed object
MUST NOT be reported as complete." (REM-OBJECT §4.9).

Every digest in the chain can be computed over bytes that are already flowing
through the writer. We recommend computing `plaintext_digest` over the
emitted stream as it is written; for a plaintext copy the same value is its
`stored_digest`. The digest then costs arithmetic and no extra read.

Serves REM-OBJECT §4.9, Writer, Planner, and Reader Obligations, and
REM-OBJECT §7.2, Write-Path Verification (No Extra Reads).

### 4.2. Write through a sink that reports every block

A tape drive can accept fewer bytes than a full block, or reach the end of the
medium part-way through an object. A writer that does not see this at once
goes on writing an object that can never be read back whole.

We recommend writing through a block sink that reports the outcome of every
block write, and treating a short write or a hard end of medium as the end of
that object. How the writer then recovers, for example by starting the object
again on another tape, is its own choice. The failure itself is a requirement
of the specification: "A Writer MUST fail the object when a block write
commits fewer bytes than the full block or reports hard end-of-medium."
(REM-OBJECT §4.9).

Serves REM-OBJECT §4.9, Writer, Planner, and Reader Obligations.

### 4.3. Re-read each copy before recording it durable

The checks in section 4.1 prove that the writer archived what it was given.
They do not prove that the medium or the network kept it. A copy can be
damaged in transmission, or by a drive that reports success on a bad write.

After each copy is written, and before it is recorded as durable, we recommend
re-reading it through the object read path and re-verifying it. A full
verification checks every regular member's `file_sha256` together with the
correspondence and hardlink checks of the Verifier profile (REM-OBJECT §7.4).
At a minimum, the copy's `stored_digest` should be compared. This is the one
deliberate extra read in the pipeline, and a conformant Verifier is the
natural tool for it. REM-ENCRYPT §7.3 describes the corresponding check for
an encrypted copy.

Serves REM-OBJECT §7.3, Post-Write Re-Verification (Deployment Obligation).

### 4.4. Stage and publish a file-bound copy durably

A file renamed into place without prior synchronisation can, after a crash,
leave its final name pointing at data that was never fully written. A catalog
that already refers to that name then claims a copy that does not exist.

We recommend writing a file-bound copy to an exclusively created temporary
path, for example `name.rem-object.partial`. Flush and fsync the file, rename
it to its final name, and fsync the containing directory before reporting
success. Partial outputs should be deleted or quarantined, and never referred
to by a durable catalog.

Serves REM-OBJECT §8.3, File Binding.

### In plain terms

Write in an order that finds problems before they cost anything, watch every
block go down, read the copy back before believing in it, and never let a
half-written file wear the name of a finished one.

## 5. Catalogs and indexes

The format defines no catalog, and every copy can be read without one. A
catalog is nevertheless where most archives keep the facts that make copies
findable, scrubbable and authenticated. This chapter recommends what to keep
there and how to keep it.

### 5.1. Record, for each copy, what a reader and a scrubber will need

A reader needs `chunk_size` and the block count from outside the object; the
specification requires that "A Reader is given the object's `chunk_size` and
block count out of band (catalog, bootstrap, or filemark map) and MUST process
exactly that many blocks" (REM-OBJECT §4.2). A scrubber needs `stored_digest`,
which is never stored inside the copy.

We recommend recording, for each copy, its location, its representation, its
`stored_digest`, its `stored_size_bytes` or block count, and its `chunk_size`.
Record as well the layout the writer reports: `projected_size_blocks`, each
file's `first_chunk_lba`, the manifest's geometry, `manifest_sha256` and
`plaintext_digest`. The representation can be detected from the bytes, but
detection is a convenience. A reader should cross-check detection against the
recorded representation rather than rely on detection alone.

Serves REM-OBJECT §8.1, The Byte-Format Contract; REM-OBJECT §3.4,
Representation Detection; REM-OBJECT §4.9, Writer, Planner, and Reader
Obligations; and REM-OBJECT §7.2, Write-Path Verification (No Extra Reads).

### 5.2. Join copies by their shared identity

A plaintext copy and an encrypted copy of one object have different stored
bytes, and so different `stored_digest` values, but they wrap the same
canonical object.

An index should join the copies of one logical object by `plaintext_digest`.
For a plaintext copy, `stored_digest` and `plaintext_digest` are the same
value, so a catalog can store it once.

Serves REM-OBJECT §3.3, Identities and Digests.

### 5.3. Store per-file rows once per object, with plaintext offsets

The per-file index, `first_chunk_lba` and `size_bytes` for each file, is the
same for both representations, because both wrap the same canonical bytes.
Offsets into stored bytes differ between representations, and can be
recomputed from the per-file index whenever they are needed.

We recommend storing per-file rows once per object, not once per copy, and
keeping plaintext offsets (inner `BodyLba` and file byte ranges) as the
source of truth. A catalog should not make representation-specific stored
offsets canonical. REM-OBJECT §6.2 and REM-ENCRYPT §6.3 reproduce them.

Serves REM-OBJECT §6, Partial File Restore.

### 5.4. Keep hardlinks resolvable in a per-file index

A hardlink entry holds none of its own content. Its size is 0 and its
`first_chunk_lba` is `null`. A partial restore of a hardlinked name uses its
primary's coordinates, as the specification requires: "PFR on a hardlinked
name MUST first resolve its `link_target` to the primary entry and then use
the **primary's** `first_chunk_lba` and `size_bytes` for all arithmetic
below." (REM-OBJECT §6).

A per-file index that serves restores from catalog rows, rather than from the
full manifest, should keep every hardlink resolvable. The hardlink's row
should carry its `entry_type` and `link_target`, to be resolved at restore
time, or a copy of the primary's `first_chunk_lba` and `size_bytes`. A row
that stores only the hardlink's own `null` and `0` cannot restore that name.

Serves REM-OBJECT §6, Partial File Restore.

### 5.5. Protect the catalog

The catalog is a separate trust domain. It holds the external anchors that
authenticate plaintext copies, and it may hold cleartext paths and per-file
rows even when every stored copy is encrypted.

We recommend protecting the catalog's confidentiality, integrity and
provenance at least as carefully as the copies themselves.

Serves REM-OBJECT §12.6, Plaintext Copies Are Not Self-Authenticating.

### In plain terms

The objects can always be read without the catalog, but the catalog is what
lets an archive find, check and trust them quickly. Record enough to read and
scrub every copy, store each fact once, keep hardlinks resolvable, and guard
the catalog as carefully as the archive it describes.

## 6. Keys and secrets

This revision of the chapter holds one recommendation, from REM-OBJECT. The
key-custody practice that REM-ENCRYPT moves out of its specification will be
added here in a later revision.

### 6.1. Report attributes by name, never by value

Attribute values and extension members can hold secrets: credentials kept in
`user.` attributes, security labels, or the private metadata of another
operating system. Logs are copied, shipped and retained far more widely than
the data they describe.

A restoring tool that reports skipped or applied attributes and extensions
should report their names only, and should never log their values.

Serves REM-OBJECT §12.10, Path Traversal.

### In plain terms

Say which attributes you skipped or applied; never say what was in them.

## 7. Verification, scrub and repair

The specification defines what a Verifier checks and what a claim of validity
rests on. This chapter covers the checks that other tools can usefully make,
how to report what verification finds, and how to scrub and repair stored
copies.

### 7.1. Verify the manifest before decoding it

An unverified manifest is untrusted input from removable media. A decoder
that parses it before verifying it exposes itself to hostile bytes, and a
tool that acts on a field before verification may act on a forged value.

We recommend verifying the manifest bytes against their anchor digest before
decoding any field. The specification fixes what a tool may rely on: "A
Consumer MUST NOT rely on a manifest field unless it has verified the manifest
bytes against an anchor digest." (REM-OBJECT §4.7.2).

Serves REM-OBJECT §4.7.2, Schema.

### 7.2. Make the cheap cross-checks

Some inconsistencies can be found by any reader at almost no cost, and
finding them early points to a writer defect or to damage before it causes
anything worse.

A reader should cross-check each entry's `REMANENCE.chunk_count` against the
value recomputed from the entry's effective size, and should surface a
mismatch as an inconsistency. When both the manifest and the archive entries
are at hand, a consumer that is not acting as a Verifier should still check
that they correspond: the same paths, entry types, link targets, sizes,
hashes where present and chunk geometry, with nothing extra on either side.

Serves REM-OBJECT §4.6.2, Per-Entry Keywords, and REM-OBJECT §4.7.2, Schema.

### 7.3. Report every nonconformity

A verifier that stops at the first fault gives an operator one problem to fix
at a time, and hides how damaged a copy is.

A verifier should report every nonconformity it finds, in every
representation, rather than only the first.

Serves REM-OBJECT §7.4, Verifier Profile.

### 7.4. Make restore the default, and salvage a deliberate choice

A damaged object can still hold most of its files intact. A salvage mode that
delivers what it can, while reporting what failed verification, is valuable.
Used by accident, the same mode delivers unverified bytes to someone who
believes they were verified.

We recommend making the integrity-verifying restore mode the default, and
offering salvage only as a mode the operator selects deliberately and that is
clearly labelled in every report. The specification forbids the silent case:
"An implementation MUST NOT silently fall back to salvage." (REM-OBJECT §4.9).

Serves REM-OBJECT §4.9, Writer, Planner, and Reader Obligations.

### 7.5. Expose typed errors

A caller that receives only a message string cannot tell a damaged object
from a failing drive, and cannot act on either.

We recommend that a tool expose typed errors equivalent to the taxonomy of
REM-OBJECT §11. The specification makes one distinction a requirement: "I/O
failures MUST remain distinguishable from format violations so callers can
tell storage problems from invalid objects." (REM-OBJECT §11).

Serves REM-OBJECT §11, Errors.

### 7.6. Scrub by stored digest, without keys

A copy that nobody reads can decay for years before anyone notices. The
specification makes every copy checkable without keys: "Any stored copy MUST
be scrubbable by `stored_digest` alone" (REM-OBJECT §3.3).

We recommend scrubbing stored copies by `stored_digest` on a regular schedule,
without keys and without interpreting the bytes. On tape, the parity layer
also protects every stored block with a CRC, and can verify and repair at
block granularity without reading the whole object. Both checks work on stored
bytes and do not depend on the representation.

Serves REM-OBJECT §7.5, Scrub.

### 7.7. Repair stored blocks first, then open the encrypted copy again

Parity repair of an encrypted object works on ciphertext and needs no keys.
The envelope fails closed: a copy with damaged blocks does not open.

We recommend repairing the damaged stored blocks from parity first, and then
retrying decryption on the recovered stored bytes. A repair that did not
succeed produces no plaintext, because REM-ENCRYPT requires that "A failed
metadata or chunk tag MUST stop processing without releasing that chunk's
plaintext." (REM-ENCRYPT §12.4).

Serves REM-OBJECT §9, Relationship to the Parity Layer.

### In plain terms

Check the index before trusting it, look for the inexpensive signs of
trouble, report everything you find, never pass off a rescue as a clean
restore, and check every copy on a schedule. When a copy is damaged, mend its
bytes first and open it afterwards.

## 12. Ingest

Ingest is where files become objects. The decisions made here, about block
size, metadata and packing, are fixed in the bytes and cannot be revisited
without rewriting the object.

### 12.1. Choose a chunk size the drives can handle

The format sets no upper bound on `chunk_size`, and on tape the body block is
the tape block. A drive that cannot write or read blocks of that size cannot
handle the object.

We recommend choosing `chunk_size` within the block-size limits of every
drive that will write or read the tape, now and in the foreseeable future.

Serves REM-OBJECT §4.2, Body Blocks and `chunk_size`.

### 12.2. Report attributes that could not be captured

Some native attributes cannot be represented in the canonical wire form, for
example a name with no namespace, or one that a case-folding store cannot
round-trip without altering its case. The format does not capture them.

An ingesting tool should report every attribute it could not capture, so that
the omission is a recorded decision rather than a silent loss.

Serves REM-OBJECT §4.7.3, Extended-Attribute Preservation.

### 12.3. Carry unrecognised extensions forward when re-capturing

A tree restored from an object and then captured again produces a new object.
If the writer drops the extensions it does not understand, the new object
silently loses metadata that the old one carried.

A writer that re-captures an object from a previously restored tree should
carry forward, unchanged, every `ext` member of the source object's manifest
that it does not recognise.

Serves REM-OBJECT §4.7.5, Extension Containers, and REM-OBJECT §4.9, Writer,
Planner, and Reader Obligations.

### 12.4. Decide deliberately when to wrap small files

Every non-empty entry begins on a chunk boundary, so a tree of very small
files spends most of its space on alignment. REM-OBJECT Appendix E describes
a wrapper convention that packs a subtree into one member, at the cost of the
inner files' individual identity.

We recommend wrapping a subtree only for a stated reason: because an ingest
rule names it, or because a file in it cannot be represented as a native
entry. In the second case, that file, or the directory holding it, should be
wrapped on its own. We recommend not wrapping by size alone. A scan may
suggest candidates, but the suggestion should change nothing by itself.
Remanence, for example, follows this rule. Its scan mode suggests a directory
when at least ninety per cent of at least one hundred files under it cannot
be represented natively.

Serves REM-OBJECT Appendix E, Packing Many Small Files (Informative).

### 12.5. Upload to an object store with integrity

A copy can be damaged in transit to a store, and a store that accepted it may
still hold something different.

We recommend recording `stored_digest` as the object's integrity metadata,
uploading with whatever integrity the store offers (for example checksum
headers), and verifying the stored copy by digest after the upload. The
encrypted representation is the one intended for copies on shared
infrastructure. Whether to store plaintext copies there is a decision about
the deployment, not about the format.

Serves REM-OBJECT §8.4, Object-Store Binding.

### 12.6. Review a plaintext object before publishing it

A plaintext object discloses, to anyone who can read it, every path, the
directory tree, sizes, times and every captured attribute value.

We recommend reviewing a plaintext object before publishing it. The inventory
that a Verifier has validated is a first-pass screen, because it names every
non-`user.` namespace and extension present, but it does not bound what the
values themselves disclose.

Serves REM-OBJECT §12.12, Disclosure in Published Plaintext Objects.

### In plain terms

Decide the lasting things at ingest on purpose: a block size the drives can
handle, a record of what could not be kept, no silent loss on re-capture,
packing only for a reason, and a checked upload. Look at what a plaintext
object reveals before anyone else can.

## 13. Descriptive fields

Some fields and entries describe an object without changing how any reader
interprets it. Whether to record them is a writer's choice. This chapter says
which we recommend, and why.

### 13.1. Emit directory entries only when they carry information

A directory that contains files is already implied by those files' paths. An
empty directory has no files to imply it, and is lost unless it has an entry
of its own. A directory entry can also carry metadata of its own, such as a
modification time, preserved extended attributes or an extension, and that
metadata is lost if the entry is left out.

A writer should give a directory its own entry when the directory is empty or
when the entry would carry metadata of its own. A writer can leave out the
entry for a directory whose existence its child paths already imply and whose
entry would carry nothing else. Leaving such an entry out loses nothing that
the object would otherwise record.

Serves REM-OBJECT §4.6.1, Entry Frame.

### In plain terms

Give a directory its own entry when it is empty or has metadata of its own;
leave out entries that would only repeat what the file paths already say.

## 14. Revision history

- **26 September 2026.** First revision. Chapters 1 to 7, 12 and 13 hold the
  practice that REM-OBJECT 1.0.0-draft.4 moved out of that specification:
  handling hostile media, restoring onto a host, staging and durability,
  catalogs and indexes, keeping attribute values out of logs, verification,
  scrub and repair, ingest, and which directory entries a writer emits.
