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

This revision holds the practice that REM-OBJECT 1.0.0-draft.4,
REM-ENCRYPT 1.0.0-draft.4 and REM-PARITY 1.0.0-draft.5 moved out of those
specifications.

### How to read the recommendations

Each recommendation starts with the risk it addresses, then gives the practice
we recommend, and ends by naming the specification section it serves, by
number and title, for example "Serves REM-OBJECT §12.10, Path Traversal." The
section is named by its title as well as its number so that the reference
still makes sense if a later revision renumbers the section. A "Serves" line
names the specification section the practice supports. Where the rule itself
moved to this guide, that section may hold only the description of the hazard,
not a rule.

The guide's own advice is written with "should" and "we recommend", in lower
case. It does not use the capitalised requirement keywords of the
specifications, which are reserved for conformance requirements. Where a
recommendation rests on a requirement of a specification, the guide quotes
that requirement and cites it rather than restating it in its own words, so
that nothing here can be mistaken for a new or changed requirement. A quoted
requirement comes from the revision in preparation of the specification its
citation names: REM-OBJECT 1.0.0-draft.4, REM-ENCRYPT 1.0.0-draft.4 or
REM-PARITY 1.0.0-draft.5.

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

We recommend that a reader treat every stored byte as untrusted input.
`chunk_size` and the block count decide how the bytes are read, so a wrong
value must lead to a rejection rather than to a wrong reading. The acceptance checks that REM-OBJECT §12.9 lists, each required by
the section it cites, are the minimum. The rest of this chapter is about
performing them safely.

Serves REM-OBJECT §12.9, Hostile-Input Posture, and REM-ENCRYPT §12.9,
Envelope Hostile-Input Discharge.

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

An envelope parser meets the same risk in the key frame and the metadata
frame. It should check the key-frame length and slot count before it
allocates for the frame, and check the envelope's geometry before it seeks or
allocates. It should enforce the metadata CBOR depth and item limits as it
decodes. The bounds themselves are requirements, listed in REM-ENCRYPT §12.9;
checking them early changes no result.

A tape reader meets the same risk in the bootstrap's CBOR payload, in the
sidecar index and in the terminal replicas and separation extents. Its CBOR
decoder should size its buffers from the measured byte length of the input,
never from counts read out of the stream. Every declared count or length, and
every size formula of a replica or separation frame, should be checked against
the measured physical extent before it drives an allocation or a seek. The
check itself is a requirement: "Every declared count or length is validated
against the measured physical extent." (REM-PARITY §16.2). Making it before the
allocation, rather than after, changes no result.

Serves REM-OBJECT §4.7.1, Deterministic CBOR; REM-OBJECT §12.9, Hostile-Input
Posture; REM-ENCRYPT §12.9, Envelope Hostile-Input Discharge; REM-PARITY
§10.6, Replica Validity Conditions; and REM-PARITY §16.2, Hostile-Input
Posture.

### 2.3. Read in a stream

An object can be hundreds of gigabytes long. A reader that loads a whole
object before parsing it needs memory in proportion to the object, and a
hostile object can declare a size chosen to exhaust it. Some interfaces still
need an object whole, and a reader that serves them has to hold one in memory.

We recommend reading in a stream. A streaming reader needs memory in
proportion to `chunk_size` plus one pax header, and allocates a constant
amount per chunk, whatever the object's size. A streaming reader of an
encrypted copy likewise needs a constant amount of memory per payload chunk.
Remanence, for example, restores through its streaming reader.

A reader that materializes the whole object should reserve its up-front
allocation fallibly, so that an oversized declaration produces an error rather
than an abort. It should also enforce a size ceiling set for the deployment.
That ceiling is deployment policy, not a check of format validity: it can
refuse an object that is valid and that a streaming reader would read. The
choice of approach does not change which objects are valid, and a refusal for
size should be reported as a resource limit, not as a format violation.

A terminal replica carries a 256-byte row for every Object on the tape, and a
tape can hold a million Objects. We recommend validating the structural and
ordinal invariants of a replica's payload as its rows stream, so that reading
or writing the inventory needs no allocation proportional to the whole tape.
The invariants are the same either way.

Serves REM-OBJECT §4.9, Writer, Planner, and Reader Obligations; REM-OBJECT
§12.9, Hostile-Input Posture; REM-ENCRYPT §12.9, Envelope Hostile-Input
Discharge; and REM-PARITY §10.2, Structural Rows.

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
A tape reader is under the same discipline on every path that tape bytes
reach, and REM-PARITY fixes its arithmetic in the same way: "Arithmetic on
values read from tape MUST be checked; overflow is rejection, never
wraparound" (REM-PARITY §2.4).

Serves REM-OBJECT §12.9, Hostile-Input Posture; REM-PARITY §2.4, Integer,
Byte, and Text Conventions; and REM-PARITY §16.2, Hostile-Input Posture.

### 2.5. Test the parser with coverage-guided fuzzing

Parsers fail on inputs that nobody thought to write down, and a test suite
written by hand covers only the inputs its authors imagined.

