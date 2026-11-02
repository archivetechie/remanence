<!-- code-anchor: specs/publication specs/in-progress tools/check_spec_versioning.py .github/workflows/ci.yml @ 311cc6c3 -->
# How the formats change

This page is for a reviewer of the Remanence format specifications, and for anyone who may
one day read a Remanence tape without Remanence. It explains in plain language how the
specifications may change, which changes are forbidden, and how each kind of change is
numbered. The page is informative. The policy itself is in each specification: in the "Status
of This Document" section of REM-OBJECT and REM-ENCRYPT, and in the "Versioning and Change
Policy" section of the REM-PARITY revision in preparation. Where this page and a
specification disagree, the specification is correct. The
[versioning register](versioning-register.md) lists every versioned value, one entry each.

## Where the specifications stand

There are three format specifications. REM-OBJECT defines an archived object, REM-ENCRYPT
defines the encrypted form of an object, and REM-PARITY defines how objects and their parity
are laid out on a tape. All three are **review drafts**: published for comment, and not yet
final. Comments close on 30 April 2027, and the documents **freeze** on 31 July 2027. The
promises on this page take effect at the freeze. Until then a review finding can still change
what goes on tape, so material written during the review window should be treated as
re-writable, as the [status page](status.md) explains.

Each specification has two copies in the repository. The copy under `specs/publication/` is
the published review text, and it is the one that governs. The copy under `specs/in-progress/`
is the **preparing copy**: the next revision, assembled while review items are resolved, and
not normative until it is published. For REM-PARITY the difference matters. The published
copy describes an earlier tape layout, while current software writes and reads only the layout
that the preparing copy describes. The status page explains
[what that means for tapes written today](status.md#the-rem-parity-publication-copy).

## Why a format needs a change policy

An archive format makes an unusual promise. Most software promises to work today. An archive
format promises that something written today can be read by someone else, on other equipment,
decades from now.

That promise raises a difficulty. Specifications are written by people, and people make
mistakes. A typo will be found, a sentence will turn out to be ambiguous, and someone will
need a feature the format lacks. If a specification may never change, the typo stays forever
and the format cannot grow. If it may change whenever someone needs it to, nobody can rely on
it, because the document might mean something different next year.

The way out is to say in advance which changes are allowed, which are forbidden, and how each
kind is labelled. A reader holding any revision then knows what can differ in any other. That
set of rules is the versioning policy.

## An analogy: the building code

A building code is revised every few years, yet a house built legally in 1990 does not become
illegal when the 2020 edition appears. It was built under the rules in force at the time, and
new editions bind new construction, not old houses.

The formats work the same way. A tape is like a house: once it is written, nothing published
later can make it wrong. A specification is like the building code: it says what a correctly
built tape looks like. A reader, the software that reads tapes back, is like a building
inspector that examines a tape and decides what it holds. The policy is a set of promises
among these three.

## The three promises

The first promise is that a valid tape stays valid. No later revision of a specification will
declare an existing tape wrong. Without this promise the formats would be no use for an
archive.

The second promise is that newer readers read older tapes. Software built from any later
revision reads every tape written under an earlier one, correctly.

The third promise is that older readers are never misled by newer tapes. It needs an example.
Suppose that in 2032 the format gains a small optional feature, and a tape is written with it.
In 2035 someone reads that tape with software built in 2030, which has never heard of the
feature. Three things could happen:

1. The old reader reads the tape correctly anyway and skips the part it does not understand.
   This is called reading through. The formats are built to allow it: most new features go in
   places that a reader is required to ignore when it does not recognise them.
2. The old reader cannot use the tape, and says so clearly: the tape uses feature number 2,
   which this software does not implement. The operator knows what is wrong and what to fetch.
3. The old reader is confused. It misreads the tape, or it reports a healthy tape as blank or
   damaged.

The policy forbids the third outcome. For an archive, a false report of damage does real harm
because people believe it: a good tape is set aside or discarded because an old reader failed
to understand it. Most of the rules about which changes are allowed exist to prevent this
outcome.

## Three kinds of change

Every revision of a specification is classified by three questions, asked in order. The first
answer that applies decides the kind of change and how it is numbered.

1. Does an existing tape or object become invalid, or does the meaning of anything already
   written change? Then the change is a new major version.
2. Would a reader built from an earlier revision lose the ability to recognise a newer tape
   and refuse it cleanly? Then it is also a new major version.
3. Does anything an implementation must do change, or is a new value assigned in one of the
   specification's registries (the tables of assigned values described below)? Then it is a
   minor revision, and it is allowed only under the conditions below.

Any other change is an erratum.

### Errata: correcting the text

An erratum corrects the document, not the format: a typo, a broken cross-reference, or an
ambiguous sentence rewritten so that it can only be read the way conformant implementations
already behave. After an erratum, no tape or object changes status and no software has to
change. If a correction would make any implementation behave differently, it is not an
erratum, however small the edit. Errata change the third number of the version, so 1.0.0
becomes 1.0.1. In the analogy, an erratum is a reprint of the code book that fixes a misprint.

The policy names one exception openly. A correction to the change-policy text itself is
published as an erratum and flagged in the revision history as a policy correction. Without
this exception, a defect in the policy's own wording could never be fixed. The flag makes
every such correction visible to anyone who reads the history.

### Minor revisions: adding with care

A minor revision may add to the format, for example a new optional field or a newly approved
encryption algorithm. It changes the middle number, so 1.0.x becomes 1.1.0. The policy allows
a minor revision only if all three of these conditions hold:

1. every tape written under an earlier revision of the same major version stays valid;
2. a reader built from an earlier revision still reads correctly every tape that uses only the
   features it knows, even a tape written yesterday by new software;
3. when that reader meets a feature it does not implement, it recognises the tape and refuses
   it with a typed error naming the unimplemented value, and it never misreads the tape or
   mistakes it for damage.

A minor revision whose new value older readers must refuse says so in its revision-history
entry, so that nobody is surprised by it.

How can old software name a feature that did not exist when it was written? It does not need
to know the feature, only where to look. Each format fixes in advance a few fields, at known
positions, that hold feature numbers. An old reader can always read such a field, even when
the value is new to it, and report the number: suite 2, not implemented here. Each
specification also keeps a **registry** for these fields: a permanent table of every value
ever assigned, with its meaning and the revision that defined it. The software reports the
number, and a person looks it up in the registry of a later revision.

Two cases show how the conditions decide changes that look alike.

A new encryption algorithm is allowed as a minor revision. REM-ENCRYPT records which
key-derivation and encryption algorithms an object uses in a one-byte field, `suite_id`, and
its registry has one value today, `0x01`. Suppose a later revision approves a second suite.
Older software cannot open objects sealed with it, because nobody can read through
cryptography they do not implement. The change is allowed anyway, because of how the failure
happens. The suite number is stored unencrypted in the object's header, and REM-ENCRYPT
requires a reader that meets an unassigned value to refuse the object with the error
`InvalidSuite`. Nothing is misread, nothing is mistaken for damage, and the operator knows
what software to obtain. All three conditions hold, and REM-ENCRYPT classifies such an
assignment as a minor revision.

A new block size is not allowed as a minor revision. A tape is written in fixed-size blocks,
and REM-PARITY allows three sizes: 256 KiB, 512 KiB and 1 MiB. A reader that finds a tape
without its catalog learns the block size by reading the
[bootstrap](reference-glossary.md#bootstrap), the block at the start of every tape that
describes it, at each of the three sizes in turn. If a revision added a fourth size, an old
reader would try its three sizes, find nothing, and report that the tape has no bootstrap. To
that reader, a tape at an unknown block size looks the same as a blank or destroyed one, so it
cannot refuse the tape by name. That is the third outcome. The change fails the second
question and could only be made in a new major version, which is why REM-PARITY declares the
three sizes frozen for the life of its current major version.

### Major versions: a new format beside the old one

A major version is for changes that would break a promise: changing what an existing field
means, removing something, or any change that would leave old readers misled instead of
refusing cleanly, such as the fourth block size. A major version does not modify the existing
format. It is a separate document defining a new format, and the old document stays in force
for every tape written under it. In the analogy, the code for timber houses is not amended to
cover steel towers. A separate steel code is written, and both stay on the shelf.

A new major format also announces itself differently in the stored bytes. Each format begins
with identifying bytes. A REM-PARITY bootstrap starts with fixed magic bytes followed by its
`schema_major` number. A plaintext object starts with a header that names its format
identifier, `rem-object-v1`. An encrypted object starts with the magic `REMO`, followed shortly
by its `format_version`. A new major version changes these: a new `schema_major` or magic, a
new format identifier such as `rem-object-v2`, or a new `format_version`. A reader of the old
major sees bytes it does not recognise and refuses them, so it is not misled.

One program may implement two major versions at once, in the way one inspector can be
certified under two codes. It then follows each major's rules for the tapes and objects
written under that major.

## Reading a version number

A specification's version always has three parts, major.minor.erratum:

- 1.0.0 is the first revision of the 1.0 line;
- 1.0.1 has the same rules as 1.0.0, in better text;
- 1.1.0 adds to the format under the three conditions;
- 2.0.0 would be the first revision of a different format, in a separate document beside
  version 1.

A revision published for review before it is frozen carries a suffix `-draft.N` on that core,
for example 1.0.0-draft.3. It names the revision it anticipates, and sorts before it. The
draft number rises separately in each document as review changes its text. Entry 18 of the
register gives the
[current version of each document](versioning-register.md#18-the-specification-documents).

A document's title carries its version line, 1.0, and its file name carries only the major
number, as in `rem-parity-1-specification.md`. File names therefore do not change with small
revisions. The full three-part version is in the status table at the head of each document.

<a id="the-test-vectors--evidence-that-never-moves"></a>

## The test vectors never change

Beside the specifications the project publishes **test vectors**: worked examples, with the
answers a correct implementation must give. The archive `remanence-test-vectors.tar` holds
small complete objects and tape images, some healthy and some damaged at recorded positions,
each with its expected result: what is recovered, which error is reported, which checksums
come out. An implementation conforms when it produces those answers from those inputs.

The archive is identified by its SHA-256 digest, and every specification that relies on it
quotes the digest. Anyone can check that they hold exactly the bytes the specifications mean.

The vectors have the strictest rule in the policy: a published vector archive is never
changed and never replaced. If a later revision could amend the examples, "conformant" would
change meaning with them, and every implementation tested against the old examples would hold
a claim that no longer said anything. A revision that needs new examples publishes an
additional archive, a supplement with its own name and digest, and cites it separately.
Changing the expected result of an existing entry needs at least a minor revision, and an
erratum may not alter any entry (REM-OBJECT §14).

When an implementation disagrees with a vector, the implementation is presumed wrong, because
the vectors are the text's published answers. If the text itself turns out to contradict a
vector, the text is the fixed point, and the change policy decides what kind of correction the
text or the vectors need. A vector is never edited to match software that failed it.

One consequence can look like an error. The published archive contains REM-PARITY tape images
whose bootstraps carry `schema_major` 1, at `schema_minor` values 2 and 3, because they were
generated for the earlier tape layout. They are correct examples of that layout, which the
published REM-PARITY copy describes, and current software rejects them because it reads only
`schema_major` 2. The vectors for the layout in the preparing copy exist as review-only
candidates. They are kept apart from the published archive until that revision is frozen and
publishes an archive of its own.

## A tape does not record which revision wrote it

Nothing on a tape says "written under revision 1.0.0". The specifications leave it out on
purpose, because a reader never needs it. The second promise means that any later reader reads
any older tape. The conditions on minor revisions mean that an older reader either reads a
newer tape correctly or refuses it, naming the value it lacks. In every case what matters is
which features a tape uses, and the tape announces those itself. Which edition of the text its
writer had is never needed. Houses are not stamped with the edition of the building code
either; what matters is the wiring found in the wall.

What a tape carries instead are a few small numbers in its stored bytes, each naming a feature
generation. The bootstrap carries `schema_major` and `schema_minor`. A plaintext object
carries its format identifier, its stream schema version and its manifest schema version. An
encrypted object carries its `format_version`, `suite_id` and `wrap_suite`. Each
specification's registry lists every value of these fields that has been assigned, with its
meaning and the revision that defined it.

The registry connects a stored number to the text without any arithmetic between the two.
There is no rule such as "revision 1.1 means value 4". A revision assigns a new value only when
it changes what can be stored, and it names itself in the registry row when it does. Most
revisions, every erratum among them, assign nothing.

## A found cartridge

The formats are designed around one scenario. A few cartridges turn up that were never
migrated with the rest. The system that wrote them was retired years ago, and nobody kept its
catalog.

1. Reader software, built from whatever revision is current by then, loads a tape and finds
   the bootstrap. The second promise says this works whichever revision wrote the tape.
2. The bootstrap describes the tape's structure, and its numbers name the feature generations
   in use. The reader either understands them all and reads the tape, or names what it lacks.
3. A person who wants to check the tape against the text uses the registry in the current
   revision, which names the revision that defined each value found on the tape.
4. That person retrieves the exact text of that revision. Each specification has a reserved
   concept DOI (a Digital Object Identifier, a permanent citation that resolves to an archived
   record), and the policy is to deposit every published revision under it with a DOI of its
   own. No revision has been deposited yet. Until the first deposits are published, a revision
   is identified by its version string and the repository commit that holds it.
5. The specifications are written so that a tape's arithmetic can be checked from the text
   alone, and the test vectors let that person check their own tools against known-good
   material. Once the deposits exist, none of these steps depends on the project still
   existing.

The specifications therefore have DOIs of their own, separate from the software's. The code is
useful, but the promises are carried by the documents, and each revision is to have its own
permanent record so that a citation such as "REM-PARITY 1.0.0" names one exact text.

## Three documents, and how they cite each other

There are three specifications because they describe three layers that can be used apart.
REM-OBJECT defines an object, which can be stored on any medium. REM-ENCRYPT defines the
encrypted form of an object. REM-PARITY defines a tape layout and treats the objects it holds
as opaque bytes. Each document has its own version and its own registries.

The documents cite each other in a form that survives revisions. A citation reads, in effect,
"version 1.0 or any later 1.x revision; this revision was published against 1.0.0". The first
half means that a small revision of one document never forces a revision of the others,
because the promises make any 1.x revision as good as another for the citing document. The
second half records which text sat beside which.

## Who keeps these promises

No standards body has reviewed or adopted the specifications. There is no ISO number and no
RFC. The same people wrote the specifications and the software, so the promises are the
project's own undertaking, and each specification says so. The words "final" and "frozen"
usually imply a committee somewhere, and here there is none.

What the project offers instead is that the promises can be checked. The test vectors are
published and identified by their digest, and the arithmetic in the specifications can be
re-derived from the text alone. A reader does not have to trust the project to test its
central claims.

The documents are also checked by machine, because prose kept consistent by hand across three
documents drifts. On every push, the repository's continuous integration runs
`tools/check_spec_versioning.py`. It fails the build if the change-policy text differs between
the three specifications, if a document's version strings disagree with each other, if the
vector-archive digest is quoted differently anywhere, if a revision history is out of order,
or if a section or appendix reference does not resolve. It also checks that each
specification's statement of what it covers reads the same in all three, except in the
published revisions that predate that statement, and that a version string recorded as
deposited is never reused for different bytes.

## In plain terms

A specification is a rule book, and a tape is something built under it. The project promises
that nothing built legally becomes illegal, that future inspectors will understand old
buildings, and that old inspectors will never condemn a new building they do not understand.
The most an old inspector will say is "this uses something newer than I know, and here is its
number". Reprints fix wording and change nothing else. Additions are allowed only when they
cannot mislead anyone. Anything larger is a new rule book, and the old one stays in force for
its own buildings. Buildings are not stamped with an edition of the rule book; each feature
they use is listed in a registry that names the edition that introduced it. These promises
take effect when the review drafts freeze on 31 July 2027.
