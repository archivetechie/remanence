# The Remanence Formats, Explained

*A companion to the RAO Format Specification and the REM-PARITY Tape Format
Specification. This document is informative: it explains what the formats do
and why they are shaped the way they are, in plain language. The
specifications remain the only normative documents — where this text and a
specification disagree, the specification wins.*

---

## 1. The problem

Suppose you are responsible for a large media archive — decades of footage,
still growing every day — and your job is to make sure that in fifty
years it still exists, and can still be read.

Magnetic tape is still the medium of choice for this: it is cheap per
terabyte, it sits on a shelf using no power, and its raw bit rot rates are
excellent. But almost nobody loses an archive because the magnetic particles
faded. Archives die in more mundane ways:

- **The software dies.** The backup application that wrote the tapes is
  discontinued, the company folds, the license server goes dark, and the
  tapes are full of a proprietary format nobody can read.
- **The database dies.** The tapes are fine, but the catalog that knew what
  was on them — which file, on which tape, at which position — was on a
  server that failed, and the tapes are now a warehouse of unlabeled boxes.
- **The people leave.** The one person who understood the system retires,
  and the institution is left operating a black box it dares not touch.
- **A bug corrupts everything the same way.** All the copies were written by
  the same program, so a single flaw damaged all of them identically.

Every design decision in these formats traces back to one of those failure
modes. The guiding question is never "how do we write tapes efficiently?" —
it is *"what will the person who finds these tapes in thirty years need, if
they have nothing but the tapes and a copy of the specification?"*

## 2. The landscape, and the gap

If you go shopping for tape archival software today, you find two shelves.

**The enterprise shelf** — IBM, Oracle, Quantum, and the media-asset-management
vendors built above them — works, at a price that makes it a board-level
decision: per-terabyte licensing, per-drive licensing, mandatory support
contracts, and professional services to change a setting. Worse than the
price is the dependency it buys: the on-tape format is the vendor's, the
catalog is the vendor's, and the day the contract lapses or the product is
discontinued, your archive's readability has an expiry date. For an
institution thinking in decades, the licensing model itself becomes an
archival risk.

**The open-source shelf** holds two things. Backup suites (Bacula and its
descendants) are honest tools shaped for a different job — rotating
short-lived backup sets, not preserving fixed masters — and their on-tape
formats are their own, readable by their own tools. And then there is LTFS,
which deserves a careful paragraph, because half of it is exactly right.

LTFS's achievement is real: an open, vendor-neutral, standardized format —
the tape carries its own index and can move between systems from different
vendors. That openness is why it became the common ground of the industry,
and it is the one property of LTFS these formats set out to keep.

The other half of LTFS is the problem: its founding idea is to make a tape
behave like a disk drive — mount it, see a filesystem, drag files around.
But tape is not a disk, and the illusion has costs that matter enormously
at archival timescales. The whole tape's readability hinges on a small,
mutable index that must be rewritten on every unmount — an unclean stop
leaves the index stale, and the failure mode of a fifteen-terabyte
cartridge concentrates in its most frequently rewritten region. The
filesystem illusion invites disk-shaped behavior — in-place edits, small
random operations — that a sequential medium answers with seek thrash and
mechanical wear. And the format itself carries no fixity (no per-file
cryptographic identity to verify against), no parity (a damaged stretch of
media is simply lost, discovered at restore time), and no determinism (two
writes of the same content produce different bytes, so independent copies
cannot be compared). LTFS was designed for *interchange* — moving a tape
between systems this year — and it is good at that. It was not designed
for *preservation* — recovering a damaged, unlabeled cartridge in forty
years — and it shows.

So there is a gap: an **open, standardizable tape format that is honest
about being sequential** — write-once objects, streamed in order, never
edited — and that builds preservation in: self-description, per-file
fixity, parity repair, deterministic bytes, and a specified bare-tape
recovery procedure. Filling that gap, with a specification anyone can
implement, is what these formats are for.

## 3. Who this is for

Organizations that archive to tape come in two kinds, and their tools have
historically been built for one or the other.

**Proper archives** — national libraries, film institutes, broadcast
heritage collections — exist to make data survive. Access is occasional,
scheduled, and patient; the mandate is measured in centuries. Their tools
optimize for custody and fixity, and treat retrieval as an event.

**Production houses** exist to deliver. The archive is where yesterday's
work goes, and access is constant and impatient — *find that interview
from 2019 and pull the twenty seconds we need for tonight*. Their tools
optimize for speed and integration, and treat preservation as a checkbox.

These formats are built for the organization that is honestly both at
once — a working production operation whose output is also its permanent
heritage. That dual identity is why preservation-grade discipline
(fixity, parity, self-description, bare-tape recovery) and
production-grade access (partial-file restore: computing exactly which
stored bytes hold those twenty seconds and reading only them, even from
an encrypted copy) are load-bearing in the *same* format, rather than
traded off against each other.

