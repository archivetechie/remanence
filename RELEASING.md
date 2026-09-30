# Releasing Remanence

Releases are cut only from a clean `main` after the CI, format-vector, audit,
MSRV, and documentation gates pass. The release tag must be annotated and
cryptographically signed by a key GitHub recognizes as verified. The release
workflow checks the tag object, its `main` ancestry, and a successful CI run for
the exact tagged commit before publishing. It also requires the tag to be
exactly `v<workspace.package.version>`, so the release name and the versions
reported by its binaries cannot diverge. Existing historical unsigned tags are
not rewritten.

A `v*` tag runs the release workflow, rebuilds the test and lint gates, and
creates a GitHub release containing checksummed Linux operator binaries and a
separate static-musl `rem-recover` archive. The recovery archive is separate so
an operator can preserve it with key escrow without depending on a full
Remanence installation. Each archive includes Remanence's Apache-2.0 license
and a generated inventory containing the full selected license texts for its
third-party Rust dependency graph.

Format documents are released independently. A software release must not
publish or resolve a reserved format DOI; a format deposit is made only when its
document, vector archive, checksum, and compatibility statement agree.

## Depositing a format revision

A deposit on Zenodo is permanent. A published record cannot be deleted with the project's token, and a
mistake in a deposited text stays public. The tool `tools/zenodo_deposit.py` exists so that the
irreversible step is the last one and every earlier step can be repeated. This section is the order of
work. The design that explains the choices is kept with the project's working documents.

Format records are deposited as one set: the three specifications, the companion and the vector
dataset. The documents cite each other and the dataset by DOI, and a reserved DOI does not resolve until
its record is first published. A specification published alone would cite DOIs that do not yet exist.
The tool therefore refuses a plan that does not cover all five, and it publishes them in a fixed order:
the dataset, the companion, REM-OBJECT, REM-ENCRYPT, and REM-PARITY last.

### Before the day

- Each record exists as an unsubmitted draft on Zenodo, with its version DOI reserved. The deposited
  text cites that DOI as the DOI of the revision.
- The records file (`release/zenodo-records.json`) holds each record's metadata and file list. The tool's
  `stage-metadata` mode writes the metadata to the drafts and reads it back. A draft without files cannot
  be published, so this is safe to do in advance.
- `python3 tools/zenodo_deposit.py probe` checks the tool's assumptions about Zenodo's responses against
  a published reference record and the five drafts. It changes nothing. Run it before the day and on it.
- The draft vector archive is built from a clean tree (`tools/build_draft_vector_archive.py`), verified
  (`tools/verify_draft_vector_archive.py`), and its digest is quoted in the texts that cite it.
- The GitHub-to-Zenodo integration is switched off for this repository (see "The webhook"). The tool
  refuses to publish without an explicit statement that it is.

### On the day

1. Start from a clean `main` with a green CI run.
2. Promote the preparing copies to `specs/publication/`, set each document's date to the date the text
   is finalised, run the preservation gate, and commit locally. Do not push.
3. Tag the last commit before the promotion with the annotated tag `specs-pre-draft5`, which is where the
   previous publication texts stay retrievable.
4. `python3 tools/zenodo_deposit.py plan`. It checks the promoted texts against the records file, requires
   a clean tree, hashes every file, and prints one plan digest. Read the list of external URLs it prints
   and confirm that each still resolves.
5. `stage-metadata`, then `stage-files`. Both read everything back and compare it with the plan. The
   drafts are still private.
6. Read the five drafts in the Zenodo web interface. A staged draft is one click from public there, so
   leave the drafts alone except to read them.
7. `publish --plan-digest <digest> --publish-permanently --webhook-off-confirmed`. The tool checks all
   five records before the first irreversible call, reads each one fresh before it publishes it, and
   compares the account's record list after each. If it stops, run it again with the same plan: it
   verifies the records already public and completes the rest. It cannot change a published record.
8. `record`. It downloads every published file through the records API, compares SHA-256 and MD5,
   and only then appends the lines to `specs/publication/DEPOSITED.sha256`.
9. Commit the recording and the repository statements that say what is deposited (the READMEs,
   `CITATION.cff`, the changelog and the documentation), then push. Nothing is pushed before step 8
   succeeds, so an aborted deposit leaves `main` as it was.

The tool verifies remote files by size and MD5 at publication, because those are all the API reports.
The SHA-256 is verified locally before the files are staged and again on the downloaded bytes.

### If something goes wrong

- If the publish stops part-way, the state file the tool writes (outside the repository) says what is
  public. Record that, and complete the rest with the same plan. Never edit a text that has been
  deposited, and never try to publish a different set of bytes under the same version string.
- If a deposited text turns out to be wrong, the fix is a new revision with a new version string,
  deposited as a new version of the same record.

### Later revisions

A later revision of an already deposited document is a new version of its record. Zenodo's `newversion`
action creates a new draft from the published record, and that draft's version DOI is known only when
it is created. The text of the new revision cites that DOI, and the records file names the new draft.
The same order applies.

### The webhook

Zenodo can archive a repository's GitHub releases automatically. When it is enabled for this
repository, publishing a GitHub release creates a software record by itself, and that record cannot be
removed with the project's token. Format deposits are made by the tool and never by a release, so the
integration must be off for this repository when format records are deposited.
