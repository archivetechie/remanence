<!-- code-anchor: crates/remanence-format/src/model.rs crates/remanence-format/src/layout.rs crates/remanence-cli/src/archive_ingest.rs crates/remanence-cli/src/lib.rs crates/remanence-stream/src/lib.rs crates/rem-recover/src/main.rs @ 625f7044 -->
# Extended attributes and file metadata

This reference is for operators and archivists. It describes which file
metadata a REM-OBJECT object preserves, where that metadata is stored, which
attributes are captured and restored, and what a restore made with only a
standard `tar` reproduces. REM-OBJECT is the format of the objects Remanence
stores. An extended attribute (xattr) is a named piece of metadata that a
filesystem attaches to a file beside its content, such as `user.comment`; its
name begins with a namespace (`user.`, `security.`, `system.`, `trusted.`). The
[glossary](reference-glossary.md) defines the other terms used here.

The format rules are in the REM-OBJECT specification: Section 4.3 (the tar
header fields), Section 4.7.3 (extended-attribute preservation), Section 4.10
(standard-tool extraction) and Section 12.10 (the hazards of restoring paths,
links and attributes). Recommended restore practice is in the [REM
Implementation and Operations
Guide](../specs/publication/rem-implementation-guide.md), section 4, "Restoring
onto a host".

## Preserved metadata