We recommend validating the property of section 2.4 with coverage-guided
fuzzing. For REM-OBJECT, fuzz at least three targets: the pax record loop, the
manifest CBOR decoder, and whole-object open and verify for plaintext inputs.
For REM-ENCRYPT, fuzz at least four targets, separately from the plaintext
ones: the scalar-header parser, the key-frame parser, the metadata CBOR
decoder, and whole-object open and verify for encrypted inputs. For
REM-PARITY, fuzz the bootstrap, sidecar, ParityMap, terminal-replica and
separation parsers, and the scan walk. Each should run long enough for its
coverage to stop growing. Remanence, for example, keeps these targets in the
`fuzz/` directory of its repository. Its REM-PARITY targets cover the
bootstrap, the ParityMap, the sidecar and the scan walk, but none yet covers
the terminal-replica or separation parser. The project requires those before
it freezes REM-PARITY: they are REM-PARITY freeze criterion 3, in the
[release record](../specs/README.md#how-a-revision-is-frozen).

Serves REM-OBJECT §12.9, Hostile-Input Posture; REM-ENCRYPT §12.9, Envelope
Hostile-Input Discharge; and REM-PARITY §16.2, Hostile-Input Posture.

### 2.6. Escape text read from the medium before showing it

Bootstrap keys 3 and 4, and ParityMap keys 6 and 7, hold text chosen by
whoever wrote the tape. They are the first human-readable text a diagnostic
tool prints from an unknown cartridge, and the operator reading them is
deciding whether the cartridge is damaged or hostile. Text that carries
terminal control sequences or markup can change what the operator sees, or
what a program that receives the output does.

A tool that renders any of these values should escape it, so that no part of
it can be interpreted as a control or formatting instruction by whatever
receives the output. The specification bounds the text and fixes how a reader
treats a value outside the bounds: "A Reader MUST tolerate the absence of
either key, and MUST treat a value violating either rule exactly as it treats
that key's absence, for every purpose." (REM-PARITY §8.2). Escaping is what
keeps a value inside the bounds from doing harm.

Serves REM-PARITY §8.2, CBOR Payload; REM-PARITY §10.1.4, Payload (CBOR); and
REM-PARITY §16.2, Hostile-Input Posture.

### In plain terms

A reader has two jobs: read valid objects correctly, and stay standing when it
is handed something that is not one. The specification takes care of the
first. This chapter is about the second: check every number before you act on
it, never hold more than you have checked, escape any text from the medium
before you show it, and test the parser with inputs designed to break it.

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
`stored_digest`. The digest then costs arithmetic and no extra read. For an
encrypted copy, we recommend computing its `stored_digest` in the same way,
over the envelope bytes as the sealer emits them.

Serves REM-OBJECT §4.9, Writer, Planner, and Reader Obligations; REM-OBJECT
§7.2, Write-Path Verification (No Extra Reads); and REM-ENCRYPT §7.2,
Write-Path Verification.

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

Serves REM-OBJECT §7.3, Post-Write Re-Verification.

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

For an encrypted copy, we recommend recording also its `format_version`,
`metadata_frame_len` and `key_frame_len`, and the recipient epoch ids present
in its key frame. These are public envelope geometry. They let a tool choose a
key and plan reads without opening the object, but they do not replace
parsing the envelope's own header and key frame, which is where a reader
takes them from.

Serves REM-OBJECT §8.1, The Byte-Format Contract; REM-OBJECT §3.4,
Representation Detection; REM-OBJECT §4.9, Writer, Planner, and Reader
Obligations; REM-OBJECT §7.2, Write-Path Verification (No Extra Reads); and
REM-ENCRYPT §8.1, Backend Records for Encrypted Copies.

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

### 5.6. Keep a tape catalog rebuildable from the commit records

A catalog of tapes records which files each tape holds, where they are, and
what state each tape is in. It is quick to query and convenient to trust. After
a crash it can disagree with the tape, and with the records that committed the
tape's files, because it was updated after them.

We recommend treating a tape catalog as a projection of the commit records:
rebuild it from those records after a crash, never the reverse, and never let a
catalog entry stand in for commit authority or for a finalized tape's terminal
inventory. The specification fixes the part that concerns claims: "A record the
implementation does not treat as commit authority, such as a rebuildable
catalog or cache, does not commit a tape file." (REM-PARITY §3.4), and "An
off-tape catalog is a cache." (REM-PARITY §12.1). Remanence, for example, can rebuild its SQLite catalog from
its journals with `rem rebuild-catalog-from-journals`.

Serves REM-PARITY §3.4, The Durable Boundary, and REM-PARITY §12.1, Inputs and
Authority.

### In plain terms

The objects can always be read without the catalog, but the catalog is what
lets an archive find, check and trust them quickly. Record enough to read and
scrub every copy, store each fact once, keep hardlinks resolvable, rebuild a
tape catalog from the commit records rather than the other way round, and
guard the catalog as carefully as the archive it describes.

## 6. Keys and secrets

An encrypted copy stays readable only while a key that opens it survives, and
stays confidential only while the secrets that went into it stay secret.
REM-ENCRYPT fixes how the envelope binds its keys, and defines no key registry
or custody protocol (REM-ENCRYPT §1.5). This chapter recommends how a tool
should choose recipients, hold private keys, obtain randomness and handle
secrets while it works, and how long to keep keys. Its first section, on
attribute values, comes from REM-OBJECT; the rest from REM-ENCRYPT.

### 6.1. Report attributes by name, never by value

Attribute values and extension members can hold secrets: credentials kept in
`user.` attributes, security labels, or the private metadata of another
operating system. Logs are copied, shipped and retained far more widely than
the data they describe.

A restoring tool that reports skipped or applied attributes and extensions
should report their names only, and should never log their values.

Serves REM-OBJECT §12.10, Path Traversal.

### 6.2. Seal to at least two independent recipients

Any one matching private key opens an encrypted object, and an object whose
every recipient key is lost cannot be opened by anyone. An archive that seals
to a single recipient loses everything sealed to it when that one key is
lost.

We recommend sealing to at least two independent recipients, held in
separate custody, or else protecting the sole recipient's secret
independently, for example by splitting its seed with Shamir's secret
sharing. A tool should default to at least two recipients, and should seal to
one only when its operator opts in explicitly. The pinned vector
`writer-one-slot` records what a sealer that follows this default does: it
refuses a one-recipient seal made without that opt-in. It is an informative
vector, as REM-ENCRYPT §13.4 says, because a single-recipient envelope is
valid and "Readers accept any canonical frame with one through eight slots."
(REM-ENCRYPT §5.3). Remanence, for example, refuses a one-recipient seal
unless its caller sets `allow_single_recipient`.

Serves REM-ENCRYPT §5.3, The Key Frame and HPKE Wrapping, and REM-ENCRYPT
§13.4, Negative Vectors.

### 6.3. Fail the whole seal when a recipient cannot be wrapped

A sealer asked to seal to three recipients that quietly seals to two leaves
the third without access, and nobody learns of it until that key is the one
that is needed.

A sealer that cannot wrap the data key to one of the recipients it was asked
to seal to should fail the whole seal, rather than emit an envelope without
that recipient. The specification fixes what may be reported: "A Sealer MUST
NOT report a seal as successful unless the key frame contains a slot for
every recipient it was asked to seal to." (REM-ENCRYPT §5.3). Failing the
seal is the simplest way to meet it.

Serves REM-ENCRYPT §5.3, The Key Frame and HPKE Wrapping.

### 6.4. Keep each private key as its 32-byte seed

A private key is useful only if some tool can load it when it is needed,
perhaps decades later and in another program. The specification defines one
form for it: a recipient epoch's "secret custody form is the 32-byte X-Wing
seed" (REM-ENCRYPT §2.3). The expanded ML-KEM decapsulation key and the
X25519 secret `sk_X` are derived from the seed, and no REM specification
defines how to store them.

We recommend storing each private key as its seed, and deriving the expanded
decapsulation key and `sk_X` in memory only, when the key is used. A key file
should contain the seed, not an expanded key. Thirty-two bytes can also be
written on paper, stamped into metal or split among custodians, which an
expanded key does not allow as easily.

Serves REM-ENCRYPT §5.3.1, Frozen X-Wing Construction, and REM-ENCRYPT §5.4,
Key Inputs and Identification.

### 6.5. Pin recipient public keys

A sealer that takes a recipient's public key from an untrusted channel can be
given an attacker's key instead. The object is then sealed to the attacker,
who can read it, and nothing in the envelope shows it.

Where public keys could be substituted, we recommend pinning each recipient
public key, or its fingerprint, through a channel independent of the one that
delivers it, and checking the pin before every seal. Public keys and their
fingerprints are inputs to custody, which the format does not define.

Serves REM-ENCRYPT §5.4, Key Inputs and Identification, and REM-ENCRYPT
§12.11, Threat Model and Secret Handling.

### 6.6. Take randomness from the operating system, and fail when it is missing

The confidentiality of every object rests on its data-encryption key and its
encapsulation randomness being fresh and unpredictable. A tool that falls
back to a weak source when the operating system's generator is unavailable
produces objects that look sound and are not.

We recommend obtaining the data key and the encapsulation randomness from the
operating system's cryptographically secure generator, through an interface
that reports failure, and failing the seal with `EntropyUnavailable` whenever
that source cannot supply them. The specification requires the outcome:
"Every seal MUST use a fresh uniformly random 32-byte DEK and fresh HPKE
encapsulation randomness for every recipient, following [RFC9180] §9.2.3.
Entropy failure is fatal." (REM-ENCRYPT §12.1).

Serves REM-ENCRYPT §5.4, Key Inputs and Identification, and REM-ENCRYPT
§12.1, Per-Object Key Uniqueness.

### 6.7. Derive the salt inside the sealer

An interface that accepts a salt from its caller invites callers to supply
one. An envelope sealed under any salt other than the one derived from its
data key does not open: a reader rederives the salt and rejects a mismatch
with `SaltDerivationMismatch`.

A sealer's interface should derive the salt itself, as the specification
requires ("A Sealer MUST derive the salt." REM-ENCRYPT §5.5), and should
offer no way for a caller to supply one.

Serves REM-ENCRYPT §5.5, Salt and Object-Key Derivation.

### 6.8. Seal with the current suites

A suite is superseded for a reason, usually because a better construction has
replaced it. A new seal under a superseded suite extends that suite's life by
the lifetime of the object.

We recommend sealing new objects with the current `suite_id` and
`wrap_suite` only. Objects already sealed under a superseded suite remain
readable, because "Superseded suites remain valid for **opening**."
(REM-ENCRYPT §10.4), and they keep that suite's protection until they are
resealed (section 6.11). Today there is one current suite of each kind.

Serves REM-ENCRYPT §10.4, Assignment and Deprecation Policy.

### 6.9. Keep an epoch's private key while any object needs it

A key frame cannot be rewritten without resealing the object, so a recipient
cannot be added to an object once it is sealed. An object can be opened only
while the private key of at least one of its recipient epochs survives, and a
key destroyed too early cannot be replaced for the objects already sealed to
it.

We recommend never destroying a recipient epoch's private key while any live
object references it. Retire a key only when the catalog shows that no live
object depends on it, or once every object that did has been resealed to
other recipients.

Serves REM-ENCRYPT §12.8, Key Rotation and Epoch Longevity.

### 6.10. Keep secrets few, short-lived and out of records

A secret can leak from any place where a copy of it persists: memory that is
swapped or dumped, logs and diagnostics, command lines that other users can
see, core dumps, and plaintext staged during recovery. Section 6.1 applies the
same reasoning to attribute values.

We recommend keeping as few copies as possible of data keys, derived keys,
private keys, HPKE ephemeral secrets and random-generator state, and zeroising
the mutable buffers that hold them as soon as they are no longer needed. A
tool should never write a secret to a log, a diagnostic, a command line or
durable plaintext staging.

A core dump needs more than care about what the tool writes, because it
captures memory that the tool never chose to write out. We recommend
disabling core dumps for processes that hold secrets, or excluding
secret-bearing memory from dumps where the platform allows it, for example
with `madvise` and `MADV_DONTDUMP` on Linux. Where neither can be assured, a
dump file should be treated as secret-bearing.

Serves REM-ENCRYPT §12.11, Threat Model and Secret Handling.

### 6.11. Reseal alongside media migration

Resealing an object, to new recipients or under a new suite, reads, opens and
rewrites the whole object, and on append-only media produces a new copy.

We recommend planning resealing together with media migration, which reads
and rewrites the objects anyway. A resealed copy keeps the object's
`object_id`, `chunk_size`, canonical bytes and `plaintext_digest`, and has new
envelope bytes and a new `stored_digest` (REM-ENCRYPT §12.8), so a catalog
should record it as a new copy of the same object.

Serves REM-ENCRYPT §12.11, Threat Model and Secret Handling, and REM-ENCRYPT
§12.8, Key Rotation and Epoch Longevity.

### 6.12. Decide what an encrypted copy may reveal, and where provenance comes from

Encryption hides an object's contents, not its existence. An encrypted copy
reveals its identifier, its recipient epochs and their labels, and its size,
among the public facts REM-ENCRYPT §12.5 lists. Because anyone who holds the
recipient public keys can make a new, internally valid object, encryption
also does not show who made an object.

A deployment that treats an object's existence, identifier or approximate
size as sensitive should add its own policy above the format, which defines
no padding. A deployment that needs provenance should keep an independently
authenticated or signed external manifest, because "REM-ENCRYPT claims
confidentiality and self-consistency, not writer identity or provenance."
(REM-ENCRYPT §12.7).

A tape adds public facts of its own. REM-PARITY's structures are plaintext on
the tape and reveal the number of objects, each object's block count, the write
timeline and a CRC-64 of every stored block (REM-PARITY §16.4). An unkeyed CRC
of a stored block can confirm a guess about that block's content. We recommend
storing objects in the encrypted representation whenever their content is
confidential, so that every stored block, and every CRC and parity block
computed from it, is a function of ciphertext.

Serves REM-ENCRYPT §12.5, Confidentiality Boundary, Public Facts, and Catalog
Trust; REM-ENCRYPT §12.7, Non-Committing AEAD; and REM-PARITY §16.4, Structure
Leakage and Confidential Payloads.

### In plain terms

Seal every object so that losing one key does not lose the object, keep each
private key as its 32-byte seed, trust a public key only when it has been
checked independently, and take randomness only from the operating system.
While a tool works, it should hold secrets briefly and never write them where
others can read them, attribute values included. Keep every key for as long
as an object needs it.

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
representation, rather than only the first. The same holds for a tape
verifier, which checks a tape's structures and digests end to end.

Serves REM-OBJECT §7.4, Verifier Profile; REM-ENCRYPT §7.4, Encrypted Verifier
Profiles; and REM-PARITY §2.2, Conformance Roles.

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

A tape tool should likewise expose typed errors equivalent to the taxonomy of
REM-PARITY §15. That specification makes the same distinction and one more:
"Refusals (Section 13.2), parse failures, and reconstruction failures MUST
remain distinguishable; I/O faults MUST remain distinct from format
violations." (REM-PARITY §15).

Serves REM-OBJECT §11, Errors, and REM-PARITY §15, Errors.

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

Serves REM-OBJECT §9, Relationship to the Parity Layer, and REM-ENCRYPT §12.4,
Fail-Closed.

### 7.8. Validate and stage recovered plaintext before publishing it

Authentication shows that an envelope is intact and was sealed under its
keys. It does not show that the plaintext inside is a valid REM-OBJECT, or
that it is the object the header names: a defective sealer can seal the wrong
stream. Members published from such a recovery, or published before the
recovery has finished, may be wrong or incomplete.

After whole-object authentication, we recommend validating the recovered
inner stream under the Verifier profile of REM-OBJECT §7.4, staging the
recovered plaintext on protected storage, and publishing restored members only
after the whole recovery has succeeded. The specification fixes the part that
concerns what a tool may claim: "A Keyed Reader MUST NOT publish restored
members until it has compared the inner stream's `REMANENCE.object_id` and
`REMANENCE.chunk_size` with the scalar header." (REM-ENCRYPT §5.10). Section
6.10 explains why the staging area should be protected.

Serves REM-ENCRYPT §5.10, Opening, Recovery, and Keyless Inspection, and
REM-ENCRYPT §12.11, Threat Model and Secret Handling.

### In plain terms

Check the index before trusting it, look for the inexpensive signs of
trouble, report everything you find, never pass off a rescue as a clean
restore, and check every copy on a schedule. When a copy is damaged, mend its
bytes first and open it afterwards. When you decrypt a copy, check what came
out and keep it staged until the whole recovery has succeeded.

## 8. Writing tapes: sessions, commit and resume

REM-PARITY fixes what a committed tape file is, what the bytes of a tape must
be, and where a resumed session appends. It does not say how a writer should
keep its commit records, batch its synchronization, react to a failed write or
seed a resumed session. Those choices decide whether a writer that crashes
leaves a tape that can be continued safely. This chapter collects the practice.

### 8.1. Put enough in each commit record to resume

A commit record is what makes a tape file part of the committed prefix. A
record that lacks the file's place in the map, or the state a resumed session
needs, leaves a later session unable to find the true append point, and a write
at the wrong point overwrites committed data.

We recommend that each commit record hold the tape file's filemark-map entry
(REM-PARITY §7.1) and enough state to seed a resumed writer (section 8.9). An
object and the sidecars emitted at its close can share one durable
transaction, because they are committed together. How the record is stored is
the implementation's own choice: "The commit record's format is
implementation-defined (a journal, a database row, a replicated log entry)."
(REM-PARITY §3.4).

Serves REM-PARITY §3.4, The Durable Boundary.

### 8.2. If commit authority spans several records, name them and make them agree

Some implementations spread one logical commit record over several durable
records, for example a journal of tape files and a journal of checkpoints. If
one of them is missing, or two of them disagree, a resumed session can derive
the wrong prefix and write over committed data.

We recommend naming which records are required commit authority, and, before
positioning or writing, requiring every one of them to be present and their
overlapping claims to agree. When they do not agree, a tool should stop, and
restore one unambiguous commit record through a deliberate recovery procedure,
rather than choose between them. Before finalization these records, not the
tape, are the authority for the open prefix and the append position. The
specification fixes the outcome: "Before a Resumer positions to an append point
or writes, the validated combination of the records it relies on MUST determine
exactly one committed prefix and its append point." (REM-PARITY §3.4); a resume that
cannot establish one fails as `ResumeAppend`. Remanence, for example, keeps a
tape-file journal and a checkpoint journal for each tape and compares their
histories entry by entry before it appends; the
[on-tape layout reference](reference-tape-layout.md#on-disk-durable-records-and-rebuildable-state)
describes them.

Serves REM-PARITY §3.4, The Durable Boundary, and REM-PARITY §12.1, Inputs and
Authority.

### 8.3. Batch synchronization behind one barrier

A synchronous filemark after every tape file makes the drive stop and wait for
the medium each time, which costs it its streaming speed.

Consecutive tape files can share one synchronizing barrier, and one commit
record, or one durable transaction, can cover the whole batch. A writer usually
keeps, in memory, a projection of the filemark map and a record of its durable
boundary. We recommend adding a tape file's entry to that projection, and
advancing the boundary past the file, only once the file's synchronization is
proved: at its own synchronous filemark, or, under deferral, at the barrier,
for every file the barrier covers. The specification fixes when the batch counts as committed. Of the
barrier it says: "It MUST complete before the commit record of any file it
covers takes effect." (REM-PARITY §11.1).

Serves REM-PARITY §11.1, Commit Discipline (per Tape File).

### 8.4. Treat staged records as uncommitted until their barrier

A writer that records files durably before their barrier completes can crash
between the record and the barrier. A replay that trusts such a record counts a
file as committed that may never have reached the medium.

A writer can stage durable records for files that are written but not yet
proved, but replay should disregard any staged record that is not followed by a
commit marker written after its barrier. Remanence, for example, keeps such
records as orphan evidence and refuses to append to the tape until they have
been reconciled with the tape's physical tail.

Serves REM-PARITY §11.1, Commit Discipline (per Tape File).

### 8.5. Stop the session when an outcome is unknown

After a failed or completion-unknown barrier, or an end-of-medium report, a
writer no longer knows exactly where the head is or which blocks reached the
medium. A further write can land over data the writer believes is committed.

We recommend poisoning the writer in each of these cases, and after any failure
whose completion is unknown: a poisoned writer refuses every further operation
in the session. A write failure whose completion is known, one that consumed no
position, can leave the writer usable. The specification fixes what may be
claimed: after such an outcome "every file the barrier would have covered
remains uncommitted" (REM-PARITY §11.1), and "Failure at any step MUST abandon
the in-flight file." (REM-PARITY §11.1). Remanence, for example, poisons its
parity sink after a barrier error, an end-of-medium report or a failed position
check, and keeps it usable after a block write that failed without consuming a
position.

Serves REM-PARITY §11.1, Commit Discipline (per Tape File).

### 8.6. Write with memory bounded by the geometry

An object can be far larger than a parity epoch, and an epoch far larger than a
host's memory.

Because parity accumulates incrementally (REM-PARITY §6.3), a writer needs to
hold only `S × m` block-sized parity accumulators and one CRC per pending data
block, whatever the sizes of the objects it writes. At the default geometry the
accumulators take 512 MiB (REM-PARITY Appendix A.1). A sidecar that is complete
but not yet written can wait in memory or in a spool file until the current
object closes.

Serves REM-PARITY §11.2, Epochs and Sidecars.

### 8.7. Prove that compression is off before writing parity

Hardware compression breaks the correspondence between logical blocks and
media on which the damage model of parity rests, and it does so while appearing
to work (REM-PARITY §16.3). A drive can also ignore a setting, or a later
command can change it.

We recommend setting the drive's compression off and then reading the setting
back, before the first parity write of a session, and refusing to write if the
read-back does not confirm it. The specification requires the outcome: "Drive
hardware compression MUST be verified off before any parity write." (REM-PARITY
§11.4). Remanence, for example, sends MODE SELECT, checks the result with MODE
SENSE, and records the observed value as bootstrap key 5.

Serves REM-PARITY §11.4, Session Preconditions.

### 8.8. Know where the head is before writing

Counting blocks as they are written, dead reckoning, is fast and usually right.
After a filemark, the end of data, an unclassified error or a read that crossed
tape files, the count can be wrong, and a write issued at a wrong position lands
over committed data or short of the append point.

Dead reckoning is fine between checks. We recommend resynchronizing with a
positional query, for example SCSI READ POSITION, after any boundary or
unclassified error, and verifying the append point by a positional query before
the first block a resumed session writes. The specification fixes the outcome:
"The first block written lands exactly at the append point" (REM-PARITY §14).
Remanence, for example, locates to the append point on resume, reads the
position back with READ POSITION, and refuses to write if the two differ.

Serves REM-PARITY §3.5, Requirements on the Tape I/O Layer, and REM-PARITY §14,
Resumer Obligations.

### 8.9. Resume a tape from its commit records

A resumed session has to continue the parity of the epoch that was open when
the last session ended, and finalization has to describe the whole tape later.
A writer that seeds itself from anything less than the committed prefix
produces a sidecar or a terminal inventory that disagrees with the tape.

We recommend seeding a resumed writer with the complete replayable prefix map,
the Object recovery rows, one full directory entry for every committed sidecar,
the durable boundary, `W`, the next epoch id, and the open epoch `[W, T)`
rebuilt from the tape (REM-PARITY §14, step 3). That source should stay
replayable, so that finalization can later emit the final ParityMap and stream
the same snapshot into replicas A, B and C. A generation-2 tape carries at most
one ParityMap, so its `sequence` counter has no consequence for a reader.

At resume time the writer should not close `[W, T)` or emit a sidecar for it;
the epoch closes later, through the ordinary cycle. When a sidecar is emitted,
we recommend re-parsing its encoded bytes and comparing them with the planned
header, index and shard bytes before committing it. A correct encoder never
fails this check, and a defective one is caught before its bytes are committed.
Remanence, for example, does not close `[W, T)` at resume, and re-parses a
rebuilt sidecar before it writes it. The pinned vector `resume-round-trip`
records the bytes of a writer that does not close `[W, T)` at resume.

Serves REM-PARITY §10.3, Object Recovery Rows, and REM-PARITY §14, Resumer
Obligations.

### In plain terms

A tape writer keeps a record, off the tape, of what it has committed. Put enough
in that record to pick up where you left off, and if the record is spread over
several places, make sure they agree before you write. Wait for the drive to
confirm that data is on the medium before calling it committed, stop the
session when you are unsure what happened, check the head's position before
writing, and prove that compression is off before writing parity.

## 9. Finalizing a tape and recovering from a crash

Finalization writes five components at the end of a tape, in order: replica A,
separation AB, replica B, separation BC and replica C (REM-PARITY §8.3). Each
takes time, and a crash can come between any two of them. REM-PARITY fixes
what a finalized tape contains and what a tool may report about it. This
chapter is about getting there across crashes and failures.

### 9.1. Make the decision to finalize durable before the first terminal write

A tool that begins writing terminal components before it has durably recorded
its decision to finalize can crash, restart without knowing that the decision
was made, and append an Object after a partial suffix. The tape then carries
control structures among its Objects.

We recommend recording the transition to `Finalizing` durably before any
terminal media motion, and refusing every Object from that moment. The
specification fixes the effect: "The accepted transition to `Finalizing`
permanently disables Object admission." (REM-PARITY §3.4).

Serves REM-PARITY §3.4, The Durable Boundary.

### 9.2. Record progress after each barrier-proved component

A replica's footer records where the writer believed it was, but it does not
prove the replica's trailing filemark, its barrier or any host record
(REM-PARITY §8.4). A tool that reads its progress back from the tape, or that
records progress before a barrier completes, can skip a component or write one
twice.

We recommend recording durable progress at six points, before replica A and
after each of the five components, and advancing only when a component's
barrier has completed and the component agrees with the plan. A count of
completed replicas is a convenient summary, but it is not enough to resume
from: after a complete separation extent, only the six-point record says that
the extent must not be written again.

Serves REM-PARITY §3.4, The Durable Boundary; REM-PARITY §11.3, Finalization;
and REM-PARITY §12.6, Terminal Completeness.

### 9.3. Finish the host steps on restart without moving the tape

When replica C has been proved, the tape is complete, but the tool may not yet
have recorded that durably, or updated its catalog. A crash at that moment
leaves the tape finished and the host unfinished.

On restart, a tool in that state should finish its host records without further
media motion: no terminal component is repeated, and nothing is written after
C. If a completed record and a stale intent to finalize both survive and
agree, the completed record should take precedence. If they disagree, the tool
should stop rather than choose. Remanence, for example, keeps a sealed
checkpoint and a companion intent. On the uninterrupted path it retires the
companion as soon as the sealed checkpoint has been fsynced, before its final
catalog projection. When a crash leaves both behind, startup recovery projects
the sealed checkpoint first and retires the companion only after that
projection succeeds. The
[on-tape layout reference](reference-tape-layout.md#finalization-and-catalog-less-recovery)
describes the sequence.

Serves REM-PARITY §3.4, The Durable Boundary.

### 9.4. Keep a failure's classification across restarts

A component that failed, or whose completion is unknown, leaves the medium in a
state that must be reconciled before finalization continues. A restart that
forgets the failure proceeds as if the component were intact.

We recommend storing the classification durably with the finalization's
progress, keeping it across restarts at the same progress, and clearing it only
when the next component succeeds or, after replica C, when a normal completed
record has been made durable. The specification calls the state
`RecoveryRequired` (REM-PARITY §3.4).

Serves REM-PARITY §3.4, The Durable Boundary.

### 9.5. Repair only what is missing, under the medium's rewrite policy

A component whose header was written but whose payload was not is torn terminal
control, and it sits at a planned position that later components depend on. On
rewritable media it can be rewritten. On WORM media, or where the tool cannot
prove where the component starts, a further write risks damaging what is
already there.

We recommend first reconciling the medium with the recorded progress; then
rewriting a torn component only where its start is proved and the medium allows
rewriting; and, on WORM media or at a start that cannot be proved, stopping with
no further motion. A tool should never remove its own barrier against new
Objects in order to recover. The specification fixes what a recovery may not
do: "It MUST NOT write an Object or append a second terminal triple."
(REM-PARITY §3.4). Remanence, for example, classifies a header-only component
that it finds on restart as torn terminal control, never as an Object.

Serves REM-PARITY §3.4, The Durable Boundary, and REM-PARITY §12.3, The
Classification Ladder.

### 9.6. Accept reduced redundancy only as a separate, deliberate decision

If a component is torn on WORM media after one or two replicas were completed,
those replicas are complete inventories, but the tape will never have three.
Treating it as finished without saying so would hide, from later readers and
operators, that one damaged region could now cost the whole index.

We recommend that a tool remain in `RecoveryRequired` in that case, and that
accepting the reduced set be a distinct operator decision, recorded in the
tool's audit trail. The specification fixes what may be claimed in the
meantime: "A Writer MUST NOT report a tape as `Finalized` while fewer than three
complete replicas exist." (REM-PARITY §3.4). It allows the separate acceptance,
as `FinalizedDegraded`, without defining it. Remanence does not yet offer that
decision: its catalog can represent a `finalized_degraded` outcome, but nothing
produces it, so such a tape stays in `RecoveryRequired`.

Serves REM-PARITY §3.4, The Durable Boundary.

### 9.7. Check the position against the planned end of data

A drive that reports success for every component can still leave the head
somewhere other than planned, for example after a silent position error.

After replica C's barrier, we recommend one read-only position check against
the planned end of data, `expected_eod_lba` in the terminal layout (REM-PARITY
§8.3). Remanence, for example, makes this check.

Serves REM-PARITY §8.3, Placement (Writer).

### 9.8. Stream the inventory into the replicas

A tape can hold a million Objects, and each replica carries a 256-byte row for
every one of them.

Finalization can stream the complete row set from the writer's replayable
source into A, B and C, and needs no allocation for the whole index. The three
replicas should carry the same snapshot, fixed before replica A (REM-PARITY
§8.3).

Serves REM-PARITY §10.3, Object Recovery Rows.

### In plain terms

Finalizing a tape writes its index three times at its end. Decide durably
before you start, record progress only after each piece is proved on the
medium, and on restart finish what the tape already shows rather than writing
it again. When something breaks, repair only what is missing, and do not call a
tape finished with fewer than three good copies of its index unless someone has
decided, on the record, to accept that.

## 10. Reading tapes

REM-PARITY fixes what a reader must accept, reject and report. It leaves open
how a reader should get there efficiently, and how it should present a long
operation to its operator. This chapter covers those choices, for inventories
and for recovery.

### 10.1. Classify boundaries in both sense-data formats

A read that meets a filemark or the end of data is reported through sense data,
and a SCSI transport can report it in either of two formats, fixed and
descriptor. A reader that understands only one of them misses boundaries, and
builds a wrong map of the tape.

We recommend handling both formats, and testing the tool against both. The
specification requires the outcome: "Boundary classification MUST distinguish
the Filemark and EndOfData outcomes on every transport, and on a SCSI transport
in both of its sense-data formats [LTO-SCSI]." (REM-PARITY §3.5).

Serves REM-PARITY §3.5, Requirements on the Tape I/O Layer.

### 10.2. Compare the envelopes first, and read a body only when needed

A replica's body can run to gigabytes, and reading three bodies to compare them
costs time and wear. But a replica that is valid on its own may still disagree
with another, and accepting it without looking would hide the conflict.

Every field that the replicas must share is in the envelope, the header and
footer of each replica (REM-PARITY §8.5 and §10.4). We recommend reading all
three envelopes; reading one body on the healthy path, where the envelopes
agree; and replaying bodies only for envelopes that disagree, to find which
editions have payload-valid survivors. The specification fixes the rule that
the comparison serves: "A Scanner MUST NOT accept a replica while another fully
valid replica differs from it in any edition-common field" (REM-PARITY §8.5).
Remanence, for example, reads all three envelopes on every inventory and reads
one body when they agree.

Serves REM-PARITY §8.5, Authoritative Selection, and REM-PARITY §12.4, Terminal
Replica Validation.

### 10.3. Among agreeing replicas, read C, then B, then A

Agreeing replicas give the same inventory whichever of them is read, so the
choice is about cost.

We recommend trying C first, then B, then A. Discovery starts from the end of
data (REM-PARITY §8.4), and C is the last component on the tape, so on a healthy
tape the reader reads the body nearest to where the head already is. Remanence
follows this order.

Serves REM-PARITY §8.5, Authoritative Selection.

### 10.4. Stream the inventory as provisional attempts

A consumer that stores inventory rows as they arrive, before the replica
carrying them has been fully validated, can keep rows from a replica that later
fails.

We recommend giving each attempt at a replica its own identifier, marking its
rows as provisional until the terminal summary names the selected attempt, and
emitting an explicit rejection for a failed attempt before trying the next.
The stream can then be bounded and backpressured, with no buffer for the whole
index and no second read of a body on the healthy path. When envelopes
conflict, resolving them can need a bounded replay, and the selected attempt
can then be replayed for its consumer. The specification fixes the consumer's
side: "A Consumer MUST commit only the attempt named by the terminal summary and
MUST discard rejected or unselected attempts." (REM-PARITY §12.4). Remanence,
for example, gives each attempt an `attempt_id` in its inventory stream and
names a rejected attempt before it falls back to the next.

Serves REM-PARITY §12.4, Terminal Replica Validation.

### 10.5. Check the role magic first

A matching magic on a malformed control structure is a damaged index, not an
Object. A reader that parses fields before it checks the magic can mistake
damaged control for an Object, or spend its resources on a structure it would
have rejected.

We recommend checking a tape file's role magic before anything else, then the
frame's fixed fields, then every size formula before it drives an allocation or
a seek (section 2.2), and only then the plan, the digests and the payload. The
specification fixes the conditions but not their order, except that "a matching
role magic commits the tape file to its control type, so malformed control
never falls through to Object" (REM-PARITY §10.6).

Serves REM-PARITY §10.6, Replica Validity Conditions, and REM-PARITY §12.3, The
Classification Ladder.

### 10.6. Walk a tape from BOT with a notice, progress and a way to stop

When all three replicas are lost, the only way to a map is a structural walk of
the whole tape from BOT (REM-PARITY §8.4.1). It can take hours. An operator who
cannot see it progress, or stop it, cannot plan around it, and a tool that keeps
commanding motion against a drive or medium that refuses it can make the damage
worse.

We recommend that a tool:

- say that it is falling back to the walk before it starts any BOT recovery
  I/O;
- report progress at least once per tape file crossed: the tape-file ordinal,
  the position as a logical block address, the candidates found so far, and
  the elapsed time;
- let the operator abort between tape files, and on abort report the last
  tape-file ordinal crossed, the candidates found and the drive's position, or
  say that the position is not known;
- once it has decided to abort, not read the first record of the next tape
  file;
- stop and report after a small number of consecutive positioning failures
  between tape files, for example eight, rather than keep commanding motion,
  and count read failures separately; and
- accept the hints it can use: the expected tape UUID and block size, which the
  specification requires when the bootstrap is unreadable, and an expected
  tape-file count and capacity, which are useful only for estimating progress.

The specification fixes what a hint may not do: "Hints MUST NOT cause any tape
file to be skipped." (REM-PARITY §8.4.1). Remanence, for example, emits one
start notice and one progress event for every structurally complete tape file,
and lets the operator abort between files. It has no positioning-failure
budget: its walk stops at the first positioning failure.

Serves REM-PARITY §8.4.1, All-Replicas-Invalid BOT Walk.

### 10.7. Refuse an unrecoverable request before reading the tape

A request to recover an ordinal outside the validated scope, in the pending
epoch whose parity does not exist yet, or in a tape file beyond the durable
boundary cannot succeed. Attempting it costs tape motion and ends in failure
anyway.

We recommend making those refusals before any tape read, so that a request
that cannot succeed costs no motion. The specification fixes the refusals
themselves: "The Recoverer MUST reject, as typed refusals distinct from
recovery failures" each of those cases (REM-PARITY §13.2). Two of the pinned
recovery vectors describe their cases as refused before I/O.

Serves REM-PARITY §13.2, Typed Refusals.

### 10.8. Plan bulk recovery by epoch, and read in tape order

Recovering a damaged region can need every peer of many stripes. Read without a
plan, the same peer is read again for each stripe, and the head moves back and
forth across the tape.

We recommend planning per epoch, reading each needed peer at most once per
planning window, and reading in physical tape order. Window and cache sizes are
choices of the implementation, not rules of the format. Remanence, for example,
bounds a planning window at 1024 stripes and its recovery cache at 8 GiB.

Serves REM-PARITY §13.6, Bulk Recovery (Informative).

### In plain terms

Read the cheap parts first and the expensive parts only when needed: compare
the three index envelopes, read one body, and start from the end of the tape.
Keep a consumer from trusting rows until the reader has chosen them. When the
index is gone and the whole tape must be walked, tell the operator, show
progress, and let them stop. Refuse what cannot be recovered before moving the
tape, and recover in tape order.

## 11. Capacity admission

A REM-PARITY tape is finished by writing a terminal suffix at its end: the final
ParityMap when there are sidecars, three replicas and two separation extents
(REM-PARITY §8.3). A tape that runs out of room before its suffix is written can
still be read, but only by a structural walk from BOT, and it never gains its
catalog-less index. This chapter is about keeping that room.

### 11.1. Admit an Object only if the tape can still be finalized

We recommend refusing, before any tape motion for it, an Object that would
leave less than the tape's close reserve after it. The specification defines
the reserve: "A tape's close reserve is the space its finalization needs: parity
closeout, the checked terminal payload `64 × structural_row_count + 256 ×
object_row_count` in three rounded replica records, both separation extents,
and five filemark charges." (REM-PARITY §10.3). The terminal payload grows with
every Object, so the reserve should be recomputed for each admission, with
checked arithmetic, and an overflow should refuse. Remanence, for example,
refuses with `CapacityReserveExceeded` when the reserve would not remain, and
with `ObjectTooLargeForEmptyTape` when the Object and the reserve could not fit
even on an empty tape.

Serves REM-PARITY §10.3, Object Recovery Rows, and REM-PARITY §11.4, Session
Preconditions.

### 11.2. Keep a safety allowance

The capacity a drive reports is an estimate, and filemarks and rounding take
space that a plan counts only approximately.

We recommend adding a small safety allowance to the reserve. Remanence, for
example, adds 4 blocks. Capacity charges, including a conservative charge for
each filemark, are not on-media locations and have no part in the terminal
layout digest (REM-PARITY §8.3).

Serves REM-PARITY §11.4, Session Preconditions.

### 11.3. Do not reapply caps after the tail is proved

Once replica C is proved, the reserved tail is already on the medium. A capacity
cap or watermark that changed since the tape was admitted can then only block
the host's remaining steps; it cannot protect any space.

We recommend skipping capacity checks on that host-only suffix. Remanence, for
example, does so.

Serves REM-PARITY §3.4, The Durable Boundary.

### 11.4. Use the default separation extent unless there is a reason

Each separation extent keeps two replicas physically apart, so that one damaged
region cannot take both. The default size is 1 GiB (`DEFAULT_INDEX_SEPARATION_BYTES`,
REM-PARITY §2.5). The format records the size in each extent's frame, so a tape
written with another size is read correctly (REM-PARITY §10.5).

We recommend the default unless a medium's damage profile argues for another
size. Remanence uses the default.

Serves REM-PARITY §10.5, Index Separation Extents.

### In plain terms

Always keep enough room at the end of a tape to write its index, and refuse
anything that would eat into it, with a little extra to spare.

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

### 13.2. Record the software that wrote each bootstrap and ParityMap

A tape is produced by an implementation, not by a specification, and
implementations have defects. When a cartridge does not decode as the
specification says it should, the only thing that resolves the disagreement is
knowing which software wrote it, so that its behaviour at that version can be
established. A conformance claim does not do that job: the claim states an
intention, and the tape is the result.

We recommend that a writer record its identity in bootstrap key 3 and in
ParityMap key 6, in the form `<implementation>/<version>`, optionally followed
by a space and a parenthesised build identifier, for example
`remanence/1.0.0 (v1.0.0-12-g874b111)`. The implementation part names the
software, not the format, so two conformant implementations will not agree on
it, and are not expected to. A ParityMap is written by software too, and can be
written by a different version of it than the bootstrap it accompanies, so each
should record its own. Readers tolerate the absence of both keys, as the
specification requires: "A Reader MUST tolerate the absence of either key"
(REM-PARITY §8.2). A tape written without them, or before this
recommendation, therefore remains fully readable. Remanence, for example,
writes both keys today with its version number alone, which identifies the
version but not the software.

Serves REM-PARITY §8.2, CBOR Payload, and REM-PARITY §10.1.4, Payload (CBOR).

### 13.3. Record when each bootstrap and ParityMap was written

The time a structure was written helps an operator place a cartridge in its
history, and it costs a few bytes.

We recommend recording it in bootstrap key 4 and in ParityMap key 7, as an
RFC 3339 `date-time` within the 64-byte bound. Remanence, for example, writes
bootstrap key 4 but not ParityMap key 7.

Serves REM-PARITY §8.2, CBOR Payload, and REM-PARITY §10.1.4, Payload (CBOR).

### 13.4. Choose the edition identity at random

The edition ID of a finalized tape binds its three replicas and its two
separation extents to one another. A reader checks only that it is not zero and
that it is equal across all five (REM-PARITY §10.4), so any nonzero value is
valid.

We recommend choosing it at random, for example as the bytes of a version-4
UUID. A random value makes it unlikely that components of two different
editions, for example after a defective tool rewrote part of a suffix, share an
edition ID and so escape the agreement check. Remanence, for example, uses a
version-4 UUID.

Serves REM-PARITY §10.4, Terminal Replica Framing and Digests.

### In plain terms

Give a directory its own entry when it is empty or has metadata of its own;
leave out entries that would only repeat what the file paths already say. Say
on the tape which software wrote it, and when, and choose each edition's
identity at random.

## 14. Revision history

- **26 September 2026.** Third revision. Chapters 8 to 11 are new. They hold
  the practice that REM-PARITY 1.0.0-draft.5 moved out of that specification:
  commit records, barrier batching, writer poisoning, position tracking, the
  compression check and resuming a tape (chapter 8); the finalization
  lifecycle and crash recovery (chapter 9); replica comparison and selection,
  the streaming inventory, check order, the BOT walk, recovery refusals and
  bulk recovery (chapter 10); and capacity admission (chapter 11). Chapters 2,
  5, 6, 7 and 13 gain REM-PARITY's practice on allocation, checked arithmetic,
  fuzzing and escaping, tape catalogs, confidential payloads, reporting and
  typed errors, and descriptive fields. Chapter 1 says the guide now holds the
  practice of all three specifications.
- **26 September 2026.** Second revision. Chapter 6 now holds the practice on
  keys and secrets that REM-ENCRYPT 1.0.0-draft.4 moved out of that
  specification: recipients and the two-recipient default, keeping private
  keys as seeds, pinning public keys, randomness, the salt interface, current
  suites, keeping epoch keys, handling secrets, resealing, and what encryption
  neither hides nor proves. Chapters 2, 4, 5 and 7 gain REM-ENCRYPT's practice
  on envelope parsing and fuzzing, computing an encrypted copy's
  `stored_digest` as it is written, recording envelope fields, and validating
  and staging recovered plaintext. Chapter 1 says which revisions the guide
  now holds.
- **26 September 2026.** First revision. Chapters 1 to 7, 12 and 13 hold the
  practice that REM-OBJECT 1.0.0-draft.4 moved out of that specification:
  handling hostile media, restoring onto a host, staging and durability,
  catalogs and indexes, keeping attribute values out of logs, verification,
  scrub and repair, ingest, and which directory entries a writer emits.
