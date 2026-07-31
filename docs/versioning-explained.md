# How These Formats Change — and What Never Does

*A plain-language companion to the versioning and revision policy of the
Remanence format specifications. This document is informative. The policy
itself lives in each specification's "Status of This Document" section and
its Section 10 (or 8.1.1) versioning machinery; where this text and a
specification disagree, the specification wins.*

## Why this page exists

An archive format makes an unusual promise. Most software promises to work
today; an archive format promises that something written today will still be
readable by someone else, on other equipment, a very long time from now.

That promise immediately raises an awkward question. Specifications are
written by people, and people make mistakes. What happens when we find a
typo? What happens when a sentence turns out to be ambiguous? What happens
when someone needs a feature the format does not have? If the answer is
"the specification never changes, ever", then the first typo we find lives
forever, and the format can never grow. If the answer is "we change it when
we need to", then the promise is worthless, because nobody can rely on a
document that might mean something different next year.

The resolution is to say, precisely and in advance, **which kinds of change
are allowed, which are forbidden, and how each kind is labelled** — so that
a reader holding any revision of the document knows exactly what may differ
in any other revision. That set of rules is the versioning policy. This page
explains it slowly, with examples. A companion page,
[versioning-register.md](versioning-register.md), then lists every versioned
component individually — one entry each, same questions answered every
time.

## An analogy to hold on to: the building code

A building code changes every few years. And yet a house built legally in
1990 does not become illegal when the 2020 edition of the code comes out.
Everyone understands this without thinking about it: the house was built
under the rules in force at the time, and it stays legal. New editions of
the code bind new construction, not old houses.

Our formats work the same way:

- A **tape** is like a house. Once written, it is built. Nothing we publish
  later can make it retroactively wrong.
- A **specification** is like the building code. It says what a correctly
  built tape looks like.
- A **reader** — the software that reads tapes back — is like a building
  inspector. It examines a tape and decides what it holds.

The whole policy is about keeping three promises among those three parties.
Everything else is detail.

## The three promises

**Promise one: a valid tape stays valid forever.** No future revision of any
specification will ever declare an existing tape wrong. The house stays
legal. This is the promise that makes the format archival at all.

**Promise two: newer readers read older tapes.** Software built from any
future revision of the specification will read every tape written under any
earlier revision, correctly. A 2040 inspector understands a 1990 house.

**Promise three: older readers are never fooled by newer tapes.** This one
is subtler, and it is where most format policies go wrong, so it deserves a
story.

Suppose in 2032 the format gains a small optional feature, and a tape is
written using it. In 2035 someone reads that tape with software built in
2030 — software that has never heard of the feature. What happens?

There are three possibilities, and only two are acceptable:

1. The old reader reads the tape correctly anyway, simply skipping over the
   part it does not understand. This is called *reading through*, and the
   format is deliberately built to make it possible: most new features are
   carried in places old readers are instructed to ignore.
2. The old reader cannot use the tape — but it says so clearly: "this tape
   uses feature number 2, which I do not implement; obtain newer software."
   The person standing at the drive knows exactly what is wrong and exactly
   what to do.
3. The old reader gets confused. It misreads the tape, or worse, reports
   that the tape is blank or damaged when it is actually fine.

The third outcome is the one we forbid absolutely. For an archive, a false
"this tape is damaged" is close to the worst thing software can say,
because people believe it. A perfectly good tape gets set aside, or
discarded, because an old inspector condemned a house it merely failed to
understand. Every rule in the policy about what changes are allowed comes
down to preventing outcome three.

## The three kinds of change

With the promises in place, every possible change to a specification falls
into one of three kinds. They are checked in order, and the first one that
applies decides how the change is labelled and published.

### Errata — fixing the text without changing the rules

An erratum corrects the document, not the format. A typo. A broken
cross-reference. A sentence that two people read two ways, rewritten so it
can only be read the one way that every existing implementation already
behaves.

The test for an erratum is strict: after the correction, **no tape changes
status and no software has to change**. If fixing a sentence would oblige
any implementation to behave differently, it is not an erratum, no matter
how small the edit looks.

Errata change the third number of the version: 1.0.0 becomes 1.0.1, then
1.0.2. In the building-code analogy, this is reprinting the code book
because page 40 had a misprint. Nobody's house is affected; no inspector
changes their checklist.

There is one special case, which we state openly because hiding it would be
worse. If the *policy text itself* — the very rules this page describes —
contains a mistake, the correction is published as an erratum and flagged in
the revision history as a **policy correction**. This happened once already:
the first published revision contained a sentence that accidentally forbade
the middle kind of change described next, contradicting the format's own
extension machinery two sections later. A policy that cannot correct its own
wording would be frozen together with its mistakes, so the escape hatch is
written down, narrow, and every use of it is labelled.

