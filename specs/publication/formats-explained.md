# The Remanence Formats, Explained

*A companion to the RAO Format Specification and the REM-PARITY Tape Format
Specification. This document is informative: it explains what the formats do
and why they are shaped the way they are, in plain language. The
specifications remain the only normative documents — where this text and a
specification disagree, the specification wins.*

---

## 1. The problem

Suppose that you are responsible for a large media archive — decades of
footage, growing daily — and that your task is to ensure that in fifty
years it still exists and can still be read. It is worth pausing over what
that undertaking actually requires, because the obvious answer turns out to
be the wrong one.

The obvious answer is that the enemy is physical decay, and that the task
is therefore to buy good media and keep it cool and dry. Magnetic tape
serves this purpose well: it is cheap per terabyte, it sits on a shelf
consuming no power, and its rate of spontaneous bit error is very low.
But experience — ours and the industry's — shows that archives are rarely
lost because the magnetic particles faded. They are lost in more mundane
ways:

- **The software dies.** The application that wrote the tapes is
  discontinued, the company folds, the license server goes dark, and the
  tapes remain full of a proprietary format that nothing can read.
- **The database dies.** The tapes are intact, but the catalog that knew
  what was on them — which file, on which tape, at which position — was on
  a server that failed, and what remains is a warehouse of unlabeled
  boxes.
- **The people leave.** The one person who understood the system retires,
  and the institution finds itself operating a machine it dares not touch.
- **A single flaw damages every copy alike.** All the copies were written
  by the same program, so one latent bug corrupted them all in the same
  way, and the redundancy that looked like safety was no safety at all.

Every design decision in these formats traces back to one of these four
failures. The question that governed the design was not "how shall we
write tapes efficiently?" but a more patient one: what will the person who
finds these tapes in thirty years need, supposing that they have nothing
but the tapes and a copy of the specification?

## 2. The landscape, and the gap

A person shopping for tape archival software today will find two shelves,
and it is instructive to examine both.

The first shelf belongs to the enterprise vendors — IBM, Oracle, Quantum,
and the media-asset-management systems built above them. Their products
work, and it would be unjust to pretend otherwise. The difficulty is what
they cost and what the cost buys. The licensing is per terabyte, per
drive, and per year; changing a setting may require professional services;
and the deeper price is dependency, for the on-tape format is the
vendor's, the catalog is the vendor's, and on the day the contract lapses
or the product is discontinued, the archive's readability acquires an
expiry date. An institution that thinks in decades must regard the
licensing model itself as an archival risk, quite apart from the expense.

The second shelf is open source, and it holds two kinds of thing. The
backup suites — Bacula and its descendants — are honest tools built for a
different purpose: the rotation of short-lived backup sets, not the
preservation of fixed masters, and their on-tape formats are their own.
The remainder of the shelf, in one form or another, rests on LTFS, which
deserves to be treated carefully, because it is half right, and the half
that is right matters.

What LTFS got right is openness. It is a vendor-neutral, standardized
format; the tape carries its own index; a cartridge written by one
system can be read by another. This openness is the reason LTFS became
the common ground of the industry, and it is the one property of LTFS
that these formats set out to keep.

The difficulty lies in its founding idea, which is to make a tape behave
like a disk: mount it, see a filesystem, drag files about. Tape is not a
disk, and the pretense has costs which are modest in daily use and severe
at archival timescales. The readability of the whole cartridge comes to
depend on a small mutable index that must be rewritten at every unmount,
so that an unclean stop leaves the index stale, and the failure of
fifteen terabytes concentrates in its most frequently rewritten region.
The filesystem illusion invites the habits of disk — small operations,
in-place edits — which a sequential medium answers with seek thrash and
mechanical wear. And the format itself offers no fixity, no parity, and
no determinism: no per-file cryptographic identity to verify against, no
means of repairing a damaged stretch of media, and no guarantee that two
writes of the same content produce comparable bytes. None of this is a
criticism of LTFS on its own terms. It was designed for interchange —
moving a tape between systems this year — and at that it is good. It was
not designed for recovering a damaged, unlabeled cartridge in forty
years, and it shows.

