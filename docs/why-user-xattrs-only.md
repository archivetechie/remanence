<!-- code-anchor: crates/remanence-cli/src/archive_ingest.rs crates/remanence-cli/src/lib.rs crates/remanence-stream/src/lib.rs specs/in-progress/rem-object-core-1-specification.md @ f04a0951 -->
# Why Remanence keeps only user extended attributes

This page is for an operator or an archivist deciding whether Remanence suits a collection
whose files carry extended attributes, access control lists or file flags. It also gives a
reviewer the reasons behind the choice. Remanence captures the `user.*` extended attributes
of the files it archives, and restores them. It does not capture security labels, file
capabilities, access control lists, trusted attributes or file flags, and it never applies
them on restore. Sections 1 to 9 explain why. Sections 10 and 11 list the cases where the
decision costs something, with a recipe for each.

The [extended attributes reference](reference-extended-attributes.md) describes the exact
behaviour: what is captured, where it is stored, and what each report field means.

## 1. What extended attributes are

An **extended attribute** (xattr) is a named value that a filesystem stores with a file,
beside its contents. A program that copies only a file's bytes leaves its attributes behind.
On Linux the part of the name before the first dot is the namespace. The namespace decides
who may read and write the attribute, and whether the kernel gives it any meaning
([xattr(7)](https://man7.org/linux/man-pages/man7/xattr.7.html)).

POSIX access control lists (ACLs), which grant access to named users and groups beyond a
file's owner, group and other, are stored through the same mechanism. File flags are stored
differently, but they raise the same questions, so this page treats them together.

| Kind | Examples of what it holds | Who may set it | Remarks |
| --- | --- | --- | --- |
| `user.*` attributes | The address a file was downloaded from, in `user.xdg.origin.url`, which `curl --xattr` and `wget --xattr` record. A MIME type in `user.mime_type`. The DOS attributes (read-only, hidden, system, archive) that a Samba server keeps for Windows clients in `user.DOSATTRIB`. | Anyone allowed to write the file. | The kernel stores the value and attaches no meaning to it. Linux allows these attributes only on regular files and directories. |
| `security.*` attributes | An SELinux label such as `system_u:object_r:httpd_sys_content_t:s0`, in `security.selinux`. A file capability, in `security.capability`: one of the separate privileges into which Linux divides the powers of root, attached to a program. On many distributions `ping` carries `cap_net_raw`, which lets it open raw network sockets without running as root. Integrity data for IMA and EVM (the kernel's Integrity Measurement Architecture and Extended Verification Module) in `security.ima` and `security.evm`. The Windows ACLs that a Samba server keeps in `security.NTACL`. | Root, or a process holding the matching capability. | The kernel's security mechanisms act on these attributes. They change what a program may do, or who may open a file. |
| `trusted.*` attributes | State kept by system software. GlusterFS records a file's identity within the cluster in `trusted.gfid` and its replication state in `trusted.afr.*`. overlayfs marks opaque and renamed directories in its upper directory with `trusted.overlay.*`. | Only processes holding the CAP_SYS_ADMIN capability, which they also need to read these attributes or see that they exist. | Meaningful only to the software that wrote them. |
| POSIX ACLs, stored as `system.posix_acl_access` and `system.posix_acl_default` | Access entries such as `user:2001:rw-`, which lets the account with user ID 2001 read and write the file. A directory's default ACL is copied to the files created in it. | The file's owner. | Entries name accounts by number. |
| File flags | Inode flags shown by `lsattr` and set by `chattr`, such as `i` (immutable), `a` (append only) and `d` (skipped by the `dump` program). They are not extended attributes. | Root for `i` and `a`; the owner for most others. | The kernel enforces them on the filesystem that holds them. |

## 2. Where we started

Our starting point was to keep everything, for the usual archival reason. Archiving is the moment when
the information is available, and anything not captured then cannot be recovered later. A
format that carries every attribute loses nothing, and a later reader can decide what to use.

REM-OBJECT, the format of the objects Remanence stores, was designed with that in mind, and it
can still carry attributes of any namespace (section 9). Earlier versions of Remanence had
options to capture the other namespaces and to apply them on restore. Those options
(`--full-fidelity-xattrs`, `--xattr-namespace` and the ruleset key `xattr-namespace`) are now
refused, with a message saying that Remanence preserves only `user.*` attributes. We would
still prefer to lose nothing, and the sections below explain why we decided otherwise.

## 3. What Remanence is for

Remanence is a long-term preservation tool. It writes collections to LTO tape in published
formats, so that they can be read many years later, on other hardware and with other
software, possibly without Remanence at all. The machine that reads a tape back is usually
not the one that wrote it. It may run another operating system, with other accounts, another
security policy and none of the services that the original host ran.

Remanence is not a system backup tool, and it does not do bare-metal restore. A system backup
exists to put a machine back the way it was after a failure: its programs, accounts,
permissions, security labels and the state of its services. Bare-metal restore is the extreme
case, in which the machine is rebuilt from an empty disk. Both concern one host at one moment,
and that host's security metadata is a large part of what they put back.

The two jobs keep different things. A file's contents, and notes about the file such as where
it came from, belong to the file and stay true wherever it goes. The rules that one host
applied to the file belong to that host.

## 4. Why the other kinds do not travel

Every kind of metadata outside `user.*` refers to something on the original host.

Access control lists refer to accounts. A POSIX ACL entry names a user ID or a group ID. On
the restoring host, years later, that number belongs to whoever it was given to there, or to
nobody. An NT ACL in `security.NTACL` names Windows security identifiers (SIDs), which mean
something only inside the domain that issued them.

Security labels, integrity data and file flags refer to policy. An SELinux label is a name in
one system's policy, and another system may use another policy or none. IMA and EVM data is
checked against keys that one system trusts. An EVM HMAC, a checksum computed with a secret
key, is made with a key held on that machine, so no other machine can verify it. The immutable
and append-only flags record a decision that an administrator made about that machine.

File capabilities and `trusted.*` state refer to software. A capability is set on a program by
the package that installed it, for that program on that system. GlusterFS and overlayfs state
describes one cluster, or one directory layout, as that software saw it.

Applied on another system, this metadata is at best meaningless. At worst it is wrong. An ACL
entry that gave a colleague access to a share would give the same access to whoever holds
that user ID on the new host. Restoring the metadata faithfully would reproduce one host's
security arrangements on a host to which they do not belong.

## 5. Why restoring them is a risk

The main reason is that applying this metadata changes what the restoring host permits. A
`security.capability` on a restored program gives the program privileges every time it runs.
An SELinux label can place a file in a domain that the local policy trusts, and an ACL entry
grants access to an account. Applying them needs root, so a restore that applied them would let
whoever wrote the cartridge decide part of the security state of the machine that reads it.

A second reason is that attribute data read from a tape is untrusted input. The tape may come
from another institution, may have been written by other software, or may have been altered.
The REM-OBJECT specification treats preserved attributes as untrusted for this reason
(REM-OBJECT §12.10, Path Traversal).

A third reason comes from experience. In an earlier design some entries were stored as nested
tar archives. On restore, Remanence filtered out the attributes it did not allow, and the
system's tar program then applied the rest. An independent security review found several ways
for attributes and ACLs to reach the restored files despite the filter, because the tar
program interpreted more of the archive than the filter had checked. A filter in front of
another program protects only as far as both programs read the input in the same way, and two
independent implementations of tar do not. Remanence now applies attributes itself, from the
object's manifest, and gives no other program authority to apply metadata from an archive.

Against these risks the benefit was small. The attributes would mean little on the host that
restores them, and the cases that need them are better served by tools built for that purpose
(section 8). For this tool, the risks outweighed the benefits.

## 6. Why user attributes are still kept

The main reason is that archiving is the irreversible moment. User attributes often record
facts that exist nowhere else, such as the address a file was downloaded from, its MIME type,
or a tag that another program attached. If ingest does not capture them, they are lost.

A second reason is that they carry no privilege. The kernel stores a user attribute as a name
and a value, and checks only the file's ordinary permissions before reading or writing it.
Restoring one needs no privilege and grants none. A program may read meaning into a user
attribute, as Samba does with `user.DOSATTRIB`, but the attribute itself gives no account any
access.

User attributes are stored in the object's manifest, in each entry's
`metadata_preservation_data` (REM-OBJECT §4.7.3, Extended-Attribute Preservation). An ingest
ruleset can narrow the selection by naming `user.*` attributes to drop or to keep; the
[reference](reference-extended-attributes.md#capture) describes the rules.

Remanence applies the attributes itself when it restores an object, writing each one onto the
restored entry without following a symbolic link. A failure to apply one stops the restore
with an error that names the attribute and the file, and a destination filesystem without
attribute support is reported as a skip. A plain `tar` extraction of the object applies none
of them, because it extracts the manifest as an ordinary file. The
[reference](reference-extended-attributes.md#restore) lists which restore commands apply
attributes; through the daemon, `rem get` writes file contents and modification times only.

## 7. Nothing is dropped silently

When `rem archive build` reads a file, it lists all of the file's extended attributes. It
captures the `user.*` attributes that its policy selects. It records the names of all the
others, per path, in the `ingest` section of its JSON report, as `scan.dropped_xattrs`, and
prints one warning with the total. Values are never reported.

The example tree used in section 11 has POSIX ACLs, which Linux presents as
`system.posix_acl_*` attributes. To build it and print the dropped names:

```sh
rem archive build --inputs share --out share.rem-object > build.json
jq '.ingest.scan.dropped_xattrs' build.json
```

The build prints a warning, and the report names the two ACL attributes:

```text
warning: 2 extended attribute(s) dropped by capture policy; see dropped_xattrs in the ingest report
{
  "reports": [
    "system.posix_acl_default"
  ],
  "reports/survey.csv": [
    "system.posix_acl_access"
  ]
}
```

A security label, a file capability or an NT ACL is reported in the same way, by its
attribute name.

The report lists what the ingest process can see, and three kinds of metadata do not appear in
it. `trusted.*` attributes are invisible to a process without CAP_SYS_ADMIN, so an ingest run as
an ordinary user cannot report them. File flags are not extended attributes. Ownership and most
permission bits are not kept for any file, as the
[table of preserved metadata](reference-extended-attributes.md#preserved-metadata) shows, so
the report does not list them file by file.

Restores report in the same way. When an object written by other software carries attributes
outside `user.*`, Remanence skips them, prints a warning, and lists their names in the restore
report's `skipped_xattrs`.

## 8. A deliberate limit

This was a conscious decision, taken knowing what it costs. If you need a backup that
preserves security attributes with full fidelity, Remanence is not the right tool.

Three kinds of tool are built for that job. A system backup tool records a host's files with
their ownership, permissions, ACLs, labels and capabilities, so that the host can be rebuilt.
A filesystem dump tool copies a filesystem with everything that filesystem stores. A disk or
filesystem imaging tool copies a disk or a partition block by block. Each is designed and
tested to put a particular machine back as it was.

They work alongside Remanence because they answer a different question. A system backup
answers how to get a machine running again after a failure. Remanence answers how someone will
read a collection many years from now, on another system. A site may need both, run
separately, and Remanence can also hold such a tool's output as one opaque file, returned
unchanged (section 11.2).

## 9. What the format allows

The limit is Remanence's policy, not a limit of the format. A REM-OBJECT entry's
`metadata_preservation_data` may hold an `xattrs` map from attribute names to raw values, of
any namespace. The specification defines two tiers: the portable core, which is the `user.`
namespace, and the extension tier, which is every other attribute. Both are carried
faithfully, and the choice of what to capture is left to the ingesting system: "Which xattrs
an ingesting system selects is policy outside this byte format" (REM-OBJECT §4.7.3,
Extended-Attribute Preservation).

An object that carries any attribute outside `user.` must list those namespaces in an
inventory in its `object_metadata`, so that a holder can see them without decoding every
entry (REM-OBJECT §4.7.6, Object Metadata Inventory). The specification treats every
preserved attribute as untrusted on restore (REM-OBJECT §12.10, Path Traversal). The
[REM Implementation and Operations Guide](../specs/publication/rem-implementation-guide.md)
recommends, in chapter 4, "Restoring onto a host", that a restoring tool apply only `user.`
attributes by default, and other namespaces only when explicit operator policy names them.

Another implementation could therefore write objects that carry security labels or ACLs, and
a reader could apply them under such a policy. Remanence does neither: it writes only `user.*`
attributes, and on restore it skips and reports every other name (section 7).

## 10. When the decision costs something

The cases below are those in which keeping only user attributes loses something that someone
may need, roughly in order of how often they arise. The table names what Remanence does not
keep in each case, and the recipe in section 11 that addresses it.

| Case | What Remanence does not keep | Recipe |
| --- | --- | --- |
| 1. A departmental or research share whose access rules are POSIX ACLs | The ACLs, and the owners, groups and setgid directories that the access rules depend on | 11.1, or 11.2 |
| 2. A Samba share used from Windows | The NT ACLs in `security.NTACL`. The DOS attributes in `user.DOSATTRIB` are kept. | 11.3 |
| 3. A system or bare-metal backup of a Linux host | File capabilities, such as `cap_net_raw` on `ping`; SELinux labels, which need relabelling on restore; ownership and setuid bits | 11.7, with 11.4 and 11.5 |
| 4. A host that uses IMA or EVM | `security.ima` and `security.evm` | 11.7 |
| 5. A container image or a root filesystem archived as a directory tree | File capabilities, ownership and setuid bits, without which programs in the image fail or behave differently | 11.7, or 11.2 run as root |
| 6. Forensic or legal preservation, where the security metadata is itself evidence | Every attribute outside `user.*`, file flags and ownership | 11.2 or a forensic image (11.7), with the dumps of 11.1 as a readable copy |
| 7. Files under compliance flags, such as `chattr +i` or `chattr +a` | The flags | 11.6 |
| 8. Software that keeps state in `trusted.*`, such as GlusterFS bricks and overlayfs upper directories | The `trusted.*` attributes | 11.7 |

## 11. Recipes

Each recipe says what it preserves, what it does not, which commands need root, and what to
check after a restore. The recipes were run on Linux, on an ext4 filesystem, as an ordinary
user without root, with GNU tar 1.35, bsdtar 3.7.2, and the `acl`, `attr` and `e2fsprogs`
tools. Steps that need root, an SELinux system or a Samba domain could not be run there. Each
recipe says which of its steps were run.

The recipes share one example, a directory `share` of the kind a department might keep. To
create it:

```sh
mkdir -p share/reports share/tools
printf 'site,count\nnorth,12\n' > share/reports/survey.csv
printf '#!/bin/sh\necho hello\n' > share/tools/run.sh
chmod 755 share/tools/run.sh
setfattr -n user.xdg.origin.url -v 'https://example.org/survey.csv' share/reports/survey.csv
setfattr -n user.mime_type -v 'text/csv' share/reports/survey.csv
setfacl -m u:2001:rw-,g:users:r-- share/reports/survey.csv
chmod 2775 share/reports
setfacl -d -m g:users:r-x share/reports
chattr +d share/tools/run.sh
```

The file `reports/survey.csv` has two user attributes, and an ACL that gives user ID 2001 read
and write access and the group `users` (group ID 100) read access. The directory `reports` has
the setgid bit and a default ACL. The script `tools/run.sh` has the `d` flag.

`rem archive build` places the contents of a directory given to `--inputs` at the root of the
object, so a restore of `share` produces `reports` and `tools` directly under the destination.
A file given to `--inputs`, such as a metadata dump, lands at the root beside them. Archived
and restored by Remanence alone, the tree keeps its user attributes and loses the rest:

```sh
rem archive build --inputs share --out share.rem-object > build.json
rem archive extract --object share.rem-object --dest restored > extract.json
getfattr -R -d -m - restored
getfacl -R --numeric restored/reports
lsattr restored/tools/run.sh
```

The build prints the warning shown in section 7. The restored tree has its user attributes,
no ACLs, no setgid bit and no `d` flag:

```text
# file: restored/reports/survey.csv
user.mime_type="text/csv"
user.xdg.origin.url="https://example.org/survey.csv"

# file: restored/reports
# owner: 1000
# group: 1000
user::rwx
group::r-x
other::r-x

# file: restored/reports/survey.csv
# owner: 1000
# group: 1000
user::rw-
group::r--
other::r--

--------------e------- restored/tools/run.sh
```

The `e` that `lsattr` shows on every file is set by ext4 itself, for files stored in extents.

### 11.1 Keep the metadata as data next to the files

This recipe records ACLs, ownership, permission bits and every readable extended attribute in
text files, and archives the text files with the tree. Remanence stores them like any other
files. After a restore, `setfacl` and `setfattr` read them back and apply them.

Run the dumps from inside the tree, so that the paths they record are relative and apply
wherever the tree is restored. Then give the dumps to `rem archive build` beside the tree:

```sh
cd share
getfacl -R --numeric . > ../share.acl
getfattr -R -d -m - . > ../share.xattr
cd ..
rem archive build --inputs share share.acl share.xattr --out share-meta.rem-object > build.json
```

`getfacl` writes one block per file: the owner, the group, the setuid, setgid and sticky bits
(the `# flags:` line), and the ACL. For the two entries under `reports`, `share.acl` holds:

```text
# file: reports
# owner: 1000
# group: 1000
# flags: -s-
user::rwx
group::rwx
other::r-x
default:user::rwx
default:group::rwx
default:group:100:r-x
default:mask::rwx
default:other::r-x

# file: reports/survey.csv
# owner: 1000
# group: 1000
user::rw-
user:2001:rw-
group::r--
group:100:r--
mask::rw-
other::r--
```

`getfattr -m -` dumps every attribute the process can read, including the ACLs a second time,
in their binary form. Text values are quoted, and binary values are written in base64 after
`0s`. `share.xattr` holds:

```text
# file: reports
system.posix_acl_default=0sAgAAAAEABwD/////BAAHAP////8IAAUAZAAAABAABwD/////IAAFAP////8=

# file: reports/survey.csv
system.posix_acl_access=0sAgAAAAEABgD/////AgAGANEHAAAEAAQA/////wgABABkAAAAEAAGAP////8gAAQA/////w==
user.mime_type="text/csv"
user.xdg.origin.url="https://example.org/survey.csv"
```

To restore, extract the object, then apply the dumps from the top of the restored tree:

```sh
rem archive extract --object share-meta.rem-object --dest restored > extract.json
cd restored
setfacl --restore=share.acl
setfattr --restore=share.xattr
getfacl -R --numeric reports
```

`getfacl` prints the same two blocks as the dump above: the ACLs, the default ACL and the
setgid bit are back in place. To check the whole tree, compare a fresh dump with the archived
one. The only differences should be the entries for the two dump files:

```sh
getfacl -R --numeric . | diff share.acl - | grep '^> # file'
```

```text
> # file: share.acl
> # file: share.xattr
```

The recipe preserves ACLs and default ACLs, owners and groups, permission bits including the
setuid, setgid and sticky bits, and every extended attribute that the dumping process can
read; a dump made as root also includes `trusted.*` attributes. It does not preserve file
flags, which section 11.6 covers. The dumps are plain text in the formats that the `getfacl`
and `getfattr` manual pages describe, so a person can read them even where nothing can apply
them.

`--numeric` records the user and group IDs that the filesystem stores, and `setfacl` applies
them unchanged. Without it, `getfacl` writes account names, which `setfacl` looks up again on
the restoring host. Either way, check that they refer to the same accounts there.
`--absolute-names` would keep absolute paths, which suit only a restore to the original
location.

Dumping `trusted.*` attributes, and restoring other users' ownership or `security.*` and
`trusted.*` attributes, need root. The owner of the restored files can restore the rest.
Without root, `setfacl --restore` still applies the ACL entries, and prints
`Cannot change owner/group: Operation not permitted` for each file whose owner or group it
cannot set. `setfattr --restore` prints `Operation not permitted` for each attribute it cannot
set, and continues with the rest.

The recipe was run end to end as an ordinary user on a tree that user owned, and both failure
messages above were observed. The steps that need root were not run.

### 11.2 Archive the tree as one opaque file

This recipe packs the tree with an archiver that records the metadata you need, and gives
Remanence the result as one regular file. Remanence stores the file, records its SHA-256 and
returns it unchanged. The archiver that wrote it unpacks it.

With GNU tar, to pack the tree and archive the result:

```sh
tar --create --format=posix --xattrs --xattrs-include='*' --acls --selinux --file=share.tar -C share .
rem archive build --inputs share.tar --out share-tar.rem-object > build.json
jq -r '.files[] | "\(.path) \(.file_sha256)"' build.json
```

The build report has one entry, for the tar file:

```text
share.tar 9e68fde22383426eaa52c2515472f4b41df8eb5d47daa61c614d737b6154d7ee
```

To restore, extract the object, check the tar file against the report, and unpack it with the
same options:

```sh
rem archive extract --object share-tar.rem-object --dest restored > extract.json
sha256sum restored/share.tar
mkdir share-again
tar --extract --xattrs --xattrs-include='*' --acls --selinux --preserve-permissions --file=restored/share.tar -C share-again
```

The checksum matches the build report:

```text
9e68fde22383426eaa52c2515472f4b41df8eb5d47daa61c614d737b6154d7ee  restored/share.tar
```

In `share-again`, `getfacl -R --numeric` and `getfattr -R -d -m -` print the same ACLs,
default ACL, setgid bit and user attributes as for `share`. `lsattr` shows that the `d` flag
on `tools/run.sh` is gone, because GNU tar does not record file flags. GNU tar records
attributes of every namespace it can read when it creates an archive, but on extraction it
applies only `user.*` attributes unless `--xattrs-include` names others, so the option appears
on both commands. `--format=posix` writes the POSIX pax format, in which the attributes and
ACLs are extended-header records (`SCHILY.xattr.*`, `SCHILY.acl.*`).

With bsdtar, which also records file flags:

```sh
bsdtar --create --format pax --xattrs --acls --fflags --file share.tar -C share .
rem archive build --inputs share.tar --out share-tar.rem-object > build.json
rem archive extract --object share-tar.rem-object --dest restored > extract.json
mkdir share-again
bsdtar --extract --preserve-permissions --xattrs --acls --fflags --file restored/share.tar -C share-again
lsattr share-again/tools/run.sh
```

The ACLs, the setgid bit and the user attributes come back as with GNU tar, and so does the
`d` flag:

```text
------d-------e------- share-again/tools/run.sh
```

bsdtar records these by default when it creates an archive, and applies them when it runs as
root or is given `--preserve-permissions`; the explicit options make the intent visible.

The recipe preserves whatever the archiver records. Run as root, GNU tar records attributes of
every namespace, ACLs, SELinux labels, ownership and all permission bits, and bsdtar adds file
flags. An archive made by an ordinary user lacks `trusted.*` attributes and any file that user
cannot read. Extracting as root is needed to restore other users' ownership, `security.*` and
`trusted.*` attributes, and the `i` and `a` flags. As an ordinary user, both archivers restored
the ACLs, the user attributes and the permission bits, and bsdtar restored the `d` flag.

Remanence sees one file, so it records one checksum and one catalog entry for the tar file, not
one per file inside it. To recover a single file, restore the tar file and extract that member
from it. To check a restore, compare the tar file's SHA-256 with the `file_sha256` in the build
report, then inspect the unpacked tree with `getfacl`, `getfattr` and `lsattr`.

A filesystem image, or the output of a filesystem's own dump tool, can be stored in the same
way. Making one usually needs root, and that variant was not run. The GNU tar and bsdtar
variants were run end to end as an ordinary user; the steps that need root were not.

### 11.3 Samba shares used from Windows

Samba keeps two kinds of Windows metadata on a Linux share. With `store dos attributes`
enabled, it writes each file's DOS attributes into `user.DOSATTRIB` (see the `smb.conf` manual
page). Remanence keeps that attribute like any other user attribute; a test value came back
byte for byte. With the `acl_xattr` module, Samba writes each file's NT ACL into
`security.NTACL` (see the `vfs_acl_xattr` manual page), and Remanence does not keep it.

An NT ACL names users and groups by their SIDs. The `vfs_acl_xattr` manual notes that the
mapping between those SIDs and the server's users and groups must stay consistent for a stored
ACL to keep its meaning. A restored NT ACL is therefore useful only on a server in the same
domain, or in a domain whose SIDs can be mapped. `samba-tool ntacl changedomsid` rewrites the
domain SID in a file's ACL for that purpose. Anywhere else, the restored entries name accounts
that do not exist.

On the server, as root, dump the NT ACLs together with the POSIX ACLs. By default Samba keeps
the POSIX ACLs in step with the NT ACLs and evaluates both (the `acl_xattr:ignore system acls`
option), so both should be restored. `getfattr` lists `security.NTACL` only when the pattern
names it:

```sh
cd share
getfacl -R --numeric . > ../share.acl
getfattr -R -d -m '^security\.NTACL$' . > ../share.ntacl
cd ..
rem archive build --inputs share share.acl share.ntacl --out share-smb.rem-object > build.json
```

To restore, as root, from the top of the restored tree on the new server:

```sh
setfacl --restore=share.acl
setfattr --restore=share.ntacl
```

`samba-tool` reads and writes an NT ACL as SDDL text (Security Descriptor Definition Language,
the Windows text form of an ACL), one file at a time. `<FILE>` is a path on the server, and
`<SDDL>` is the string that `get` printed:

```sh
samba-tool ntacl get --as-sddl <FILE>
samba-tool ntacl set '<SDDL>' <FILE>
```

From a client, `smbcacls` prints a file's NT ACL over SMB, and its `--set` option applies one.
`<SERVER>` and `<SHARE>` name the Samba server and share, and `<PATH>` is the file's path within
the share:

```sh
smbcacls //<SERVER>/<SHARE> <PATH> --sddl
```

A share configured with `acl_xattr:security_acl_name = user.NTACL` keeps its NT ACLs in a user
attribute instead. Remanence captures and restores that attribute like any other user
attribute. The `vfs_acl_xattr` manual warns that any user can then overwrite the ACL, and the
SID caveat above applies equally.

The recipe preserves the NT ACLs, and with the `getfacl` dump the POSIX ACLs and ownership. It
cannot make the SIDs resolve in another domain. Dumping and restoring `security.NTACL` needs
root on the server, `samba-tool` needs the server's Samba configuration, and `smbcacls` needs
access to the share. After a restore, `samba-tool ntacl get --as-sddl <FILE>` shows whether the
entries resolve to accounts. Only the `user.DOSATTRIB` round trip was run. None of the NT ACL
steps was run, because each needs root on a Samba server, its configuration, or a live domain.

### 11.4 SELinux labels

On a different host, the usual answer is not to archive labels at all, and to relabel the
files after the restore. `restorecon` sets each file's label from the target system's policy,
which is normally what you want on another machine years later. `<PATH>` is the restored tree
in its final location:

```sh
restorecon -R -v <PATH>
```

If the tree needs a type other than the policy's default for that location, add a rule first.
This example gives the tree the type that the reference policy uses for web content:

```sh
semanage fcontext -a -t httpd_sys_content_t '<PATH>(/.*)?'
restorecon -R -v <PATH>
```

Archive the original labels only when they are evidence, and keep them as data rather than
apply them:

```sh
cd share
getfattr -R -d -m '^security\.selinux$' . > ../share.selinux
```

Relabelling produces the labels that the target policy assigns, not the original ones. To put
the original labels back on the same system, use recipe 11.2 with GNU tar's `--selinux` option,
run as root. `restorecon` and `semanage` need root. After a restore, `ls -RZ <PATH>` shows the
labels. None of these steps was run: the test system had no SELinux, and `restorecon` was not
installed.

### 11.5 File capabilities

On a new host, the usual answer is to reinstall the package that installed the program. The
package's installation scripts set the capability again. `<PACKAGE>` is the package name; on
Debian and Ubuntu:

```sh
apt-get install --reinstall <PACKAGE>
```

and on Fedora:

```sh
dnf reinstall <PACKAGE>
```

To record capabilities for a tree that no package installs, dump them with the tree:

```sh
cd share
getcap -r . > ../share.caps
```

Each line of the dump is a path followed by its capabilities. On a host where `ping` carries a
capability, `getcap /usr/bin/ping` prints:

```text
/usr/bin/ping cap_net_raw=ep
```

To restore, as root, from the top of the restored tree:

```sh
while read -r path caps; do setcap "$caps" "$path"; done < share.caps
```

The loop does not handle paths that contain spaces. For those, dump
`getfattr -R -d -m '^security\.capability$' .` instead, and restore it with
`setfattr --restore`, as in recipe 11.1.

The recipe preserves file capabilities, and nothing else. Setting a capability needs root,
and `getcap -r .` on the restored tree shows the result. The `getcap` dump was run as an
ordinary user: the example tree has no capabilities, so its dump is empty, and the `ping` line
above is from the test host. The restore loop and the package reinstall need root and were not
run.

### 11.6 Compliance flags

To record the flags with the tree:

```sh
cd share
lsattr -R . > ../share.flags
cd ..
rem archive build --inputs share share.flags --out share-flags.rem-object > build.json
```

For the example tree, `share.flags` holds:

```text
--------------e------- ./tools

./tools:
------d-------e------- ./tools/run.sh

--------------e------- ./reports

./reports:
--------------e------- ./reports/survey.csv
```

To restore, extract the object and run the loop below from the top of the restored tree. The
`for` line names the flags to reapply. Run it after every other restore step, because an
immutable file refuses later changes to its permissions and attributes:

```sh
for flag in a d i; do
  awk -v f="$flag" 'NF > 1 && $1 ~ /^[-a-zA-Z]+$/ && index($1, f) { sub(/^[^ ]+ /, ""); print }' share.flags |
  while IFS= read -r path; do chattr "+$flag" "$path"; done
done
lsattr -R reports tools
```

`lsattr` shows the `d` flag back on the script:

```text
--------------e------- reports/survey.csv
------d-------e------- tools/run.sh
```

The recipe preserves the flags you name in the loop. `lsattr -R` does not list the top
directory itself; add `lsattr -d .` to the dump if it carries flags. The loop handles paths
with spaces, but not paths that contain a newline. `lsattr` and `chattr` work only on
filesystems that support these flags, such as ext4, XFS and Btrfs. Setting `i` or `a` needs
root. The recipe was run end to end as an ordinary user. The tree had only the `d` flag, so
the loop set nothing for `a` and `i`; reapplying those needs root and was not run.

### 11.7 Systems, integrity-checked hosts and software state

Cases 3, 4 and 8 concern a host to be rebuilt, a host that uses IMA or EVM, and software that
keeps state in `trusted.*`. For each, a system backup or imaging tool is the right primary tool.
Remanence can hold that tool's output as one opaque file for the long term, as recipe 11.2
shows. Rebuilding a working system also needs more than files, such as the partition layout
and the boot loader, and those tools are built to handle it.

IMA and EVM data is checked against keys that the host trusts. On another host the files are
normally signed or measured again under that host's keys. An image of the original host keeps
the data as it was, for that host.

`trusted.*` state belongs to the software that wrote it. When what matters is the data, read
it through that software: archive a GlusterFS volume through a client mount rather than from a
brick, and an overlay through its mounted, merged directory. When the software's own state
matters, use the software's own backup method.

A container image (case 5) is best kept in its own format. `podman save` and `docker save`
write an image as one archive file, and an OCI (Open Container Initiative) image layout is a
directory that recipe 11.2 can pack. Remanence then stores the result as one file.

For forensic or legal preservation (case 6), a forensic imaging tool produces an image file and
its checksum. Remanence stores the image as one opaque file and records its own SHA-256 beside
it. The dumps of recipe 11.1 can be archived with it as a copy that a person can read without
the imaging tool.

None of the steps in this section was run.

## In plain terms

Remanence keeps a file and the notes attached to it, because they stay true wherever the file
goes. It does not keep the locks that one machine put on the file, because on another machine
years later they would fit nothing, or open the wrong doors. When the locks matter, write them
down as a document of their own, or keep the machine's own backup, and Remanence will store
either one faithfully.

## See also

- [Extended attributes and file metadata](reference-extended-attributes.md): the exact
  capture and restore behaviour, and every report field.
- [REM-OBJECT core specification](../specs/publication/rem-object-core-1-specification.md):
  extended-attribute preservation, the object metadata inventory, and the hazards of restoring
  attributes.
- [REM Implementation and Operations Guide](../specs/publication/rem-implementation-guide.md),
  chapter 4, "Restoring onto a host".
- [Why Remanence?](why-remanence.md): what Remanence is for, and what it does not do.