The formats themselves are content-agnostic, but it is honest to say what
they were tuned for: they carry no compression of their own, because the
design assumed video and other already-compressed media, where compression
would buy nothing and would destroy the range arithmetic (Section 8). A
telescope's nightly output or a sequencing run fits exactly as well. Data
that *is* highly compressible — text, logs, records — should be compressed
*before* it reaches the archive, as part of ingest; it then archives the
same way as everything else. If your data is large, fixed once written,
and must both survive and stay reachable, the shape fits.

## 4. The bets

The formats make a small number of deliberate bets, in priority order.

**Bet 1: plain readability outlives everything.** A stored archive object
is, first and foremost, a valid POSIX tar archive. If every piece of
Remanence software vanished tomorrow, a stock `tar` command — the same tool
that has shipped with every Unix system since the 1980s — extracts every
file, byte for byte. The clever parts (indexes, checksums, alignment) are
carried in tar's own extension mechanism, invisible to a tool that doesn't
understand them, and losable without losing a single file byte.

**Bet 2: every object describes itself.** Each object carries its own
index — every file's name, size, cryptographic fingerprint, and exact
position — inside the object. The catalog database is a convenience, an
accelerator. It is never the only copy of the truth. If the database burns
down, the truth is rebuilt by reading the media.

**Bet 3: every tape describes itself.** The same principle one level down:
each tape carries, on the tape, a map of its own structure and the parity
data needed to repair damage. A bare, unlabeled, partially damaged tape plus
the specification is a recoverable situation by design.

**Bet 4: arithmetic beats scanning.** When someone needs thirty seconds out
of the middle of a two-hour recording, the format can compute exactly which
stored bytes to read — with pencil-and-paper arithmetic, no decompression,
no index lookup, no reading from the beginning. On tape, where every
unnecessary seek costs real time and drive wear, this matters enormously.

**Bet 5: honesty about what you have.** Every file has a SHA-256
fingerprint recorded at ingest; every stored copy has a fingerprint of its
stored bytes. Verification is therefore always a mechanical comparison, and
"the archive is intact" is a claim you can prove, not a hope.

## 5. A tour of one archive object

An RAO object ("Rem Archive Object") bundles a set of files — typically the
contents of one camera card or one submission — into a single unit.

**Bundling exists for a practical reason: tape only performs well when it
streams.** A tape drive is very good at one thing — writing large amounts
of data in one continuous motion. Ask it to handle a million four-kilobyte
files individually and throughput collapses: the drive stops and
repositions constantly, and the catalog fills with a million fingerprint
records for cache fragments nobody will ever restore one at a time. Real collections are full of exactly that: an editing
application's cache directory, the sweepings of a decommissioned laptop, a
project folder with ten thousand thumbnails. The format answers at two
levels. First, the object itself: many files become one large,
tape-friendly unit, written as a single stream. Second, **wrapping**: at
ingest, rules can direct that a pathological subtree — that cache
directory, that thumbnail forest — be wrapped into a *single member* with
one fingerprint and its own internal listing. The wrapped subtree can
still be restored whole, or opened and picked from when someone really
does need one file out of it, but it costs the tape one stream and the
manifest one record instead of ten thousand. Files that deserve
individual identity keep it; the rest are carried in bulk, deliberately.

**It is a tar file with discipline.** The object is a POSIX pax tar stream,
which is the modern, standardized flavor of tar. Two rules give it its
power:

1. **Everything is deterministic.** Given the same input files, every
   conformant writer produces the *identical* byte stream — same ordering,
   same padding, same metadata encoding. This is what makes fingerprints
   meaningful: two independently produced copies of the same object match
   byte for byte, and any difference is damage, not noise.
2. **Every file's payload starts at a block boundary.** The format inserts
   precisely calculated padding (using tar's own comment mechanism, so
   ordinary tools skip it) so that each file begins at a multiple of the
   chunk size. This one rule is what makes the pencil-and-paper arithmetic
   of Bet 4 possible.

**The manifest rides in the back.** The final member of every object is a
compact index — the manifest — listing every file with its path, size,
SHA-256, and block address. A future reader lists a two-terabyte object's
contents by reading a few kilobytes at the end, and can rebuild a whole
catalog from manifests alone. If even the manifest is lost, the files are
still ordinary tar entries: nothing about self-description is load-bearing
for recovery of the bytes.

**Identity is layered.** Each file has its own fingerprint. The whole
plaintext stream has one logical fingerprint (the `plaintext_digest`), which
is the object's identity everywhere. Each stored copy has a physical
fingerprint of its bytes as stored (the `stored_digest`). Storage systems
scrub copies against the physical fingerprint without needing to understand
— or decrypt — anything.