There is, then, a gap: an open, standardizable tape format that is honest
about being sequential — objects written once, streamed in order, never
edited — and that builds preservation in from the start: self-description,
per-file fixity, parity repair, deterministic bytes, and a specified
procedure for recovering a bare tape. Filling that gap, with a
specification anyone may implement, is what these formats are for.

## 3. Who this is for

Institutions that archive to tape come, broadly, in two kinds, and their
tools have historically been built for one kind or the other.

The first kind is the proper archive — the national library, the film
institute, the broadcast heritage collection. Its purpose is survival.
Access is occasional, scheduled, and patient; the mandate is measured in
centuries; the tools optimize for custody and fixity and treat retrieval
as an event.

The second kind is the production house, whose purpose is delivery. The
archive is where yesterday's work goes, and access to it is constant and
impatient — find the interview from 2019 and pull the twenty seconds
needed for tonight. Its tools optimize for speed and integration, and
preservation tends to be a checkbox.

These formats were built for the institution that is honestly both at
once: a working production operation whose output is also its permanent
heritage. This dual identity explains a combination that would otherwise
seem extravagant — preservation-grade discipline (fixity, parity,
self-description, bare-tape recovery) and production-grade access
(partial-file restore: computing exactly which stored bytes hold those
twenty seconds, and reading only them, even from an encrypted copy)
carried in the same format, rather than traded against each other.

The formats themselves are content-agnostic, though it is fair to say
what they were tuned for. They carry no compression of their own, because
the design assumed video and other already-compressed media, for which
compression buys nothing and would destroy the range arithmetic described
below (Section 8). A telescope's nightly output or a sequencing run fits
exactly as well as footage does. Data that is highly compressible — text,
logs, records — should be compressed before it reaches the archive, as a
step of ingest; it then archives in the same way as everything else. If
your data is large, fixed once written, and required both to survive and
to remain reachable, the shape fits.

## 4. The bets

The formats rest on a small number of deliberate bets, stated here in
priority order.

**Bet 1: plain readability outlives everything.** A stored archive object
is, before anything else, a valid POSIX tar archive. If every piece of
Remanence software vanished tomorrow, a stock `tar` command — the tool
that has shipped with every Unix system since the 1980s — would extract
every file, byte for byte. The cleverness (indexes, checksums, alignment)
travels in tar's own extension mechanism, invisible to a tool that does
not understand it, and losable without the loss of a single file byte.

**Bet 2: every object describes itself.** Each object carries its own
index — every file's name, size, cryptographic fingerprint, and exact
position — inside the object. The catalog database is a convenience and
an accelerator; it is never the only copy of the truth. If the database
burns down, the truth is rebuilt by reading the media.

**Bet 3: every tape describes itself.** The same principle, one level
down: each tape carries, on the tape, a map of its own structure and the
parity data needed to repair damage. A bare, unlabeled, partly damaged
tape, together with the specification, is by design a recoverable
situation.

**Bet 4: arithmetic beats scanning.** When someone needs thirty seconds
from the middle of a two-hour recording, the format computes exactly
which stored bytes to read — by plain arithmetic, with no decompression,
no index lookup, and no reading from the beginning. On tape, where every
unnecessary seek costs real time and real drive wear, this matters a
great deal.

**Bet 5: a proof is better than a hope.** Every file has a SHA-256
fingerprint recorded at ingest; every stored copy has a fingerprint of
its stored bytes. Verification is therefore always a mechanical
comparison, and the statement "the archive is intact" becomes something
one can prove rather than something one must hope.

## 5. A tour of one archive object

An RAO object ("Rem Archive Object") bundles a set of files — typically
the contents of one camera card or one submission — into a single unit.

