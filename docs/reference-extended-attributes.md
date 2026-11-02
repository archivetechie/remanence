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

| Metadata | Preserved | Remarks |
| --- | --- | --- |
| Content | Always | Each regular file carries its SHA-256. |
| Path | Always | A clean relative path. |
| Entry type | Always | Regular file, symbolic link, hard link or directory. |
| Symbolic link target | Always | Stored as given; it may be absolute or point outside the object. |
| Executable bit | When supplied | The one permission fact treated as content. `rem archive build` records it. |
| Modification time | When supplied | Stored in the pax `mtime` record. `rem archive build` records it. |
| Extended attributes | By capture policy | See [Capture](#capture). |
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
ordinary tar file, written with `bsdtar -c --format pax --xattrs`, stored as
one regular entry named `<path>.remwrap.tar`, with a `<path>.remwrap.idx` index
beside it unless the build is given `--no-index`. A wrapped directory takes its
whole subtree into the wrapper. The ingest report's `scan.clusters` names the
reason for each wrapped entry:

| Reason | Entry |
| --- | --- |
| `xattr` | A directory that carries a kept attribute. |
| `xattr-large` | A file or directory with a kept attribute value over 4 KiB, or with kept values over 16 KiB in total. |
| `xattr-name` | A file or directory with an attribute name that is not valid UTF-8. |
| `non-utf8-path` | An entry whose path is not valid UTF-8. |
| `non-utf8-symlink-target` | A symbolic link whose target is not valid UTF-8. |
| `unsupported-file-type` | A FIFO, socket or device node. |

Inside a wrapper, the attributes that the capture policy keeps appear as real
pax records, `SCHILY.xattr.*` and `LIBARCHIVE.xattr.*`. The build removes the
records of attributes the policy drops from the wrapper before it is stored.

## Capture

`rem archive build` captures attributes by a policy with four modes:

| Mode | Captured |
| --- | --- |
| `portable-core` | Attributes in the `user.` namespace, plus any namespace added with `xattr-namespace`. The default. |
| `denylist` | Every attribute except the names listed with `xattr-drop`. |
| `allowlist` | Only the names listed with `xattr-keep`. |
| `full-fidelity` | Every namespace. Not portable, and never the default. |

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
| `option xattr-mode MODE` | Selects one of the four modes. |
| `xattr-namespace PREFIX` | Adds a namespace to the `portable-core` set. `PREFIX` is one non-empty namespace followed by a dot, such as `security.`. |
| `xattr-drop NAME` | Drops one attribute by name. Without an explicit mode, it selects `denylist`. |
| `xattr-keep NAME` | Keeps one attribute by name in `allowlist` mode. |

`rem archive build --full-fidelity-xattrs` selects `full-fidelity` without a
ruleset; it is refused together with `--rules` or `--map`. Full-fidelity
capture does not change restore policy: privileged namespaces are still not
applied on restore unless the restoring operator adds them. In every mode,
attributes of symbolic links are not captured.

Attribute values never appear in command output. The build report's `ingest`
object records the policy and, per entry, the names that were dropped or
captured outside `user.`: `xattr_policy.mode`, `xattr_policy.allowed_prefixes`,
`xattr_policy.applied_non_core_namespaces`, `scan.dropped_xattrs`,
`scan.captured_non_core_xattrs` and `scan.totals.dropped_xattrs`. When a name
is dropped, the build also prints one warning on standard error with the
total.

For example, a ruleset containing the line `xattr-drop user.secret`, applied to
a directory whose file `notes.txt` carries `user.comment` and `user.secret`,
prints:

```text
warning: 1 extended attribute(s) dropped by capture namespace policy; see dropped_xattrs in the ingest report
```

and the build report's `ingest` object contains (trimmed):

```text
"scan": {
  "captured_non_core_xattrs": {},
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
  "allowed_prefixes": [],
  "applied_non_core_namespaces": [],
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

Restores therefore apply only the `user.` namespace by default. An attribute in
any other namespace is skipped and reported by name, and its value is not
logged. `rem archive extract`, `rem restore` and `rem-recover` accept
`--xattr-namespace PREFIX`, repeatable, to apply further namespaces in that run:

```sh
rem archive extract --object demo.rem-object --dest restored --xattr-namespace security.
```

On a standalone drive, `rem get` and `rem tape restore` apply the default and
have no such option. Through the daemon, `rem get` restores the bytes and
modification times of the members it reads and applies no extended attributes.

A skipped attribute does not fail the restore. The command prints one warning
on standard error, and its report lists the skipped names per entry in
`skipped_xattrs`, and any privileged attributes that an opt-in applied in
`applied_privileged_xattrs`. A restore of an object whose file `bin/tool`
carries `security.capability`, run without `--xattr-namespace`, prints
(illustrative):

```text
warning: 1 extended attribute(s) skipped by namespace policy; see skipped_xattrs in the report
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

Wrapped entries follow a different path. After the native restore,
`rem archive extract` unpacks each `.remwrap.tar` with
`bsdtar -x -p --xattrs`, unless it is given `--no-unwrap`. That step applies
the attribute records the wrapper carries, which are the ones the capture
policy kept, and the namespace policy of the restore does not filter them. A
wrapper built under `portable-core` therefore restores only `user.` attributes,
while a wrapper built with a widened namespace or with `full-fidelity` capture
can restore privileged attributes when the restoring account is allowed to set
them. The other restore commands do not unpack wrappers; they write the
`.remwrap.tar` and `.remwrap.idx` files as ordinary files.

## The standard-tool recovery path

A recovery made with a plain `tar` and no Remanence program reapplies no
attribute that lives in the manifest, because the outer tar stream has no
attribute records for those entries. Such a recovery restores the file bytes,
the directory structure, symbolic links and hard links. It writes the manifest
as one ordinary file, `_remanence/manifest.cbor`, whose attribute data is inert
bytes applied to nothing. For a natively represented entry, a
`security.capability` cannot be reapplied by `tar`, even with `tar --xattrs`,
because there is no record for it to find.

A wrapped entry does carry real pax attribute records, one level deeper, inside
the nested `.remwrap.tar`. A single `tar x` of the outer object does not open
the nested file, so those attributes are present in the archive and still not
applied. Combined with the exclusion of ownership and of setuid and setgid mode
bits, this means that the standard-tool path cannot raise privilege through
file metadata. Only a Remanence-aware reader can reapply privileged
attributes, and that reader applies the namespace policy described above.

## Published archives

The manifest of a plaintext object is stored in the clear and can be read with
any CBOR tool. Attribute values, symbolic link targets and absolute paths can
disclose information about the system that produced them; REM-OBJECT
Section 12.12 lists what a published plaintext object discloses. An encrypted
object places its manifest inside the authenticated, encrypted envelope, so this
disclosure does not apply to it.