## 6. Encryption without a hostage situation

Some copies leave the building — an offsite tape, a rented object store.
Those copies are encrypted. Encrypting an archive, however, introduces the
most serious failure mode in this whole design space: an archive that
outlives its keys is lost as thoroughly as if the tapes had burned. For
that reason the encrypted representation is designed around key custody
first and cryptography second.

**The envelope.** An encrypted object is the *identical* plaintext stream —
manifest and all — sealed inside an authenticated envelope. Nothing about
the contents, names, sizes, or structure is visible without a key; only a
small plaintext header remains, carrying exactly what storage maintenance
and key recovery need.

**One fresh key per object, locked to public keys.** Each object is
encrypted under its own random data-encryption key, generated at sealing
time and never reused. That key is then *wrapped* — encrypted — separately
to each of at least two **recipients**, and the wrapped copies are stored in
the object's own header. A recipient is just a keypair: sealing needs only
the *public* half.

Three consequences, each deliberate:

- **The sealing machine holds no secrets.** Public keys are all a writer
  needs, so compromising the machine that writes tapes yields nothing that
  decrypts them.
- **The object carries its own key — locked.** Recovery does not depend on
  any external key database. Whoever holds a recipient's private key can
  open the object with nothing but the object itself; a small standalone
  recovery tool exists precisely so this works on a bare machine, decades
  out, with no infrastructure.
- **Two locks on every box, held apart.** Every object is sealed to at
  least two recipients. In the intended arrangement, one is the operational
  key used for day-to-day restores; the other is a **recovery key** whose
  private half is generated on an offline machine, split into shares held
  by different people, and never present on any server. Losing the
  operational key is then an inconvenience rather than a catastrophe.

**Range reads still work.** The encryption is chunked so that its blocks
line up one-to-one with the plaintext's blocks. The same arithmetic that
finds thirty seconds of video in a plaintext object finds the corresponding
encrypted chunks — the reader fetches and decrypts only those. Encryption
costs confidentiality's overhead, but it does not cost the ability to
restore a fragment from the middle of a large object.

**What an attacker with a stolen tape sees:** how many objects it holds and
how large they are — and nothing else. Not file names, not counts, not
structure. What *you* need in thirty years: the tape, the specification, the
recovery tool (or any implementation of the spec), and one recipient private
key out of escrow. Nothing else.

## 7. The tape knows itself

The companion REM-PARITY format governs how objects are laid out on a tape
and how the layout survives damage. Three structures do the work:

**Objects sit in plain sight.** Each object occupies its own tape file — a
contiguous run of fixed-size blocks between tape marks, navigable with the
standard positioning tools that have existed for decades. No parity byte,
no metadata, nothing foreign is ever interleaved inside an object.

**Parity rides in sidecars.** It is worth being precise about what parity
is *for*, because modern tape drives have earned some trust: they verify
every block as they write it, so the odds that a drive wrote wrong bytes
and told you all was well are minuscule. That is not where archives get
hurt. What actually happens — and this design is informed by it happening —
is that a cartridge comes back years later with a *region* it cannot read:
servo tracks disturbed by dust or a handling crease leave the drive unable
to position over certain stretches of tape at all. The data there was
written perfectly; it is simply unreachable, and every file that lived in
that span is gone — even though the other 99% of the cartridge reads
flawlessly. Parity exists for exactly that event: with per-region
redundancy on the tape, an unreadable stretch becomes a repair job instead
of a permanent loss.

So, as objects stream to tape, the writer accumulates Reed–Solomon
parity — the same mathematical family that lets a scratched CD play — over
fixed groups of blocks, and writes it in *separate* tape files at natural
boundaries, physically elsewhere on the tape than the blocks it protects.
How much damage it absorbs is a
tunable geometry, not a fixed promise — at today's defaults, hundreds of
megabytes of *contiguous* unreadable tape per protected region, repairable
by mathematics alone — and the geometry is recorded on the tape itself, so
the numbers can grow with cartridge capacities. The format is designed to
outlive any particular LTO generation; nothing in it assumes one.

**The bootstrap makes the tape self-starting.** At the beginning of the
tape — and again at checkpoints and at the end — sits a bootstrap block: the
tape's identity, its parity scheme, a directory of the parity sidecars, and
a cryptographic digest of the tape's structural table of contents. The
recovery procedure for a bare, unlabeled, damaged tape is: find any
bootstrap copy, rebuild the map, verify it cryptographically, repair damaged
blocks from parity, then read objects. Every step is specified; none
requires a database, a catalog, or any prior knowledge of the tape.