**Bundling exists for a practical reason: tape performs well only when it
streams.** A tape drive is very good at one thing, which is writing large
amounts of data in one continuous motion. Ask it instead to handle a
million four-kilobyte files individually, and throughput collapses: the
drive stops and repositions constantly, and the catalog fills with a
million fingerprint records for cache fragments that nobody will ever
restore one at a time. Real collections contain exactly this — the
editing application's cache directory, the sweepings of a decommissioned
laptop, a project folder holding ten thousand thumbnails. The format
answers at two levels. First, the object itself: many files become one
large, tape-friendly unit, written as a single stream. Second, wrapping:
at ingest, rules may direct that a pathological subtree — that cache
directory, that thumbnail forest — be wrapped into a single member with
one fingerprint and its own internal listing. The wrapped subtree can
still be restored whole, or opened and picked from when someone genuinely
needs one file out of it; but it costs the tape one stream, and the
manifest one record, instead of ten thousand. Files that deserve
individual identity keep it; the rest are carried in bulk, deliberately.

**It is a tar file with discipline.** The object is a POSIX pax tar
stream — the modern, standardized flavor of tar. Two rules give it its
useful properties:

1. **Everything is deterministic.** Given the same input files, every
   conformant writer produces the identical byte stream: the same
   ordering, the same padding, the same metadata encoding. This is what
   makes fingerprints meaningful. Two independently produced copies of
   the same object match byte for byte, and any difference between them
   is damage, not noise.
2. **Every file's payload begins at a block boundary.** The format
   inserts precisely calculated padding — using tar's own comment
   mechanism, so that ordinary tools skip it — so that each file begins
   at a multiple of the chunk size. This one rule is what makes the
   arithmetic of Bet 4 possible.

**The manifest rides at the back.** The final member of every object is a
compact index — the manifest — listing every file with its path, size,
SHA-256, and block address. A future reader can list the contents of a
two-terabyte object by reading a few kilobytes at its end, and can
rebuild an entire catalog from manifests alone. And if even the manifest
is lost, the files remain ordinary tar entries; nothing about
self-description is load-bearing for the recovery of the bytes.

**Identity is layered.** Each file has its own fingerprint. The whole
plaintext stream has one logical fingerprint (the `plaintext_digest`),
which serves as the object's identity everywhere. Each stored copy has a
physical fingerprint of its bytes as stored (the `stored_digest`).
Storage systems scrub copies against the physical fingerprint without
needing to understand, or to decrypt, anything at all.

## 6. Encryption without a hostage situation

Some copies leave the building — an offsite tape, a rented object store —
and those copies are encrypted. Encrypting an archive, however,
introduces the most serious failure mode in this whole design space: an
archive that outlives its keys is lost as thoroughly as if the tapes had
burned. It is a curious feature of the situation that the lock, meant as
a protection, is the one part of the system capable of destroying
everything it protects. For that reason the encrypted representation is
designed around key custody first and cryptography second.

**The envelope.** An encrypted object is the identical plaintext stream —
manifest and all — sealed inside an authenticated envelope. Nothing about
the contents, names, sizes, or structure is visible without a key; only a
small plaintext header remains, carrying exactly what storage maintenance
and key recovery require.

**One fresh key per object, locked to public keys.** Each object is
encrypted under its own random data-encryption key, generated at sealing
time and never reused. That key is then wrapped — that is, itself
encrypted — separately to each of at least two recipients, and the
wrapped copies are stored in the object's own header. A recipient is
simply a keypair, and sealing requires only its public half.

Three consequences follow, each of them intended:

- **The sealing machine holds no secrets.** Public keys are all that a
  writer needs, so compromising the machine that writes the tapes yields
  nothing that decrypts them.
- **The object carries its own key, locked.** Recovery does not depend on
  any external key database. Whoever holds a recipient's private key can
  open the object with nothing but the object itself; a small standalone
  recovery tool exists precisely so that this remains possible on a bare
  machine, decades from now, with no surrounding infrastructure.
- **Two locks on every box, held apart.** Every object is sealed to at
  least two recipients. In the recommended arrangement, one is the
  operational key used for day-to-day restores; the other is a recovery
  key whose private half is generated on an offline machine, split into
  shares held by different people, and never present on any server. The
  loss of the operational key is then an inconvenience rather than a
  catastrophe.

