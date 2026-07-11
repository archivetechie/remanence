# Rem Archive Object (RAO) Format — Version 1.1

| | |
| --- | --- |
| Status | Draft for review |
| Version | 1.1 (minor; additive over 1.0) |
| Date | 2026-06-15 |
| Base | [RAO10], normative for everything not restated here |
| Stream version | `REMANENCE.schema_version = 1.1` (only when 1.1 metadata is present) |
| Adds | per-entry extended-attribute (xattr) metadata preservation |

## Status of This Document

This document is a draft specification, published for review. It is a
normative additive delta on RAO 1.0 [RAO10]: it specifies only what
version 1.1 adds. It freezes no earlier than RAO 1.0; after freeze, no
normative change is permitted other than errata that do not change the set
of valid objects.

## Abstract

This document specifies version 1.1 of the Rem Archive Object (RAO) format:
a backward-compatible minor revision of RAO 1.0 that adds preservation of
per-entry POSIX extended attributes (xattrs), stored in the reserved
`metadata_preservation_data` manifest container that RAO 1.0 defines. An
object that preserves no extended attributes is byte-identical to an RAO 1.0
object, and an RAO 1.0 reader reads a 1.1 object correctly, recovering every
file byte-for-byte.

## 1. Conventions and Terminology

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT",
"SHOULD", "SHOULD NOT", "RECOMMENDED", "NOT RECOMMENDED", "MAY", and
"OPTIONAL" in this document are to be interpreted as described in BCP 14
[RFC2119] [RFC8174] when, and only when, they appear in all capitals, as
shown here.

All conventions, conformance roles, definitions, and terms of RAO 1.0
Section 2 apply to this document unchanged. Terms used here — entry,
object, manifest, manifest profile, `REMANENCE.schema_version`,
`plaintext_digest`, `format_id`, `link_target`, hardlink primary — are
defined in [RAO10]. Native hardlinks, symbolic links, and empty directories
are part of RAO 1.0's entry set — not additions of 1.1 (see [RAO10]
Sections 4.3.4, 4.6, and 4.7.2).

## 2. Relationship to 1.0 and Version Gating

RAO 1.1 adds one capability: preservation of POSIX extended attributes per
entry, in the reserved `metadata_preservation_data` container that [RAO10]
Section 4.7.2 already defines (empty in 1.0). It is purely additive and
fully backward-compatible:

- A Writer that preserves no xattrs MUST emit
  `REMANENCE.schema_version = 1.0` and an empty
  `metadata_preservation_data` on every entry; the resulting object is
  byte-identical to an RAO 1.0 object, and every RAO 1.0 test vector and
  `plaintext_digest` is unchanged.
- A Writer that emits a non-empty `metadata_preservation_data` on any entry
  MUST set `REMANENCE.schema_version = 1.1`. The manifest CBOR
  `schema_version` integer stays 1: per [RAO10] Section 4.7.2 the reserved
  containers are the designated 1.x extension surface, and filling one does
  not bump the integer.
- A 1.0 reader reads a 1.1 object correctly. It gates on the major version
  ([RAO10] Section 4.5.2), then ignores unknown content in
  `metadata_preservation_data` ([RAO10] Section 4.7.2 obligation 3). It
  recovers every file byte-for-byte; it simply does not reapply the
  preserved xattrs. There is no misinterpretation and no rejection — which
  is why this revision is a minor version and not a new `format_id`
  ([RAO10] Section 10).

## 3. Extended-Attribute Preservation

### 3.1. Wire Format

An entry's `metadata_preservation_data` is a CBOR map (manifest profile,
[RAO10] Section 4.7.1) that MAY contain:

```text
"xattrs" : { <name> : <value>, ... }
```

- `<name>` — a text key (the manifest profile requires text-string keys,
  [RAO10] Section 4.7.1) holding the attribute's name. A Writer MUST reject
  (`InvalidInput`) an xattr name that is not valid UTF-8 [RFC3629]; no name
  escaping is defined. (In practice, xattr names are ASCII by namespace
  convention; disposition of files carrying non-UTF-8 names is an
  ingest-side policy above this format — Section 3.2.)
- `<value>` — a CBOR byte string holding the raw attribute value bytes,
  stored directly (the manifest is CBOR, so no textual encoding such as
  base64 is applied).
- Deterministic encoding ([RAO10] Section 4.7.1): the `xattrs` map's keys
  MUST be sorted in the ascending bytewise order of their deterministic
  encodings, with shortest-form lengths and no duplicate names.
- The container is keyed (`"xattrs"`) so a later minor revision can add
  other preserved-metadata types without restructuring; readers MUST ignore
  unknown keys within the container.
- An entry with no preserved xattrs MUST carry an empty
  `metadata_preservation_data` (preserving 1.0 byte-stability). A non-empty
  container is what marks a 1.1 object.
- Hardlink entries MUST carry an empty `metadata_preservation_data`. They
  carry no independent metadata fields; the shared file's restored xattrs
  come from the regular-file primary named by `link_target`.

Scope is xattrs only: ownership stays unpreserved ([RAO10] Section 4.3.1),
`mtime` is already covered ([RAO10] pax `mtime`, fractional seconds), and
mode bits beyond `executable`, as well as access-control lists (ACLs), are
out of scope for 1.1.

