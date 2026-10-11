<!-- code-anchor: crates/remanence-format/src/model.rs crates/remanence-format/src/tar.rs crates/remanence-format/src/writer.rs crates/remanence-cli/src/archive_ingest.rs crates/remanence-stream/src/lib.rs crates/rem-recover/src/main.rs @ 6ed03ef0 -->
# Extended attributes and file metadata

This reference describes which file metadata Remanence preserves in a REM-OBJECT
object, how that metadata is stored, and how it behaves on restore, including
a restore made years later with nothing but a standard `tar`. An extended
attribute (xattr) is a named piece of metadata that a filesystem attaches to a
file in addition to its content, for example `user.comment`. REM-OBJECT is the
native stored-object format; the [glossary](reference-glossary.md) defines the
other terms used here. The format rules live in the REM-OBJECT
specification (§4.3, §4.7.3, §4.10), and §12.10 describes the restore hazards
and what a restore must reproduce; recommended restore practice is in the
[REM Implementation and Operations Guide](../specs/publication/rem-implementation-guide.md), under
“Restoring onto a host”. This document is the operator's companion to both.

## What is preserved, and what is deliberately not

A REM-OBJECT object faithfully preserves each file's **content**, its **path**,
**entry type** (regular file, symlink, hardlink, directory), **symlink
target**, and — as the one permission fact treated as content — the
**executable bit**. Optionally it preserves **mtime** and **extended
attributes**.

It deliberately does **not** preserve **ownership** (uid/gid), and it does
not preserve **mode bits beyond the executable bit** (no setuid, setgid, or
sticky bit). The tar header carries fixed placeholder values for these
(uid/gid `0`, a normalized mode), and a reader is required to ignore them.
The reason is that an archive is meant to outlive the system that produced
it. A numeric owner or a security mode from a host that no longer exists
describes that host's environment and is not part of the file's content. The
same reasoning governs extended attributes, below.

## Where attributes are stored: the manifest, not the tar headers

Extended attributes are stored inside the object's **CBOR manifest**, in a
per-entry `metadata_preservation_data` map. They are not stored as pax
`SCHILY.xattr.*` extended-header records. This choice makes the standard-tool
recovery path (below) safe. The manifest is a file in CBOR, a compact binary
data format. A generic tar reader does not know the manifest's meaning and
treats it as one extra file in the archive.

This is the path for every natively representable entry. An entry with a
non-native file type, or with a single kept attribute value over 4 KiB, or
with a kept-attribute total over 16 KiB, is instead wrapped: `rem archive
build` stores it as an ordinary `tar --xattrs` blob (named
`<path>.remwrap.tar`, itself one regular entry in the outer manifest), and
the attributes it keeps by policy do appear there as real, filtered
`SCHILY.xattr.*`/`LIBARCHIVE.xattr.*` pax records. This does not weaken the
recovery guarantee below — a plain `tar x` on the outer archive does not
recurse into a nested `.remwrap.tar`, so those attributes still are not
reapplied by the standard-tool path — but a reader inspecting a wrapped
entry's bytes directly, rather than restoring through `rem`, finds pax
xattr records there.

## Capture

Capture is portable by default. `rem archive build` records attributes in the
`user.` namespace and drops attributes in every other namespace, including
`security.*`, `system.*`, and `trusted.*`. Dropped names are listed per entry
in the ingest report, and one stderr warning gives the total count. Attribute
values are never included in command output. Six built-in macOS housekeeping
attributes are always discarded silently, so they do not add report noise.

Attribute selection is a matter of local policy and is not part of the byte format.
An ingest ruleset can widen the portable core with a validated namespace
prefix:

```text
xattr-namespace security.
```

The prefix must be one non-empty namespace followed by a dot. The report's
`xattr_policy.applied_non_core_namespaces` field records such widening, while
`scan.captured_non_core_xattrs` lists the captured names per entry. Existing
exact-name policies remain available through `option xattr-mode denylist` plus
`xattr-drop`, or `option xattr-mode allowlist` plus `xattr-keep`.

For records mandates that require every available namespace, use either
`option xattr-mode full-fidelity` in a ruleset or the
`--full-fidelity-xattrs` build flag without a ruleset. Full-fidelity logical
capture is explicit, is not portable, and still omits the built-in junk names.
It does not relax restore policy: privileged namespaces remain denied on
restore unless the restoring operator opts in.

## Restore, and why it is cautious by default

Reapplying a stored attribute writes it onto a file on the machine
performing the restore — a machine that may be entirely different from the
one that produced the archive. Some attribute namespaces carry privilege or
access-control meaning: on Linux, `security.capability` grants a binary
elevated privileges, `security.*` and `system.*` carry SELinux labels and
POSIX ACLs, and `trusted.*` carries other privileged state. An archive that
silently reconstituted these on restore would let a cartridge configure the
security posture of the host that reads it.

Remanence therefore restores conservatively by default:

- **Allow-list.** Only attributes in the `user.` namespace are applied.
  Attributes outside it are **skipped and reported by name** (never applied,
  and their values are never logged). To apply additional namespaces, an
  operator opts in explicitly, per invocation:

  ```sh
  rem archive extract … --xattr-namespace security.
  ```

  The flag is repeatable and appends to the allow-list; with no flag, only
  `user.` is applied. The restore report lists both the attributes that were
  skipped and any privileged attributes that were applied, so an opt-in's
  effect is auditable.

- **No symlink following.** Attribute writes never traverse a symbolic link
  at the final path component; they are applied to the restored file itself,
  never to whatever a link might point at.

- **Errors are surfaced.** A genuine failure to apply an allowed attribute
  is reported as a failure. Only "this filesystem does not support extended
  attributes" is treated as a benign skip.

The same allow-list, opt-in, and reporting behavior applies to every restore
surface, including the standalone disaster-recovery tool `rem-recover`.

## The standard-tool recovery path

Because attributes live in the manifest and not in tar-visible headers, a
recovery performed with a plain `tar` does not reapply any extended attribute.
This is the long-term fallback that needs no Remanence binary. Such
a recovery restores the file bytes, the directory structure, symlinks and
hardlinks, and writes the manifest as one ordinary file
(`_remanence/manifest.cbor`) containing the attribute data as inert bytes,
applied to nothing. For a natively represented entry, a `security.capability`
cannot be reconstituted by `tar`, even with `tar --xattrs`, because there is
no attribute record in the outer tar stream for it to find. A wrapped entry
(see above) does carry a real pax record for it, but one level deeper, inside
a nested `.remwrap.tar` blob that a single non-recursive `tar x` never opens
— the attribute is present in the archive but still not reapplied. Combined
with the exclusion of ownership and setuid/setgid mode bits, the
standard-tool path cannot escalate privilege through file metadata.

Responsibility is therefore divided as follows. The recovery path that anyone
can run with an old, unchanged `tar` cannot reapply privileged metadata. Only
the Remanence-aware reader can, and that is where the allow-list policy is
enforced.

## Published archives

Preserved metadata is stored in the clear inside a plaintext object's
manifest, readable with any CBOR tool. Attribute values, symlink targets,
and absolute paths can disclose information about the originating system.
Before publishing a plaintext archive, review what its objects carry.
Encrypted REM-OBJECT objects place the manifest inside the authenticated,
encrypted frame and do not have this exposure.
