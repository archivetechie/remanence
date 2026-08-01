# Encryption explained

REM-ENCRYPT seals a REM-OBJECT before it reaches any storage medium. LTO drives
have been able to encrypt in hardware since generation 4, which raises a fair
question: why encrypt in the object format at all?

This document answers it in full. The short version is on the
[website](https://archivetech.org/drive-encryption); what follows is the detail
behind it, together with guidance on the one part an operator has to supply
themselves.

---

## 1. The short answer

Both approaches use a published authenticated cipher and neither has a known
weakness. The difference is where the boundary between plaintext and ciphertext
is drawn, and what that boundary costs the work an archive repeats for decades.

Under drive encryption the ciphertext begins at the tape head. Everything before
it — the ingest process, the staging disk, the host, the cable — holds plaintext,
and only the tape copies are protected at all. Under encryption in the object the
ciphertext begins at ingest, so every copy on every medium is the same protected
byte string.

The consequence that matters operationally: storing, replicating, repairing from
parity and scrubbing stored bytes all proceed with no key present. Authenticated
verification of an object's contents, and any restore, require a key.

---

## 2. What LTO drive encryption does

Encryption entered the LTO format at generation 4. The drive holds an AES-256-GCM
engine on the data path. Data is compressed, then encrypted, then written, at
rated speed — the LTO Program puts the performance impact at under one per cent,
with a small per-record overhead.

The key reaches the drive over the ordinary SCSI security commands and is not
stored permanently in the drive. The encryption parameters can be set to clear
when a cartridge is unloaded, and in any case a reset or a later encryption
command replaces them. The drive never writes the key to the cartridge.

Encrypted records may carry an application-supplied field called Key Associated
Data (KAD), commonly used to name the key that was used. KAD descriptors are
optional and their content is set by the application, so a cartridge does not
necessarily carry a usable key identifier at all.

### 2.1 Key management is outside the format

The LTO Program is explicit: *"Key management is not part of the LTO Ultrium
drive specification."* Three arrangements are in common use.

| Arrangement | Who holds the key | Where it is applied |
| --- | --- | --- |
| Application-managed | The writing application, supplied at each mount | Per mount, over SCSI |
| Library-managed | The library, from a key manager or its own store | Per partition or barcode range, by policy |
| System-managed | A driver or operating-system layer | Per host configuration |

### 2.2 Library-local key stores

Under library management the keys come either from an external key manager over
KMIP, or from a key store belonging to the library itself. Many mid-range
libraries offer the second, so that no external server is needed. On HPE's MSL
libraries this is the Encryption Kit: keys are generated and held on a USB key
server token that plugs into the chassis, behind a PIN, and the library fetches
the right key when a cartridge is loaded.

For a site with a single library this is much the least work of any arrangement
available. The cost is that the key store becomes part of the vendor's system.
The dependency is on the product line rather than on one chassis — tokens can be
backed up to a password-protected file and restored onto another token, and keys
from several libraries can be combined — but decrypting a cartridge still means a
compatible library of that make, a token holding the key, the credentials for it,
and firmware that still supports the feature.

### 2.3 One consequence of policy-driven encryption

Where encryption is applied by library policy, the drives serving that partition
encrypt everything they write. An estate holding both encrypted and unencrypted
collections therefore divides into encrypting and non-encrypting drives, and a
drive failure removes the ability to write one of the two kinds of copy until it
is replaced. Nothing in the hardware requires this — encryption parameters are
set per mount and cleared at unload — but it is how library-managed policy is
usually deployed.

---

## 3. Where the two designs differ

| Property | Drive encryption | REM-ENCRYPT |
| --- | --- | --- |
| Cipher | AES-256-GCM in drive silicon | ChaCha20-Poly1305; key wrapped with HPKE and X-Wing |
| Ciphertext begins | At the tape head | At ingest, before any storage is touched |
| What is protected | The cartridge | The object, on every medium it is copied to |
| Key granularity | One key per encrypted data set; reuse decided by the manager | A fresh key per seal, wrapped to one to eight recipient keys |
| Verify a copy without the key | Possible in principle — a drive can return raw encrypted blocks — but we know of no archiving product that uses it | Yes; the specification defines the role |
| Repair from parity without the key | No | Yes |
| Authority held by upkeep jobs | Must be able to decrypt everything in their scope | None for storage, replication, parity repair and stored-byte scrubbing; authenticated verification and any restore need a key |
| Migrate to new media | Decrypt, cross the host in the clear, re-encrypt | Copy the bytes; no key is loaded |
| What the medium carries | A key identifier; some key managers also write a wrapped copy of the data key to the cartridge | Format version, cipher suite, wrapping mechanism, salt, recipients and the wrapped key |
| Needed to read a copy back | A compatible drive, the key, and a host that can send the standard SCSI security commands | Both published specifications, a conforming implementation, and one matching recipient private key |
| Audit and revocation | Per-release logging, revocation and dual control from the key manager | Nothing in the format; whatever the custody arrangement provides |
| Hardware compression | Retained — the drive compresses before it encrypts | Forfeited |
| Cost in CPU | None | Cryptographic work on every byte |
| Certified cryptographic module | Specific drive models and firmware have held FIPS 140 validations | None |

### 3.1 Copies that are not on tape

Drive encryption protects the copies that are on tape. Copies held on disk or
elsewhere are outside its scope, so their confidentiality, if it is required, has
to be arranged at another layer. That layer can share centralised key management
and policy with the tape system, but it is a second mechanism either way, and the
archive's effective protection is whatever the weaker of the two provides.

There is a second consequence during repair. If a disk copy is lost and has to be
rebuilt from tape, the data is decrypted by the drive and re-encrypted by whatever
protects the disk. Repairs of this kind are routine, and each one produces
plaintext on the host.

With encryption in the object the material is sealed once, and that stored byte
string is fanned out unchanged to every medium. A later reseal uses a fresh key
and produces different bytes, but the copies made from one seal are identical.

### 3.2 The work an archive repeats

An object is written once. The operations that follow recur for as long as the
collection is held: reading copies back, scrubbing cartridges on a schedule,
rebuilding damaged regions from parity, making additional copies, and eventually
moving everything to newer media. This work is automated because it is dull and
very large, and automated work of this kind is watched less closely than the paths
an operator sits in front of.

Under drive encryption all of it runs through the decryption path, so the key
store has to be reachable and the key released for operations whose only purpose
is to confirm that stored data has not changed. The standing authority to decrypt
the collection sits with automated jobs that run unattended for years.

With encryption in the object, the ciphertext is what gets stored, checksummed,
replicated and repaired. A copy can be checked without being opened, and custody
and access become separate permissions: a cartridge can be held by another
institution which can confirm it is structurally intact and repair it from parity
without being able to see what it contains.

A keyless check establishes structural consistency, not authenticity. It shows
that stored bytes match a recorded digest; it does not prove who wrote them, and
against a holder who might alter both the bytes and the digest it proves nothing
unless the reference digest is anchored elsewhere.

### 3.3 Migration

A tape archive is copied forward to newer media on a schedule set by drive
availability rather than by the life of the medium. Since LTO-8 a drive reads only
its own generation and the one before it, and LTO-10 reads only its own.

Under drive encryption the usual path is that the old drive decrypts, the data
crosses the host in the clear, and the new drive encrypts again. Under encryption
in the object, migration is a copy of bytes: no key is loaded, no plaintext is
produced, and the work can be done by someone with no entitlement to the contents.

Two qualifications. A drive can be told to return encrypted blocks without
decrypting them, so in principle a cartridge can be duplicated without the key;
vendors document this raw mode as an exposure to be aware of rather than a
supported path, some drives do not implement it, and whether it works between
generations depends on which algorithms the source and target drives support.
Re-keying at each migration is also a policy choice rather than a necessity.

---

## 4. What has to survive alongside the key

Both designs fail if every usable key is lost — though a REM envelope can be
sealed to several recipients, and any one matching private key opens it. What
differs is how much else has to survive in order for a key to be usable at all.

### 4.1 The drive's genuine strength

Under drive encryption the key reaches the drive over the ordinary SCSI interface,
so a raw key held in a safe is enough: any host able to send those commands can
set it and read the tape. The open-source
[stenc](https://github.com/scsitape/stenc) utility does exactly this. A key
manager that has been retired does not by itself strand the cartridges.

### 4.2 Where the difficulty actually is

In a library-managed or KMIP deployment the key is generated inside the key
manager and delivered to the drive already wrapped, so the raw value may never
pass through your hands and there is nothing to put in the safe.

Some key managers reduce that exposure by writing a wrapped copy of the data key
onto the cartridge — IBM calls it an externally encrypted data key — so the
cartridge carries its own key. Reading that copy back still needs the vendor's key
pair and software that understands their wrapping, which is a convention of the
product rather than of the format.

The question worth asking of any arrangement is therefore whether it lets you hold
and escrow raw key material yourself. The answer comes from the product, not from
LTO.

### 4.3 The envelope

REM-ENCRYPT puts the whole envelope in the object: format version, cipher suite,
key wrapping mechanism, salt, the recipient identifiers, and the wrapped key
itself. Recovery is defined from the first stored block using nothing but the
object and one matching private key. The public header gives the geometry needed
to parse the envelope, and a reader without a catalog opens sequentially until it
reaches the encrypted inner manifest, which then supplies the member locations.

This creates an obligation of its own: every recipient private key has to be kept
for as long as any object wrapped to it survives.

So the drive's answer is a key plus, in most deployments, the vendor's wrapping
and the software that understands it. The object's answer is a key plus the
object.

---

## 5. What encryption in the object costs

**The framing is our own work, and it has not yet been reviewed outside the
project.** The primitives are standardised — HPKE, HKDF, ChaCha20-Poly1305,
ML-KEM — but the wrap suite freezes an X-Wing Internet-Draft under a
project-assigned identifier, and the envelope around all of it is ours.
REM-ENCRYPT is a review draft: no standards body has reviewed or adopted it, and
no one outside the project has yet built an implementation from the specification
text alone. What exists is the published specification, pinned test vectors, and a
separately written verifier.

**Hardware compression is given up.** The drive compresses before it encrypts, so
drive-encrypted data still benefits from LTO's compression. Object-encrypted data
arrives at the drive as ciphertext, which the drive cannot usefully compress, and
REM-OBJECT contains no compression of its own — the payloads it was designed for
are already-compressed video and images, and compressing the stream as a whole
would destroy the byte-range addressing that makes partial restore possible.
Material that benefits from compression has to be compressed before it is
archived.

**Encryption costs processor time.** It is cryptographic work on every byte,
alongside the digest the format already requires.

**There is no certified module, and no audit trail in the format.** Particular
drive models and firmware configurations have held FIPS 140 validations;
REM-ENCRYPT has none. A key manager also gives per-release logging, revocation and
dual control, which a private key in a safe does not — though a custody component
can supply all three (§6).

**An encrypted copy still reveals some information about itself.** Visible without
a key are its REM-ENCRYPT identity, the format and cipher suites, its object
identifier, the chunk size, the recipient identifiers and labels, the salt, the
frame and stored lengths, and — derivable from those — the exact plaintext size
and chunk count. Member names, individual member sizes, member count, the manifest
and the payload remain hidden.

---

## 6. Key custody: a component you provide

REM-ENCRYPT specifies no key registry and no custody protocol, deliberately. Key
custody is a component an operator provides, in the same way the catalog and the
copy policy are. Remanence is a component, not a product, and this is one of the
things a larger system has to bring.

Putting custody inside Remanence would also be self-defeating: the component
designed to run without keys would become the one that holds them. Custody is not
tape-specific either — it applies to every encrypted copy, on any medium — so a
tape component is the wrong owner.

What follows is not a description of software Remanence ships. It is the shape we
think that component should have.

### 6.1 The seam Remanence owns

Remanence needs a key at the moment an object is opened. It is handed one, it
performs the unwrap, and it stores nothing. Everything around that — where the key
rested beforehand, who was allowed to ask for it, and what was written down
afterwards — belongs to the custody component and can be replaced without touching
a tape.

### 6.2 The custody unit is 32 bytes

An X-Wing private key has a canonical secret-at-rest form of a 32-byte seed; the
expanded decapsulation key is derived from it when needed and must not be stored
in its place. Thirty-two bytes can be written on paper, stamped into metal, or
divided with Shamir's scheme so that no single person and no single safe can open
the archive alone.

### 6.3 Recipients as roles

An envelope carries between one and eight recipient slots, each with a readable
label, and any one matching private key opens the object. Key custody then becomes
a few named roles rather than a service.

| Role | Held how | Purpose |
| --- | --- | --- |
| Custody | Cold, split, never present on an archive system | Survives the loss of everything else |
| Operations | Warm, on a token or encrypted volume | Authorised restores and drills |
| Partner | Held by a second institution | Survives losing both the safe and the staff |

The specification recommends at least two recipients.

### 6.4 The restore procedure

A named host, an approved request, the operations key presented for the duration
of the job, the unwrap performed inside the restoring process so the data key
exists only in memory, the key withdrawn and the host rebuilt afterwards, and a
record of who asked, what was opened and which recipient answered.

That record is the audit trail, and it has to be kept deliberately, because no key
server is generating one.

### 6.5 The drill

Twice a year the whole path is exercised on purpose. An object is chosen at random
and opened with the cold key, under the procedure a real disaster would use, and
its plaintext digest is checked against the catalog. A custody arrangement that
has never been tested is not yet known to work, and the drill tests the people and
the written procedure as much as the key.

### 6.6 Two limits

Rotation is not cheap. The key frame is bound into the envelope's header hash, so
a recipient set cannot be rewritten in place, and changing it means resealing the
object — a full read and write. The recipient set is a decision to make
deliberately when an object is sealed rather than one to adjust later.

A token that performs the unwrap without ever releasing the key is the arrangement
to prefer, but support for the post-quantum half of the construction in smartcards
is still thin, so for now the seed is protected at rest rather than held in
hardware that computes with it.

### 6.7 None of this is on the availability path

If the custody arrangement is unreachable the archive still verifies, repairs,
replicates and migrates; only opening an encrypted copy waits. An escrowed seed
with the published specification and a conforming reader opens an object with
nothing running at all, which is the property the whole arrangement exists to
keep.

---

## 7. When drive encryption is the better choice

- **Where the format is not yours to change.** Most archives are written by
  software they did not write, into a container they do not control. Encryption in
  the object is not available to them, and the drive is the only control there is.
- **Where the concern is a cartridge leaving the building** — theft, loss in
  transit, or the disposal of retired media. That is the problem the feature was
  designed for, and it costs nothing in speed or capacity.
- **Where a validated module, an audit trail or revocation is required** by
  regulation or contract.
- **Where the material compresses well** and capacity matters more than the rest.
- **Where tape is the only medium.** The strongest arrangement is
  application-managed encryption with the raw key escrowed by you rather than held
  in a product, which keeps the compression, the option of a validated drive
  module, and a recovery path that needs only a drive and a host. Its key-custody
  work is much the same; what it does not give is keyless upkeep or one ciphertext
  across media.

---

## See also

- [REM-ENCRYPT specification](../specs/publication/rem-encrypt-1-specification.md)
- [REM-OBJECT core specification](../specs/publication/rem-object-core-1-specification.md)
- [Architecture overview](architecture-overview.md)
- [Why Remanence](why-remanence.md)
