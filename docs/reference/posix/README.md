# POSIX pax and ustar

This note is for contributors who work on the REM-OBJECT tar layout. The
plaintext body of a REM-OBJECT object is a POSIX tar archive in the pax
interchange format. ustar is the fixed 512-byte tar header; a pax extended
header is an extra record before it that carries fields ustar cannot hold. In
a REM-OBJECT object each entry is a pax extended header followed by a ustar
header, and the pax header carries long paths and Remanence's own
`REMANENCE.*` records. The format is defined on the `pax` utility page of
POSIX (IEEE Std 1003.1).

- The REM-OBJECT specification cites the 2017 edition, POSIX.1-2017:
  <https://pubs.opengroup.org/onlinepubs/9699919799/utilities/pax.html>
  (reference `[POSIX-PAX]` of the
  [specification](../../../specs/publication/rem-object-core-1-specification.md)).
- The current edition is POSIX.1-2024:
  <https://pubs.opengroup.org/onlinepubs/9799919799/utilities/pax.html>

The repository does not include a copy of the standard. A copy downloaded into
this directory as an `.html` file is ignored by Git (`.gitignore` lists
`docs/reference/posix/*.html`), so it stays local. Cite the edition rather
than the digest of a downloaded file: the page served at an address is not
guaranteed to stay byte-identical.

The [glossary entry for `rem-object-v1`](../../reference-glossary.md#rem-object-v1)
describes how an object uses the format.