### Minor revisions — adding, carefully

A minor revision may add to the format. A new optional field. A new
recorded detail. A new algorithm choice for encryption. Minor revisions
change the middle number: 1.0.x becomes 1.1.0.

But "adding" is only safe under conditions, and the policy states three.
A minor revision is allowed only if:

1. **Every existing tape stays valid.** (Promise one, restated.)
2. **Older readers still read everything they could read before** — a tape
   that uses only the features an older reader knows about is read
   perfectly by it, even if that tape was written yesterday by brand-new
   software. New software must not sprinkle tapes with things that break
   old readers for no reason.
3. **When an older reader does meet a feature it lacks, it recognises the
   situation and refuses cleanly** — with an error that names the
   unimplemented value it found — rather than misreading anything or
   mistaking the tape for a damaged one. Outcome two from the story above,
   never outcome three.

A fair question at this point: how can old software name a feature that did
not exist when the software was written? It cannot, and it does not need
to. What the format fixes in advance is not the features but the *places
where feature numbers live* — a handful of fields at known positions, set
aside on day one. An old reader always knows where to look and what kind of
number it is looking at, even when it has never seen the particular value.
So it reports the number: "algorithm 2, not implemented here." The number
alone is enough, because every specification carries a registry — a
permanent table mapping each value ever assigned to its meaning and to the
document revision that defined it. The software names the number; the
registry, looked up later by a person, turns the number into a name. This
is also the real reason the block-size case (below) must be a major: a tape
at an unknown block size offers the old reader no field it can read at all,
so there is no number to report, and no clean refusal is possible.

Two real examples show how these conditions decide cases that look similar
on the surface but are opposites underneath.

**Example that is allowed: a new encryption algorithm.** REM-ENCRYPT keeps
a small table of approved algorithm combinations, each with a number.
Suppose a future revision approves a new one, number 2. Objects encrypted
with algorithm 2 cannot be opened by older software — you cannot "read
through" cryptography you do not implement; that is the entire point of
cryptography. So is this forbidden? No, because of how the failure happens.
The envelope of an encrypted object states its algorithm number in plain
sight, and the specification requires old readers to respond to an unknown
number with a specific, named error: this object uses algorithm 2, which
this software does not implement. Nothing is misread. Nothing is mistaken
for damage. The operator knows exactly what to fetch. All three conditions
hold, so approving a new algorithm is a minor revision — with one extra
duty: because old readers genuinely cannot open the new objects, the
revision must say so prominently in its revision history, so nobody is
surprised.

**Example that is forbidden as a minor: a new block size.** Tapes are
written in fixed-size blocks, and REM-PARITY permits exactly three sizes.
A reader that finds an unlabelled tape discovers its block size by simply
trying each of the three. Now suppose a revision added a fourth size and a
tape was written with it. An old reader tries its three sizes, finds
nothing it recognises, and reports: no data found. It does not say "this
tape uses a block size I do not know" — it *cannot* say that, because from
where it stands an unreadable tape and a tape at an unknown block size look
identical. That is outcome three: a healthy tape condemned as blank. The
third condition fails, so a new block size can never be a minor revision.
If it is ever truly needed, it must be the next kind of change.

### Major versions — a new format beside the old one

A major version is for changes that break the promises: changing what an
existing field means, removing something, or anything (like the fourth
block size) that would leave old readers fooled rather than cleanly
refusing.

The policy's answer to such changes is blunt: they do not modify the
existing format at all. They create a **new format that lives alongside the
old one**, with its own specification document, its own name on the wire,
and its own version numbering starting again at 1.0.0. The old
specification remains in force, permanently, for every tape written under
it. In the analogy: you do not amend the timber-frame building code to
cover steel high-rises; you write a steel code, and both codes stay on the
shelf, each governing its own buildings.

The "own name on the wire" part matters. Every tape and object begins with
identifying marks — magic bytes, format identifiers, a major number — and a
new major format uses new marks. So an old reader meeting a new-format tape
does not get subtly confused; it sees marks it does not recognise and says
so. Even at the moment of the biggest possible change, outcome three is
prevented.

One practical note: a single program is allowed to implement two majors at
once — the same binary can read both old-format and new-format tapes, the
way one inspector can be certified under two codes. Each format's rules
apply to the tapes written under it.

## Reading a version number

Every document's version has three parts, always: **major.minor.errata**.

- **1.0.0** — the first published revision of the 1.0 line.
- **1.0.1** — the same rules, better text. (This is where the documents
  stand today.)
- **1.1.0** — the format gained something, under the three conditions.
- **2.0.0** — a different format, in a separate document, coexisting with
  version 1 rather than replacing it.

The document's title and filename carry only the major line ("REM-PARITY
Format", file `rem-parity-1-specification.md`), so filenames never churn
with small revisions; the full three-part number lives in the document's
own header table.