### 3.2. Selection Policy and Size (Informative)

Which xattrs are preserved is a policy of the ingesting system, above this
format: typically a built-in baseline of ephemeral attributes is always
dropped, the remainder is governed by a deny-list or allow-list stance with
a fail-safe default, and all drops are recorded by the ingesting system.
The format stores whatever surviving xattrs it is handed. An attribute too
large to sensibly live in the compact manifest (for example, a macOS
resource fork) is expected to be excluded from native preservation by the
ingesting system — for instance by archiving the file together with its
attributes in a container file of the ingesting system's choosing — rather
than stored through this mechanism; that is a storage/ingest decision, not
a format rule.

### 3.3. Read and Restore

A 1.1 Reader MUST surface an entry's preserved xattrs to its caller. On
restore, a Restoring Consumer ([RAO10] Sections 2.2, 12.10) SHOULD reapply
them through the platform's extended-attribute interface (e.g.
`setxattr()`), subject to the namespace restrictions of Section 6, and MUST
report any preserved xattr it could not or did not reapply (unsupported
namespace, platform limit, permission failure) rather than fail silently. A
1.0 reader ignores preserved xattrs (Section 2) and restores the file
without them.

## 4. Test Vectors (Additions to RAO 1.0 Section 13)

These vectors are distributed with this specification alongside the RAO 1.0
vectors; pinned outputs are **[pinned-at-generation]** exactly as in
[RAO10] Section 13.

- **RAO-TV-X1 — xattr round-trip:** the RAO-TV-P1 inputs ([RAO10]
  Section 13.2), with File 0 additionally carrying one xattr: name
  `user.color`, value the 3 bytes `72 65 64` (ASCII `red`). Expected:
  `REMANENCE.schema_version = 1.1`; File 0's `metadata_preservation_data` =
  `{"xattrs": {"user.color": h'726564'}}` in deterministic encoding; all
  other entries carry empty containers. Pinned outputs: the manifest CBOR
  bytes, `manifest_sha256`, and `plaintext_digest`. Restore reapplies the
  attribute byte-exact.
- **Byte-stability:** the same inputs with no xattrs → `schema_version`
  1.0, byte-identical to RAO-TV-P1.
- **1.0-reader compatibility:** a 1.0 reader reads RAO-TV-X1, recovering
  all files byte-exact and ignoring the preserved xattrs.
- **Determinism:** two builds of the same input and xattrs → identical
  bytes (deterministic `xattrs` map ordering).

## 5. Conformance

An implementation conforms to RAO 1.1 if it conforms to [RAO10] and
implements Section 3 of this document. This specification is a draft until
the Section 4 vectors exist in the published test-vector distribution and
pass — including the byte-stability and 1.0-reader-compatibility vectors —
under the same criteria as [RAO10] Section 14; RAO 1.1 freezes no earlier
than RAO 1.0.

## 6. Security Considerations

The security considerations of [RAO10] Section 12 apply unchanged. Version
1.1 adds one new surface: restore-time reapplication of extended
attributes.

Preserved xattr names and values are untrusted input from removable media,
like every other stored byte ([RAO10] Section 12.9). Extended attributes
can carry privilege: on Linux, `security.capability` grants file
capabilities, `security.*` and `trusted.*` participate in mandatory access
control and integrity mechanisms, and `system.posix_acl_access` encodes
ACLs. Blindly reapplying preserved xattrs at restore time can therefore
escalate privilege or alter a system's security state.

A Restoring Consumer MUST NOT reapply xattrs outside the `user.` namespace
unless explicitly configured to do so by an operator, MUST apply attributes
without following symbolic links (consistent with [RAO10] Section 12.10),
and MUST treat attribute values as opaque bytes (never interpreting or
executing them). Per Section 3.3, every attribute not reapplied is
reported, so a restricted restore is visible rather than silent.

## 7. IANA Considerations

This document has no IANA actions. The `"xattrs"` container key is assigned
by this document within the `metadata_preservation_data` extension surface
that [RAO10] governs.

## 8. References

### 8.1. Normative References

- [RAO10] — "Rem Archive Object (RAO) Format, Version 1.0", companion
  specification published alongside this document.
- [RFC2119] — Bradner, S., "Key words for use in RFCs to Indicate
  Requirement Levels", BCP 14, RFC 2119, March 1997,
  <https://www.rfc-editor.org/info/rfc2119>.
- [RFC8174] — Leiba, B., "Ambiguity of Uppercase vs Lowercase in RFC 2119
  Key Words", BCP 14, RFC 8174, May 2017,
  <https://www.rfc-editor.org/info/rfc8174>.
- [RFC3629] — Yergeau, F., "UTF-8, a transformation format of ISO 10646",
  STD 63, RFC 3629, November 2003,
  <https://www.rfc-editor.org/info/rfc3629>.
- [RFC8949] — Bormann, C. and P. Hoffman, "Concise Binary Object
  Representation (CBOR)", STD 94, RFC 8949, December 2020,
  <https://www.rfc-editor.org/info/rfc8949>.

## Author's Address

The ArchiveTech Project
Website: https://archivetech.org
Email: specs@archivetech.org
Reference implementation: https://github.com/archivetechie/remanence
