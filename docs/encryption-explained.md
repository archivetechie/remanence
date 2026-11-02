<!-- code-anchor: crates/remanence-aead/src/seal.rs crates/remanence-aead/src/header.rs crates/remanence-cli/src/pool_ops.rs specs/in-progress/rem-encrypt-1-specification.md specs/publication/rem-implementation-guide.md @ 311cc6c3 -->
# Encryption explained

This page is for an operator or a reviewer who wants to know why Remanence encrypts objects in
software, with REM-ENCRYPT, instead of using the encryption built into LTO drives, and what an
operator still has to manage. REM-ENCRYPT seals an object before it reaches any storage
medium. Sealing means encrypting the object and wrapping it in an envelope that also carries
the object's key, itself encrypted for each recipient. LTO drives have been able to encrypt in
hardware since LTO-4 ([LTO Program FAQ](https://www.lto.org/faqs-about-lto/)).

The project's website has a [shorter version](https://archivetech.org/remanence/drive-encryption)
of this comparison. This page gives the detail behind it, and guidance on key custody, which
is the part an operator has to supply. The [quickstart](guide-quickstart.md#the-encrypted-variant)
shows the commands: making keys, building an encrypted object, and opening it again.

## 1. The short answer

Both approaches use standard authenticated encryption. The difference is where the boundary
between plaintext and ciphertext is drawn, and what that boundary costs in the work an archive
repeats for as long as it holds a collection.

Under drive encryption the ciphertext begins at the tape head. The ingest process, the staging
disk, the host and the cable all hold plaintext, and only the copies on tape are protected.
Under encryption in the object the ciphertext begins at ingest, so every copy on every medium
is the same protected byte string.

The practical consequence is that storing, replicating, repairing from parity (the recovery
data written beside the objects on tape) and scrubbing (reading stored copies on a schedule to
check them against their checksums) all work with no key present. Authenticated verification
of an object's contents, and any restore, need a key.

In plain terms: drive encryption locks the cartridge, and everything outside the cartridge is
unlocked. REM-ENCRYPT locks the data itself, so every copy is locked wherever it is stored,
and the people who look after the copies do not need the key.

## 2. What LTO drive encryption does

LTO drive encryption uses AES-256 in Galois/Counter Mode (AES256-GCM) in the drive, and the
LTO Program states that it "typically affects less than 1% of tape drive performance"
([LTO Program, data encryption](https://www.lto.org/encryption/)). Each encrypted record
carries a message authentication code (IBM LTO SCSI Reference, GA32-0928-08, section
2.1.150).

The key reaches the drive through the standard SCSI security commands, in the Set Data
Encryption page of SECURITY PROTOCOL OUT. The same page can ask the drive to clear the key when
the cartridge is unloaded, or when the host's reservation of the drive is lost or taken over
(IBM LTO SCSI Reference, section 6.8.3.1). An application can also attach key-associated data
(KAD) to the records it writes, a field that is recorded with each record and is commonly used
to name the key (section 6.8.3.2). KAD is optional and its content is chosen by the
application, so a cartridge does not necessarily carry a usable key identifier at all.

### 2.1 Key management is outside the format

The LTO Program states that "key management is not part of the LTO Ultrium drive
specification" ([LTO Program, data encryption](https://www.lto.org/encryption/)). Who holds
the key is decided by the product around the drive. IBM's reference names the three usual
arrangements (IBM LTO SCSI Reference, section 4.15.1):

| Arrangement | Who holds the key | Remarks |
| --- | --- | --- |
| Application-managed | The writing application | The application sends the key over SCSI at each mount. |
| Library-managed | The library, from a key manager or its own key store | Encryption is applied by library policy. |
| System-managed | A driver or operating-system layer on the host | The host configuration decides which tapes are encrypted. |

### 2.2 Library-local key stores

Under library management the keys come either from an external key manager over KMIP (the Key
Management Interoperability Protocol, an [OASIS standard](https://docs.oasis-open.org/kmip/kmip-spec/v2.1/kmip-spec-v2.1.html)
by which a device asks a key server for keys), or from a key store in the library itself.
HPE's MSL libraries offer the second as the Encryption Kit: keys are generated and kept on a
USB key server token that plugs into the library and is protected by a password. A token can be
backed up to a password-protected file and restored to another token, and keys from several
libraries' tokens can be merged (HPE Tape Autoloader and MSL Tape Libraries Encryption Kit
User Guide, part number 20-STG-AMEK-UG-ED12).

For a site with a single library this is the least work of the available arrangements. The
cost is that the key store becomes part of the vendor's system. The dependency is on the
product line rather than on one chassis, because tokens can be backed up and merged. Still,
decrypting a cartridge needs a compatible library of that make, a token holding the key, the
credentials for the token, and firmware that still supports the feature.

## 3. Where the two designs differ

| Property | Drive encryption | REM-ENCRYPT |
| --- | --- | --- |
| Cipher | AES256-GCM in the drive | ChaCha20-Poly1305. The object's key is wrapped for each recipient with HPKE (Hybrid Public Key Encryption, RFC 9180) using X-Wing, a hybrid of classical and post-quantum key exchange. |
| Ciphertext begins | At the tape head | At ingest, before any storage is touched |
| What is protected | The cartridge | The object, on every medium it is copied to |
| Key choice | Decided by the application or key manager | A fresh key for every seal, wrapped for each recipient |
| Check a copy without the key | Only through the drive's raw decryption mode, where the drive supports it | Yes. REM-ENCRYPT defines a Keyless Verifier role. |
| Repair from parity without the key | No | Yes |
| Authority held by upkeep jobs | They must be able to decrypt everything in their scope | None for storage, replication, parity repair and scrubbing; authenticated verification and any restore need a key |
| Migrate to new media | Decrypt, cross the host in the clear, encrypt again | Copy the bytes; no key is loaded |
| What the medium carries | The key-associated data the application chose, such as a key identifier | The format version, cipher suite, wrapping scheme, salt, recipients and the wrapped key |
| Needed to read a copy back | A compatible drive, the key, and a host that can send the SCSI security commands | The published specifications, a conforming implementation, and one matching recipient private key |
| Audit and revocation | Whatever the key manager provides | Nothing in the format; whatever the custody arrangement provides |
| Hardware compression | Kept, because the drive receives plaintext | Lost, because ciphertext does not compress |
| Host processor cost | None | Cryptographic work on every byte |
| Certified cryptographic module | Some drive models have held FIPS 140 validations | None |

### 3.1 Copies that are not on tape

Drive encryption protects the copies on tape. Copies on disk or elsewhere are outside its
scope, so their confidentiality, if it is needed, has to be arranged at another layer. That
layer can share key management and policy with the tape system, but it is a second mechanism,
and the archive's protection is that of the weaker of the two.

Repair has a further consequence. If a disk copy is lost and rebuilt from tape, the drive
decrypts the data and whatever protects the disk encrypts it again. Repairs of this kind are
routine, and each one produces plaintext on the host.

With encryption in the object the material is sealed once, and that stored byte string is
copied unchanged to every medium. A later reseal uses a fresh key and produces different bytes,
but all the copies made from one seal are identical.

### 3.2 The work an archive repeats

An object is written once. The operations that follow recur for as long as the collection is
held: reading copies back, scrubbing cartridges on a schedule, rebuilding damaged regions from
parity, making more copies, and eventually moving everything to newer media. This work is
automated because it is repetitive and very large, and automated work of this kind gets less
oversight than interactive work.

Under drive encryption all of it runs through the decryption path. The key store has to be
reachable, and keys are released for operations whose only purpose is to confirm that stored
data has not changed. Jobs that run unattended for years therefore hold the authority to
decrypt the collection.

With encryption in the object, the ciphertext is what gets stored, checksummed, replicated and
repaired. A copy can be checked without being opened, so custody and access become separate
permissions. A cartridge can be held by another institution that can confirm it is intact and
repair it from parity without being able to see what it contains.

A keyless check establishes that the stored bytes are consistent, not that they are authentic.
It shows that the stored bytes match a recorded digest. It does not prove who wrote them, and
against a holder who could change both the bytes and the digest it proves nothing unless the
reference digest is kept somewhere else.

### 3.3 Migration

A tape archive is copied forward to newer media on a schedule set by the availability of
drives, not by the life of the medium. LTO-8 and LTO-9 drives read and write only their own
generation and the one before it, and LTO-10 drives only LTO-10 media
([LTO Program, generation compatibility](https://www.lto.org/lto-generation-compatibility/)).

Under drive encryption the usual path is that the old drive decrypts, the data crosses the host
in the clear, and the new drive encrypts it again. Under encryption in the object, migration is
a copy of bytes: no key is loaded, no plaintext is produced, and the work can be done by
someone with no right to the contents.

Two qualifications apply. The SCSI security commands define a raw decryption mode in which the
drive returns encrypted blocks without decrypting them, so in principle a cartridge can be
copied without its key. A drive can also mark the blocks it writes as disabled for raw reads,
and support for the mode varies by drive (IBM LTO SCSI Reference, section 6.8). Re-keying at
each migration is also a policy choice, not a necessity.

## 4. What has to survive alongside the key

Both designs fail if every usable key is lost, although a REM-ENCRYPT envelope can be sealed to
several recipients, and any one matching private key opens it. What differs is how much else
has to survive for a key to be usable at all.

### 4.1 Where drive encryption is simple

Under drive encryption the key can reach the drive over the ordinary SCSI interface, in clear
text if the application chooses (IBM LTO SCSI Reference, section 4.15.2). A raw key kept in a
safe is then enough: any host that can send those commands can set it and read the tape. The
open-source [stenc](https://github.com/scsitape/stenc) utility does exactly this. In that
arrangement, retiring a key manager does not by itself strand the cartridges.

### 4.2 Where the difficulty is

In some library-managed or KMIP deployments the key is generated inside the key manager and
delivered to the drive already wrapped, so the raw value may never pass through the operator's
hands and there is nothing to put in the safe. KMIP itself can return keys wrapped or unwrapped,
and can register keys generated elsewhere (OASIS KMIP 2.1, the Get and Register operations).
Whether the raw key can be exported is a policy of the product and its configuration.

Some key managers write a wrapped copy of the data key onto the cartridge, so that the
cartridge carries its own key. IBM's key manager does this for IBM's 3592 enterprise drives,
as an "externally encrypted data key"; for LTO drives it writes only a data key identifier to
the tape ([IBM Security Key Lifecycle Manager 2.7 overview](https://www.ibm.com/support/pages/system/files/support/swg/swgdocs.nsf/0/b5d04c75522f38c78525805b0047f131/$FILE/oview.pdf)).
Where a wrapped copy is written, reading it back still needs the vendor's key pair and software
that understands its wrapping, which is a convention of the product, not of the format.

The question to ask of any arrangement is whether it lets the operator hold and escrow raw key
material. The answer depends on the product, not on LTO.

### 4.3 The envelope

REM-ENCRYPT puts the whole envelope in the object: the format version, the cipher suite, the
key-wrapping scheme, the salt, the recipient identifiers and the wrapped key itself. Recovery is
defined from the object's first stored block, using nothing but the object and one matching
private key. The unencrypted header gives what a reader needs to parse the envelope, and a
reader without a catalog reads through the object until it reaches the encrypted inner
manifest, which then gives the location of every member.

This creates an obligation of its own. Every recipient private key has to be kept for as long
as any object wrapped for it survives.

With drive encryption, then, recovery needs the key and, in most deployments, the vendor's
wrapping and the software that understands it. With encryption in the object, recovery needs
the key and the object.

## 5. What encryption in the object costs

The envelope format is the project's own work, and nobody outside the project has reviewed it.
Its building blocks are standards: HPKE ([RFC 9180](https://www.rfc-editor.org/info/rfc9180)),
the key-derivation function HKDF ([RFC 5869](https://www.rfc-editor.org/info/rfc5869)),
ChaCha20-Poly1305 ([RFC 8439](https://www.rfc-editor.org/info/rfc8439)), ML-KEM
([FIPS 203](https://doi.org/10.6028/NIST.FIPS.203)) and X25519
([RFC 7748](https://www.rfc-editor.org/info/rfc7748)). X-Wing, the key-encapsulation mechanism
(KEM) that combines ML-KEM-768 with X25519, is pinned at revision 10 of its Internet-Draft,
[draft-connolly-cfrg-xwing-kem-10](https://datatracker.ietf.org/doc/html/draft-connolly-cfrg-xwing-kem-10),
under a project-assigned identifier, and the envelope around all of it is the project's.

That draft expired on 3 September 2026. Its construction continues as the MLKEM768-X25519
instance of
[draft-irtf-cfrg-concrete-hybrid-kems](https://datatracker.ietf.org/doc/html/draft-irtf-cfrg-concrete-hybrid-kems-04),
which states that the two are identical. The [IANA HPKE registry](https://www.iana.org/assignments/hpke/hpke.xhtml)
lists the KEM as X-Wing under the identifier `0x647a`, and the HPKE working group's
[draft-ietf-hpke-pq](https://datatracker.ietf.org/doc/draft-ietf-hpke-pq/) asks for that
entry to be renamed MLKEM768-X25519. REM-ENCRYPT pins the construction at the byte level, with
a copy of the draft and known-answer files in the repository, so what it specifies does not
change with the draft's name or revision. Its open item RE-2 records what happens when either
document becomes an RFC.

REM-ENCRYPT is a review draft. No standards body has reviewed or adopted it, and nobody outside
the project has yet built an implementation from the specification text alone. What exists is
the published specification, pinned test vectors, and a verifier written separately within the
project.

Hardware compression is given up. The drive compresses the plaintext it receives, so
drive-encrypted data still benefits from compression. Object-encrypted data reaches the drive
as ciphertext, which does not compress; IBM's reference notes that LTO compression switches
itself off for random or encrypted data so that it does not expand
(IBM LTO SCSI Reference, section 2.1.148). REM-OBJECT has no compression of its own either. The
payloads it was designed for are already-compressed video and images, and compressing the
stream as a whole would break the byte-range addressing that makes partial restore possible.
Material that benefits from compression has to be compressed before it is archived.

Encryption costs processor time on the host: cryptographic work on every byte, beside the
digest the format already computes.

There is no certified module and no audit trail in the format. Some drive models have held FIPS
140 validations, for example NIST certificate
[#3282](https://csrc.nist.gov/projects/cryptographic-module-validation-program/certificate/3282)
for HPE's LTO-7 and LTO-8 encrypting drives (FIPS 140-2, now historical). REM-ENCRYPT has none.
A key manager can also enforce policy on each release of a key, and KMIP defines operations such
as Revoke. A private key in a safe has no such controls unless a custody arrangement provides
them (section 6).

An encrypted copy still reveals some facts about itself. Without a key anyone can see its
REM-ENCRYPT identity, its format and suites, its chunk size, its recipient identifiers and
labels, its salt, its frame lengths, its object identifier and its stored length, and from
these the exact plaintext size and chunk count can be worked out. Member names, member sizes,
the number of members, the manifest and the payload stay hidden (REM-ENCRYPT §12.5).

## 6. Key custody: a component you provide

REM-ENCRYPT specifies no key registry and no custody protocol (REM-ENCRYPT §1.5). Key custody
is a component the operator provides, in the same way as the catalog and the copy policy.
Remanence is a component, not a complete product, and custody is one of the things a larger
system has to supply.

There are two reasons for keeping custody out of Remanence. The main reason is that the
component designed to run without keys would otherwise become the one that holds them. A second
reason is that custody is not specific to tape: it applies to every encrypted copy on any
medium, so a tape component is the wrong owner for it.

Remanence does not ship custody software. The rest of this section describes the shape we
think that component should have.

### 6.1 The seam Remanence owns

Remanence needs a private key only when an object is opened. The command that opens an
encrypted object, such as `rem archive extract`, `rem restore` or `rem-recover`, is given a
recipient private-key file (a `REMP` file) with `--private-key`. It unwraps the object's key in
that process and stores nothing. The daemon's read services return stored bytes and never
decrypt them. Everything around that moment,
where the key rested beforehand, who was allowed to ask for it, and what was recorded
afterwards, belongs to the custody component and can be replaced without touching a tape.

### 6.2 The custody unit is 32 bytes

An X-Wing private key is kept as a 32-byte seed, which REM-ENCRYPT calls its secret custody
form (REM-ENCRYPT §2.3). The expanded key that is used to open a wrapped key is derived from the
seed when it is needed, and the REM Implementation and Operations Guide recommends storing only
the seed. Thirty-two bytes can be written on paper, stamped into metal, or divided with Shamir's
secret sharing ([Shamir, 1979](https://doi.org/10.1145/359168.359176)), a method of splitting a
secret into shares of which a chosen number are needed to rebuild it. Split that way, no single
person and no single safe can use the key alone. The protection covers only the split key. Any
recipient's private key opens the object on its own (section 6.3), so dual control over the
whole archive needs the same controls on every recipient key.

### 6.3 Recipients as roles

An envelope has between one and eight recipient slots, each for one recipient epoch (a
recipient key pair with a 16-byte identifier and an optional readable label; see
[recipient epoch](reference-glossary.md#recipient-epoch)). Any one matching private key opens
the object. Remanence's sealing commands require two to eight recipients, and its sealing
library accepts a single recipient only when the caller sets an explicit override, as the REM
Implementation and Operations Guide recommends. Key custody then becomes a few named roles
rather than a service:

| Role | Held how | Purpose |
| --- | --- | --- |
| Custody | Cold and split, never present on an archive system | Survives the loss of everything else |
| Operations | Warm, on a token or an encrypted volume | Authorised restores and drills |
| Partner | Held by a second institution | Survives the loss of both the safe and the staff |

### 6.4 The restore procedure

A restore should follow these steps. A named host receives an approved request. The operations
key is presented for the length of the job. The unwrap happens inside the restoring process, so
the object's key exists only in memory. Afterwards the key is withdrawn and the host rebuilt, and
a record is kept of who asked, what was opened and which recipient answered.

That record is the audit trail. It has to be kept deliberately, because no key server is
producing one.

### 6.5 The drill

The whole path should be exercised regularly, for example twice a year. An object is chosen at
random and opened with the cold key, under the procedure a real disaster would use, and its
plaintext digest (the checksum of the decrypted content) is compared with the catalog. A custody
arrangement that has never been tested is not known to work, and the drill tests the people and
the written procedure as well as the key.

### 6.6 Two limits

Changing recipients is expensive. The key frame, the part of the envelope that holds the wrapped
keys, is covered by the envelope's header hash, so a recipient set cannot be rewritten in place.
Changing it means resealing the object, which is a full read and a full write (REM-ENCRYPT
§12.8). The recipient set should therefore be decided when an object is sealed, not adjusted
later.

A hardware token that performs the unwrap without ever releasing the key would be the best
arrangement. The arrangement described here does not assume one: it protects the seed at rest
instead.

### 6.7 None of this is on the availability path

If the custody arrangement is unreachable, the archive still verifies, repairs, replicates and
migrates; only opening an encrypted copy waits. An escrowed seed, the published specification
and a conforming reader are enough to open an object with no other service running, and the
custody arrangement is designed to keep that property.

In plain terms: Remanence does not guard the keys. A separate and deliberately simple
arrangement of people, safes and written procedures does, and the archive keeps working without
it except when someone needs to read the contents.

## 7. When drive encryption is the better choice

Drive encryption is the better choice in several situations.

- The format is not yours to change. Most archives are written by software their operators did
  not write, into a container they do not control. Encryption in the object is not available to
  them, and the drive is the only control there is.
- The concern is a cartridge leaving the building, through theft, loss in transit or the disposal
  of retired media. That is the problem the feature was designed for, and it costs little speed
  and keeps hardware compression.
- A validated cryptographic module, an audit trail or revocation is required by regulation or
  contract.
- The material compresses well, and capacity matters more than the other differences.
- Tape is the only medium. The strongest arrangement is then application-managed encryption with
  the raw key escrowed by the operator rather than held in a product. It keeps compression, the
  option of a validated drive module, and a recovery path that needs only a drive and a host. Its
  key-custody work is much the same as for REM-ENCRYPT. What it does not give is keyless upkeep,
  or one ciphertext across all media.

## See also

- [REM-ENCRYPT specification](../specs/publication/rem-encrypt-1-specification.md)
- [REM-OBJECT core specification](../specs/publication/rem-object-core-1-specification.md)
- [REM Implementation and Operations Guide](../specs/publication/rem-implementation-guide.md),
  chapter 7, "Keys and secrets"
- [Architecture overview](architecture-overview.md)
- [Why Remanence?](why-remanence.md)