## The tape does not know which document wrote it — on purpose

Here is the part that surprises people, and the reason it should not.

Nothing on a tape records "I was written under specification revision
1.0.1." We considered it, and rejected it, for a simple reason: **you never
need it to read the tape.** Promise two says any future reader reads any
older tape, and the conditions on minor revisions say any older reader
handles any newer tape correctly or refuses it cleanly by feature name. In
every case, what you need to know is *which features the tape uses* — and
those are announced by the tape itself — never *which edition of the text
its writer happened to have on the shelf*. Houses are not stamped with the
edition of the code they were built under either; what matters is the
wiring you actually find in the wall.

What the tape carries instead are a few small **wire numbers**, each
announcing a feature generation. For the tape layout there is a pair of
numbers in every bootstrap block (the tape's self-description block); for
objects there is a format identifier and a couple of small schema fields;
for encryption there are the algorithm numbers described earlier. Each
specification contains a **registry** for its numbers: a table listing every
value ever assigned, what it means on the wire, and which document revision
defined it.

The registry is the bridge between the two worlds. It is how a wire number
leads you to text, without the two ever being arithmetically linked. There
is no rule like "document 1.1 means wire value 4". A document revision
assigns a new wire value only when it actually changes what can appear on
tape, and it writes itself into the registry row when it does. Most
revisions of a document — every erratum, and any minor that only adjusts
obligations — assign nothing at all.

## The find-a-tape-in-2050 walkthrough

Put it all together with the scenario the formats are designed around: a
labelled-but-undocumented cartridge surfaces in 2050. The organisation that
wrote it is gone. What happens?

1. The reader software — built from whatever revision is current in
   2050 — loads the tape and finds a bootstrap block. Promise two says
   this works regardless of how old the tape is.
2. The bootstrap announces the tape's structure, and its wire numbers
   announce the feature generations in use. The reader either understands
   everything (and reads the tape), or names precisely what it lacks.
3. Suppose a human wants to go deeper — to check the tape against the
   actual specification text. The registry in the current document tells
   them which revision defined each wire value they are seeing.
4. Every published revision of every document is archived with a **DOI** —
   a Digital Object Identifier, the same permanent-citation system used by
   scientific journals. A DOI is like an ISBN that also promises the
   content is stored, unchanged, by an archival service. Each document has
   one DOI naming the document as a whole (all revisions) and one per
   revision. The 2050 reader resolves the DOI and holds the exact frozen
   text.
5. The specification, by design, contains everything needed to verify the
   tape's mathematics from the text alone, and the published test vectors
   let them check their tools against known-good material. None of this
   requires us to still exist.

Step 4 is also why the specifications are archived *separately* from the
software. The code is useful; but the promise is carried by the documents,
and they get their own permanent records so that citing "REM-PARITY 1.0.1"
means one exact, immutable text.

## Why three documents, and how they refer to each other

There are three specifications — the object format, the encryption profile,
and the tape/parity format — because they answer at different rates and can
fail independently. Each carries its own version and its own registries.

They do cite each other, and the citations are written to survive
revisions. Each reference says, in effect: "version 1.0 *or any later 1.x
revision* — interchangeable for our purposes, because of the promises
above — and for the record, this text was published against revision
1.0.1." The first half means a small revision in one document never forces
paper churn in the other two. The second half preserves the historical
fact of exactly which text sat beside which.

## Who keeps these promises

No standards body stands behind any of this. There is no ISO number and no
RFC; the same people wrote the specifications and the software, and the
promises above are our own undertaking. We say that plainly in every
document, because the words "final" and "frozen" usually imply a committee
somewhere, and here there is none.

What we offer instead of authority is checkability. The promises are narrow
enough to test: the conformance vectors are published and pinned by
checksum; the arithmetic in the specifications can be re-derived from the
text alone; and the repository runs an automated consistency check on every
change to the documents themselves — the policy text must remain identical
across all three, version numbers must agree with themselves, revision
histories must stay in order, and every registry reference must resolve.
Several of those rules exist because we made exactly those mistakes while
writing the policy, and the check now makes them impossible to repeat
silently.

## In plain terms

The specification is a rule book; a tape is a thing built under it. We
promise that nothing built legally ever becomes illegal, that future
inspectors will always understand old buildings, and that old inspectors
will never condemn a new building they merely fail to understand — the
worst they will ever say is "this uses something newer than I am; here is
its name." Small reprints fix words and change nothing else. Additions are
allowed only when they cannot fool anyone. Anything bigger becomes a new
rule book with a new name, and the old one stays in force for its own
buildings forever. The buildings are never stamped with a rule-book
edition; instead, every feature on them is listed in a registry that names
the edition which introduced it, and every edition is permanently on file
where anyone can retrieve it.
