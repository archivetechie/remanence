# Remanence draft conformance vectors

These are the review vectors that accompany the draft revisions of the Remanence format
specifications:

- REM-PARITY 1.0.0-draft.5, the tape layout of generation 2;
- REM-OBJECT Core Format 1.0.0-draft.4;
- REM-ENCRYPT 1.0.0-draft.4.

They are review material. The specifications are not final until they freeze, which is planned for
31 July 2027, and the vectors that freeze with them replace these. Where a vector and the text of
the document it accompanies disagree, the document governs, and the disagreement is an error to
report against the vector.

The archive names document versions by their version strings and contains no specification text,
so that a specification can cite this archive by its hash and the archive does not depend on the
text.

## What is in it

| Directory | What it holds |
| --- | --- |
| `rem-object/` | The REM-OBJECT and REM-ENCRYPT positive objects, the negative cases and the known-answer files for the encrypted envelope. |
| `rem-object-supplement/` | Further REM-OBJECT cases that accompany draft.4 and are not yet part of the frozen set. |
| `rem-parity-generation-2/` | The REM-PARITY generation-2 vectors: the whole-tape images, including the byte streams of every tape file; the damage matrix, with its fault maps and the outcome each case expects; the resume vectors; the negative vectors; and the terminal-index vector sets at each legal block size. |
| `rem-parity-second-implementation/` | A second implementation of the generation-2 rules, written from the specification text alone, and the decisions it reaches on the vectors. |
| `tools/` | The verifiers that ship with the vectors. |

`MANIFEST.tsv` lists every member with its size and SHA-256. `CHECKSUMS.sha256` is the same digests in
the form `sha256sum -c` reads. `CLAIMS_TO_ARTIFACTS.tsv` says which command checks which claim, and
which members it covers.

## Checking the archive

Before running `verify.py`, compare the archive's SHA-256 with the digest recorded for it.
`verify.py` is a program the archive supplies.

Extract it and run `python3 verify.py` in the extracted directory. It checks every digest and runs
each command in `CLAIMS_TO_ARTIFACTS.tsv`. It needs Python 3.12 and the standard library, and it uses
no network. `tools/requirements-rem-object-independent.txt` lists the one package that the
independent REM-ENCRYPT verifier needs.

## Which tapes and documents these cover

Generation 1 and generation 2 are different tape layouts, and neither reads the other's tapes. A
generation-2 reader cannot read a generation-1 tape, and generation-1 tooling cannot read a
generation-2 tape. REM-PARITY 1.0.0-draft.5 and the vectors here describe generation 2. A tape
written under generation 1 is governed by the text of REM-PARITY 1.0.0-draft.2, and by the
generation-1 archive `remanence-test-vectors.tar`, which is deposited unchanged in the same record as
this archive. The generation-1 archive contains vectors that generation-2 code must not accept.

## What these vectors do not cover

- Tapes written by a physical drive. The vectors are generated, and the supervised physical-media
  validation is still open.
- The fuzz campaigns for the terminal structures.
- A second implementation from a party other than the project. The one included is technical
  independence: it was written from the text without the reference implementation, and it is not the
  independence of a second institution.

## Licence

CC0 1.0. One file reproduces third-party material: `rem-object/kats/xwing-draft10-kat.txt` is vector 1
of Appendix C of draft-connolly-cfrg-xwing-kem-10, an IETF Internet-Draft, and its first line says so.
The IETF Trust's terms, not CC0, govern that draft.