This table describes native REM-OBJECT entries. Wrappers retain the tar
permission behaviour described under [Restore](#restore).

| Metadata | Preserved | Remarks |
| --- | --- | --- |
| Content | Always | Each regular file carries its SHA-256. |
| Path | Always | A clean relative path. |
| Entry type | Always | Regular file, symbolic link, hard link or directory. |
| Symbolic link target | Always | Stored as given; it may be absolute or point outside the object. |
| Executable bit | When supplied | The one permission fact treated as content. `rem archive build` records it. |
| Modification time | When supplied | Stored in the pax `mtime` record. `rem archive build` records it. |
| Extended attributes | `user.*` only | See [Capture](#capture). |
| ACLs, file flags and privileged attributes | Never | These describe the originating host’s security posture. |
| Ownership (uid, gid, user and group names) | Never | The tar header carries fixed placeholder values, and readers ignore them. |
| Mode bits other than the executable bit | Never | No setuid, setgid or sticky bit. The header carries a normalized mode, and readers ignore it. |

Ownership and security mode bits are excluded because an archive is meant to
outlive the system that produced it. A numeric owner or a security mode from a
host that no longer exists describes that host's environment, not the file's
content. The same reasoning governs which extended attributes are captured and
restored.

## Where attributes are stored

Extended attributes are stored inside the object's manifest, in the
`metadata_preservation_data` map of each entry. The manifest is a file in CBOR,
a compact binary data format, stored in the object as
`_remanence/manifest.cbor`. Attributes are not stored as pax `SCHILY.xattr.*`
extended-header records. A generic tar reader does not know the manifest's
meaning and extracts it as one extra file, so it applies none of the
attributes; see [The standard-tool recovery
path](#the-standard-tool-recovery-path).

That is the path for every entry the manifest can represent natively. An entry
it cannot represent is packed by `rem archive build` into a wrapper: an
ordinary tar file, written with `bsdtar -c --format pax` and metadata capture disabled, stored as
one regular entry named `<path>.remwrap.tar`, with a `<path>.remwrap.idx` index
beside it unless the build is given `--no-index`. A wrapped directory takes its
whole subtree into the wrapper. The ingest report's `scan.clusters` names the
reason for each wrapped entry:

| Reason | Entry |
| --- | --- |
| `non-utf8-path` | An entry whose path is not valid UTF-8. |
| `non-utf8-symlink-target` | A symbolic link whose target is not valid UTF-8. |
| `unsupported-file-type` | A FIFO, socket or device node. |

Attributes never cause wrapping. Native files and directories carry their
selected `user.*` attributes in the manifest, including values larger than
4 KiB. A selected attribute name that cannot be represented as valid UTF-8
causes an error instead of a wrapper.

Wrappers preserve content and the existing tar permission behaviour. They carry
no xattrs, ACLs, file flags or Mac metadata. Attributes on entries selected for
a wrapper, including an explicitly requested blob subtree, are reported as
not captured. They cannot be attached to individual native manifest entries
because those entries are inside the wrapper.

## Capture

`rem archive build` captures attributes only in the `user.` namespace, with three selection modes:

| Mode | Captured |
| --- | --- |
| `portable-core` | All `user.*` attributes. The default. |
| `denylist` | `user.*` attributes except the names listed with `xattr-drop`. |
| `allowlist` | Only the `user.*` names listed with `xattr-keep`. |

In every mode, six macOS housekeeping attributes are discarded without being
reported: `com.apple.quarantine`, `com.apple.metadata:kMDItemWhereFroms`,
`com.apple.lastuseddate#PS`, `com.apple.FinderInfo`,
`com.apple.metadata:_kMDItemFinderComment` and
`com.apple.metadata:kMDItemDownloadedDate`.

The mode is local policy and is not part of the byte format. It is set in an
ingest ruleset given to `rem archive build --rules <PATH>`, one directive per
line:

| Directive | Effect |
| --- | --- |
| `option xattr-mode MODE` | Selects one of the three modes. |
| `xattr-drop NAME` | Drops one attribute by name. Without an explicit mode, it selects `denylist`. |
| `xattr-keep NAME` | Keeps one attribute by name in `allowlist` mode. |

Rules may name only `user.*` attributes. The former `full-fidelity` mode,
`--full-fidelity-xattrs` flag and `xattr-namespace` ruleset key have been removed;
using one produces an error explaining the change. Remanence is a long-term
preservation tool. Security labels, ACLs and file flags describe one host's
security posture and are not captured for application on a future host.

Attribute values never appear in command output. The build report's `ingest`
object records `xattr_policy.mode`, `xattr_policy.allowed_prefixes` (always
`["user."]`), `scan.dropped_xattrs` and `scan.totals.dropped_xattrs`. Dropped
names are listed per path; the build also prints one warning on standard
error with the total. The clustered `scan.xattr_drops` view lists the same
names. No report field records captured privileged attributes.

Objects with any preserved attributes use stream schema `1.1`; those without
use `1.0`. The manifest schema integer remains `1` in either case.

For example, a ruleset containing the line `xattr-drop user.secret`, applied to
a directory whose file `notes.txt` carries `user.comment` and `user.secret`,
prints:

```text
warning: 1 extended attribute(s) dropped by capture policy; see dropped_xattrs in the ingest report
```

and the build report's `ingest` object contains (trimmed):

```text
"scan": {
  "dropped_xattrs": {
    "notes.txt": [
      "user.secret"
    ]
  },
  "totals": {
    "dropped_xattrs": 1,
    "native_entries": 1,
    ...
  },
  ...
},
"xattr_policy": {
  "allowed_prefixes": ["user."],
  "mode": "denylist"
}
```

The object keeps `user.comment` for `notes.txt`.

## Restore

Reapplying a stored attribute writes it onto a file on the machine that
performs the restore, which may be a different machine from the one that
produced the archive. Some namespaces carry privilege or access-control
meaning. On Linux, `security.capability` grants a program elevated privileges,
`security.*` and `system.*` carry SELinux labels and POSIX access control
lists, and `trusted.*` carries other privileged state. A restore that applied
these without being asked would let a cartridge configure the security of the
host that reads it.

Restores apply only `user.*` attributes through Remanence's native restore
code. Every other name in a manifest, including one from an older or foreign
writer, is skipped and reported by name without its value. There is no
option to widen this policy. The former `--xattr-namespace` flag is refused
by every command that previously accepted it, including `rem-recover`.

On a standalone drive, `rem get` and `rem tape restore` apply the default and
have no such option. Through the daemon, `rem get` restores the bytes and
modification times of the members it reads and applies no extended attributes.

A skipped attribute does not fail the restore. The command prints one warning
on standard error, and its report lists the skipped names per entry in
`skipped_xattrs`. The reserved `applied_privileged_xattrs` field is always
empty. A restore of an object whose file `bin/tool`
carries `security.capability` prints
(illustrative):

```text
warning: 1 extended attribute(s) skipped during restore; see skipped_xattrs in the report
```

and its report contains:

```text
"applied_privileged_xattrs": {},
"skipped_xattrs": {
  "bin/tool": [
    "security.capability"
  ]
},
```

An attribute write never follows a symbolic link at the last path component;
the attribute is applied to the restored entry itself. A failure to apply an
allowed attribute is reported as a failure, except when the destination
filesystem does not support extended attributes, which is treated as a skip.

After native restore, `rem archive extract` unpacks each `.remwrap.tar`,
unless given `--no-unwrap`, using:

```text
bsdtar -x -p --no-xattrs --no-acls --no-fflags --no-mac-metadata
```

`-p` keeps the existing wrapper permission behaviour; the four explicit
options disable its attribute, ACL, file-flag and Mac metadata effects. A
wrapper from an older or foreign writer restores content without applying
any of those records, including `user.*`. `skipped_xattrs` lists attribute
names; `skipped_metadata` lists ACL and file-flag record names per path. Both
also appear under `unwrap`. Values are never included.

Before extraction, Remanence checks every header checksum, validates member
and extension paths (including `X`, `L` and `K` records), and limits each pax
or long-path header to 1 MiB. Malformed or unsupported wrappers fail before
extraction. No temporary wrapper copy is made. The other restore commands
write `.remwrap.tar` and `.remwrap.idx` as ordinary files.

## The standard-tool recovery path

A recovery made with a plain `tar` and no Remanence program reapplies no
attribute that lives in the manifest, because the outer tar stream has no
attribute records for those entries. Such a recovery restores the file bytes,
the directory structure, symbolic links and hard links. It writes the manifest
as one ordinary file, `_remanence/manifest.cbor`, whose attribute data is inert
bytes applied to nothing. For a natively represented entry, a
`security.capability` cannot be reapplied by `tar`, even with `tar --xattrs`,
because there is no record for it to find.

New wrappers carry no attribute, ACL or file-flag records. Older or foreign
wrappers may contain them. Extracting the outer object leaves each nested
wrapper as a file; manually unpacking it with a standard tool follows that
tool's policy. Use the four disabling options above when recovering such a
wrapper with bsdtar.

## Published archives

The manifest of a plaintext object is stored in the clear and can be read with
any CBOR tool. Attribute values, symbolic link targets and absolute paths can
disclose information about the system that produced them; REM-OBJECT
Section 12.12 lists what a published plaintext object discloses. An encrypted
object places its manifest inside the authenticated, encrypted envelope, so this
disclosure does not apply to it.