One further practice belongs here, though it is a **recommendation, not
part of the formats**: because a latent bug in one writer could corrupt
every copy it ever wrote in the same way, we suggest keeping one copy in a
*different* format written by *different* code — format diversity as the
last line of defense. Nothing in the specifications requires or describes
this; it is simply the same philosophy taken one step further — no single
artifact, program, or database should be a single point of failure for the
archive.

## 8. What the formats refuse to do

Restraint is a feature. Some notable refusals, and their reasons:

- **No compression.** The expected payload is already-compressed media,
  so there is little to gain — and whole-stream compression would destroy
  the closed-form arithmetic of Bet 4, because any byte's position would
  depend on decompressing everything before it. Compressible data belongs
  in the pipeline *before* the archive: compress at ingest, then archive
  the result (Section 3).
- **No re-keying in place.** You cannot swap an encrypted object's
  recipients by editing its header, because the header is
  cryptographically bound to the object. Changing the audience means
  re-sealing — a deliberate rule that makes key changes loud, auditable
  operations rather than silent edits.
- **No ownership preservation.** User and group IDs are meaningless numbers
  outside the system that assigned them; carrying them would be false
  precision. (Selected extended attributes *are* preserved, with restore
  rules designed so a hostile tape cannot escalate privileges.)
- **No devices, sockets, or FIFOs.** They carry no content — they are
  handles into a running kernel — and materializing them on restore is a
  hazard. A conformant reader rejects them.
- **No multi-object containers, no network protocol, no catalog format.**
  One object is one archive is one byte string. Everything above that
  belongs to the systems that *use* the format, and is deliberately not
  standardized here.

## 9. Why you can trust an implementation — including ours

The specifications are written to be implemented independently, and the
project ships the evidence that this is real:

- **Pinned test vectors.** A published archive of test inputs and expected
  outputs, byte-exact, covering the container, the manifest, the envelope,
  partial-range reads, and dozens of malformed-input cases with their
  required error classifications. The archive's SHA-256 is printed in the
  specifications; regeneration is deterministic.
- **An independent second implementation.** The vector archive is verified
  by a from-scratch Python program that shares no code with the reference
  implementation — it re-derives the cryptography from the specification's
  prose and must recover every byte exactly. If the spec were ambiguous or
  the reference implementation quietly wrong, this is where it would show.
- **Determinism you can check.** Because writing is deterministic, anyone
  can rebuild a vector object from its described inputs and compare
  fingerprints. (Sealing uses fresh randomness by design, so encrypted
  vectors pin the artifact and are verified in the open direction, plus a
  documented deterministic hook exists solely for regenerating them.)
- **Adversarial testing.** The reference implementation treats every stored
  byte as untrusted input — headers, manifests, key frames, and pax records
  are fuzzed, and the specifications enumerate the failure taxonomy an
  implementation must produce instead of crashing or guessing.

If you are implementing a reader: start from the specification's test
vectors, make the positive vectors pass, then make every negative vector
fail with the *right* error. That ordering is the fastest path to a reader
that behaves correctly on the day it matters.

## 10. What Remanence is — and is not

The reference implementation, Remanence, is deliberately a **module, not a
platform**. It is a tape component with opinions baked in — the opinions
this document has been describing — exposed through an API. It discovers
libraries, moves cartridges, writes and reads these formats, keeps a
rebuildable catalog, and stops when hardware state is uncertain.

What it deliberately does not contain is the **orchestration layer**: which
files to archive, when, how many copies, to which pools, under what
retention rules, restored by whom. Those are policy, and policy belongs to
the system above — a media-asset manager, an institutional workflow, a
homegrown orchestrator, a script. Every deployment already has opinions
about policy; a tape module that imposed its own would either fight them
or replace them. Remanence is designed to be the tape component of a
larger architecture you control — and because the formats are published,
it is replaceable within that architecture, which is precisely the point.

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

The reference implementation was developed against a QuadStor virtual tape
library and field-tested on an HPE MSL3040 tape library with LTO-9 drives.
That is real hardware, but it is one library family and one drive
generation — a young footprint for formats that talk about decades.

So a closing piece of advice, offered in the same spirit as everything
above: this is version 1.0, and it has not yet had the years of operation
that earn an archive system real trust. Treat it accordingly. Whatever you
deploy, operate a standing process in which
data is not merely written but *restored and verified* — routinely, from
the actual media, compared fingerprint-for-fingerprint against what was
ingested. The formats make this cheap to automate: every file carries its
fingerprint, every copy carries its own, and "verified" is a mechanical
comparison rather than a judgment call. An archive's guarantees live in
its restore drills, not in its write logs — that is true of every archive
system ever built, and a 1.0 is exactly the moment to build the habit.

*Author: The ArchiveTech Project — <https://archivetech.org> —
specs@archivetech.org*