**Range reads still work.** The encryption is chunked so that its blocks
correspond one-to-one with the plaintext's blocks. The same arithmetic
that locates thirty seconds of video in a plaintext object locates the
corresponding encrypted chunks, and the reader fetches and decrypts only
those. Encryption costs the overhead of confidentiality; it does not cost
the ability to restore a fragment from the middle of a large object.

It is worth stating plainly what each party is left with. An attacker
holding a stolen tape learns how many objects it contains and how large
they are, and nothing further — no file names, no counts, no structure.
The rightful owner, thirty years from now, needs the tape, the
specification, the recovery tool or any implementation of the
specification, and one recipient private key out of escrow; and that is
the whole list.

## 7. The tape knows itself

The companion REM-PARITY format governs how objects are laid out on a
tape and how that layout survives damage. Three structures do the work.

**Objects sit in plain sight.** Each object occupies its own tape file — a
contiguous run of fixed-size blocks between tape marks, navigable with
the standard positioning tools that have existed for decades. No parity
byte, no metadata, nothing foreign is ever interleaved inside an object.

**Parity rides in sidecars.** It is worth being precise about what parity
is for, because modern tape drives have earned a measure of trust: they
verify every block as they write it, and the probability that a drive
wrote wrong bytes while reporting success is very small. That is not
where archives get hurt. What actually happens — and this design is
informed by it having happened — is that a cartridge comes back years
later with a region it cannot read. Servo tracks disturbed by dust or by
a handling crease leave the drive unable to position over certain
stretches of tape at all; the data there was written perfectly and is
simply unreachable; and every file that lived in that span is gone,
although the rest of the cartridge reads flawlessly. Parity exists for
exactly this event. As objects stream to tape, the writer accumulates
Reed–Solomon parity — the same mathematical family that lets a scratched
CD play — over fixed groups of blocks, and writes it in separate tape
files at natural boundaries, physically elsewhere on the tape than the
blocks it protects, so that an unreadable stretch becomes a repair job
instead of a permanent loss. How much damage the scheme absorbs is a
tunable geometry rather than a fixed promise — at today's defaults,
hundreds of megabytes of contiguous unreadable tape per protected
region — and the geometry is recorded on the tape itself, so the numbers
can grow with cartridge capacities. The format assumes no particular LTO
generation and is intended to outlive them all.

**The bootstrap makes the tape self-starting.** At the beginning of the
tape — and again at checkpoints, and at the end — sits a bootstrap block:
the tape's identity, its parity scheme, a directory of the parity
sidecars, and a cryptographic digest of the tape's structural table of
contents. The recovery procedure for a bare, unlabeled, damaged tape is
to find any bootstrap copy, rebuild the map, verify it cryptographically,
repair damaged blocks from parity, and then read objects. Every step is
specified, and none requires a database, a catalog, or any prior
knowledge of the tape.

One further practice belongs in this section, though it is a
recommendation and not part of the formats. Because a latent bug in one
writer could corrupt every copy it ever wrote in the same way, we suggest
keeping one copy in a different format written by different code — format
diversity as the last line of defense. The specifications neither require
nor describe this; it is simply the same reasoning carried one step
further, since no single artifact, program, or database should be a
single point of failure for the archive.

## 8. What the formats refuse to do

It is as important to say what a design will not do as what it will,
since most designs are ruined by additions rather than omissions. The
notable refusals, and their reasons:

- **No compression.** The expected payload is already-compressed media,
  so there is little to gain; and whole-stream compression would destroy
  the arithmetic of Bet 4, because any byte's position would come to
  depend on decompressing everything before it. Compressible data belongs
  in the pipeline before the archive: compress at ingest, then archive
  the result (Section 3).
- **No re-keying in place.** One cannot swap an encrypted object's
  recipients by editing its header, because the header is
  cryptographically bound to the object. Changing the audience means
  re-sealing — a deliberate rule that makes key changes loud, auditable
  operations rather than silent edits.
- **No ownership preservation.** User and group IDs are meaningless
  numbers outside the system that assigned them, and carrying them would
  be false precision. Selected extended attributes are preserved, with
  restore rules designed so that a hostile tape cannot escalate
  privileges.
