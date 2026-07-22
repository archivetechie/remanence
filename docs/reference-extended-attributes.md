# Extended attributes and file metadata

This reference explains what file metadata Remanence preserves in a RAO
object, how that metadata is stored, and — importantly — how it behaves on
restore, including the case where an archive is recovered decades later with
nothing but a standard `tar`. The normative rules live in the RAO Object
Format specification (§4.3, §4.7.3, §4.10, §12.10); this document is the
operator's companion to them.

## What is preserved, and what is deliberately not

A RAO object faithfully preserves each file's **content**, its **path**,
**entry type** (regular file, symlink, hardlink, directory), **symlink
target**, and — as the one permission fact treated as content — the
**executable bit**. Optionally it preserves **mtime** and **extended
attributes**.

It deliberately does **not** preserve **ownership** (uid/gid), and it does
not preserve **mode bits beyond the executable bit** (no setuid, setgid, or
sticky bit). The tar header carries fixed placeholder values for these
(uid/gid `0`, a normalized mode), and a reader is required to ignore them.
The reasoning is a preservation one: an archive is meant to outlive the
system that produced it, and a numeric owner or a security mode from a host
that no longer exists is not meaningful content — it is environment. The
same principle governs extended attributes, below.

## Where attributes are stored: the manifest, not the tar headers

Extended attributes are stored inside the object's **CBOR manifest**, in a
per-entry `metadata_preservation_data` map — **not** as pax
`SCHILY.xattr.*` extended-header records. This is a deliberate and
consequential choice, and it is what makes the standard-tool recovery path
(below) safe by construction. A generic tar reader has no knowledge of the
RAO manifest; to it, the manifest is simply one extra file in the archive.

## Capture

At ingest, an operator's ruleset selects which attributes are recorded.
Attribute selection is a matter of local policy, not of the byte format:
different institutions preserve different things for good reasons. Consult
the ingest configuration for the ruleset syntax (`xattr-mode`,
`xattr-keep`, `xattr-drop`).

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
surface, including the disaster-recovery reader.

## The standard-tool recovery path is safe by construction

Because attributes live in the manifest and not in tar-visible headers, a
recovery performed with a plain `tar` — the long-term fallback that needs no
Remanence binary at all — **does not reapply any extended attribute.** Such
a recovery restores the file bytes, the directory structure, symlinks and
hardlinks, and writes the manifest as one ordinary file
(`_remanence/manifest.cbor`) containing the attribute data as inert bytes,
applied to nothing. A `security.capability` in a RAO object cannot be
reconstituted by `tar`, even with `tar --xattrs`, because there is no
attribute record in the tar stream for it to find. Combined with the
exclusion of ownership and setuid/setgid mode bits, the standard-tool path
cannot escalate privilege through file metadata.

This yields a clean division of responsibility: the recovery path that
anyone can run with a forty-year-old tool is inherently safe, and the only
path that can reapply privileged metadata is the Remanence-aware reader,
which is exactly where the allow-list policy is enforced.

## A note on published archives

Preserved metadata is stored in the clear inside a plaintext object's
manifest, readable with any CBOR tool. Attribute values, symlink targets,
and absolute paths can disclose information about the originating system.
Before publishing a plaintext archive, review what its objects carry.
Encrypted RAO objects place the manifest inside the authenticated,
encrypted frame and do not have this exposure.