- **No devices, sockets, or FIFOs.** These carry no content — they are
  handles into a running kernel — and materializing them on restore is a
  hazard. A conformant reader rejects them.
- **No multi-object containers, no network protocol, no catalog format.**
  One object is one archive is one stored byte string. Everything above
  that level belongs to the systems that use the format, and is
  deliberately left unstandardized here.

## 9. Why you can trust an implementation — including ours

A specification is a promise, and promises about decades deserve
scrutiny. The specifications are written to be implemented independently,
and the project publishes the evidence that this independence is real
rather than aspirational:

- **Pinned test vectors.** A published archive of test inputs and
  expected outputs, byte-exact, covering the container, the manifest, the
  envelope, partial-range reads, and dozens of malformed-input cases with
  their required error classifications. The archive's SHA-256 is printed
  in the specifications, and its regeneration is deterministic.
- **An independent second implementation.** The vector archive is
  verified by a from-scratch Python program that shares no code with the
  reference implementation. It re-derives the cryptography from the
  specification's prose and must recover every byte exactly. If the
  specification were ambiguous, or the reference implementation quietly
  wrong, this is where the fact would show itself.
- **Determinism you can check.** Because writing is deterministic, anyone
  can rebuild a vector object from its described inputs and compare
  fingerprints. Sealing uses fresh randomness by design, so encrypted
  vectors pin the artifact and are verified in the open direction; a
  documented deterministic hook exists solely for regenerating them.
- **Adversarial testing.** The reference implementation treats every
  stored byte as untrusted input. Headers, manifests, key frames, and
  pax records are fuzzed, and the specifications enumerate the failure
  taxonomy an implementation must produce instead of crashing or
  guessing.

To the reader implementing their own: begin with the specification's test
vectors, make the positive vectors pass, and then make every negative
vector fail with the right error. That ordering is the fastest path to a
reader that behaves correctly on the day it matters.

## 10. What Remanence is — and is not

The reference implementation, Remanence, is deliberately a module rather
than a platform. It is a tape component with certain opinions built in —
the opinions this document has been describing — and it is driven through
an API. It discovers libraries, moves cartridges, writes and reads these
formats, keeps a rebuildable catalog, and stops when the hardware leaves
it uncertain of physical state.

What it deliberately does not contain is the orchestration layer: which
files to archive, when, in how many copies, to which pools, under what
retention rules, restored by whom. These are questions of policy, and
policy belongs to the system above — a media-asset manager, an
institutional workflow, an orchestrator of your own. Every deployment
already has opinions about policy, and a tape module that imposed its own
would either fight them or replace them. Remanence is intended to be the
tape component of a larger architecture that you control; and because the
formats are published, it is replaceable within that architecture, which
is precisely the point.

## 11. Where to go from here

- **The RAO Format Specification** — the normative definition of the
  archive object: container, manifest, encryption envelope, range
  addressing, test vectors, conformance.
- **The REM-PARITY Tape Format Specification** — the normative definition
  of the on-tape layout: bootstrap, parity, recovery procedures.
- **The reference implementation** — the Remanence project, an open Rust
  tape archival stack that produces and consumes these formats:
  <https://github.com/archivetechie/remanence>.

## 12. A note on maturity

The reference implementation was developed against a QuadStor virtual
tape library and field-tested on an HPE MSL3040 tape library with LTO-9
drives. That is real hardware, but it is one library family and one drive
generation — a young footprint for formats that speak of decades.

A closing piece of advice, offered in the same spirit as everything
above: this is version 1.0, and it has not yet had the years of operation
that alone earn an archive system real trust; it should be treated
accordingly. Whatever you deploy, operate a standing process in which
data is not merely written but restored and verified — routinely, from
the actual media, compared fingerprint against fingerprint with what was
ingested. The formats make this cheap to automate, since every file
carries its fingerprint and every copy carries its own, so that
"verified" is a mechanical comparison rather than a judgment. The
guarantees of an archive live in its restore drills, not in its write
logs. That has been true of every archive system ever built, and a 1.0
is exactly the moment to build the habit.

*Author: The ArchiveTech Project — <https://archivetech.org> —
specs@archivetech.org*
