# Changelog

Notable changes to Remanence and its published formats. The format
specifications carry their own revision histories; entries here are
per-release summaries.

## v1.2.0 — 2026-07-22

Metadata-preservation model, extension mechanism, and catalog-less recovery
completeness. Archived: DOI pending (concept DOI
[10.5281/zenodo.21425126](https://doi.org/10.5281/zenodo.21425126)).

- RAO object format: a **portable core** (`user.` namespace, applied on
  restore by default) and a **carry-only extension tier** (privileged
  namespaces and registered extensions, applied only under explicit
  operator policy). New `ext` extension container (reverse-DNS naming,
  ignore-unknown, preserve-on-rewrite, ancillary-by-definition) and an
  object-level metadata **inventory** that lets a holder screen what an
  object discloses without decoding its entries — with a Verifier
  obligation that the inventory is exact. Attribute names carry a canonical
  namespace wire form. Ingest now captures the portable core by default and
  reports dropped namespaces; a full-fidelity opt-in remains.
- Reference implementation of the above in the object writer/reader, plus a
  plaintext-disclosure Security Considerations section and preservation-
  vocabulary labels for the four fixity digests. All additive: existing
  objects are byte-identical and tolerated by prior readers.
- REM-PARITY: a deterministic **overlay tie-break** for structurally
  discovered `parity_map` files during bare-tape recovery (integrity gate →
  overlay-then-digest walk cross-check → rank by scope), resolving the last
  open recovery item; the attested prefix is now the largest validating
  scope. Reference Scanner implementation included.
- Specification clarity hardenings from external technical review: explicit
  `roundup()` definition, hardlink-primary selection expressed over emitted
  entries, AEAD tag-placement made explicit, the partial-file-restore
  streaming release contract, and CBOR payload-extent rules.
- Publication test vectors regenerated (archive SHA
  `f4e4331c14e67c059d1292f54e14efd8408c7d41364d2dba7f8e7567aa16c2a6`,
  superseding `32fe2a79…`): prior entries byte-identical, additive vectors
  for the metadata/extension objects, the negative cases, and the
  multi-`parity_map` tie-break — independently re-derived by a second
  implementation.

## v1.1.1 — 2026-07-22

Security hardening of the extended-attribute restore path. Archived: DOI
[10.5281/zenodo.21488792](https://doi.org/10.5281/zenodo.21488792)
(concept DOI [10.5281/zenodo.21425126](https://doi.org/10.5281/zenodo.21425126)).

- Extended-attribute restore is now allow-listed by default to the `user.`
  namespace across every restore surface (`rem archive extract`, `dump`,
  tape-restore, and the `rao-recover` disaster-recovery reader). Privileged
  namespaces (`security.*`, `system.*`, `trusted.*`) are skipped and
  reported by name — never applied — unless an operator opts them in with a
  repeatable `--xattr-namespace` flag. Attribute values are never logged.
- Attribute writes are no-follow: they never traverse a symbolic link at the
  final path component, and malformed or over-limit attribute names are
  rejected before reaching the OS. A genuine application failure is surfaced;
  only an unsupported-filesystem condition is a benign skip. One stderr
  warning is emitted per restore when attributes are skipped by policy.
- Restore reports (including the `rao-recover` summary) list both skipped and
  applied-privileged attribute names, so an opt-in's effect is auditable and
  a disaster-recovery restore never silently drops metadata.
- REM-PARITY companion note: RAO Object Format §12.10 restores the
  extended-attribute restore protections to requirement (MUST) strength;
  see the specification's revision history. New `reference-extended-
  attributes.md` documents capture/restore behavior and the safety of the
  standard-`tar` recovery path (which cannot reapply attributes).

## v1.1.0 — 2026-07-22

Batched checkpointing and the field-validated throughput stack. Archived:
DOI [10.5281/zenodo.21485573](https://doi.org/10.5281/zenodo.21485573)
(concept DOI [10.5281/zenodo.21425126](https://doi.org/10.5281/zenodo.21425126)).

- ONE write mode: batched checkpointing is now the only write path, for
  parity and non-parity pools alike. Objects get immediate filemarks;
  periodic synchronizing barriers prove durability, write a cumulative
  on-tape checkpoint bootstrap edition, and advance the commit point.
  Callers observe WRITTEN → CHECKPOINTED acknowledgment; enumeration
  never surfaces non-checkpointed objects. The per-object write mode and
  its configuration key are removed.
- Parity epochs are explicit ordinal ranges (bare-counter ids); short
  epochs are legal at any checkpoint boundary; `FINAL_PARTIAL_EPOCH`
  marks only the terminal epoch. Reconstruction locates sidecars by
  range containment.
- The bootstrap directory ceiling is an admission-time refusal with
  worst-case-row headroom reserved at batch open: a full tape seals at
  its last checkpoint and placement rolls — never a mid-object failure.
- Spool overlap (opt-in `append_staging_mode=overlap`): bounded
  receive-to-tape overlap with an 8 GiB ring; the 64 GiB object-size cap
  is removed (100 GiB object written end-to-end at rate in field
  validation).
- Lazy dismount: session close no longer unloads the drive
  (close 165 s → 16 ms on iron); idle eviction after
  `drive_idle_unload_seconds`.
- Pipelined tape I/O validated on physical LTO-9 (MSL3040 window,
  2026-07-20/21): sustained ~285 MB/s single-drive feed, ~640 MB/s
  dual-drive aggregate, 200 MiB/s bit-perfect read-back; ranged-read
  fast path fixed (6.4 MiB/s → full rate).
- REM-PARITY 1.0 draft revised (spec Appendix D): normative ordered
  persistence + synchronizing barrier; batched commit discipline with
  staged-record semantics; the attested prefix and the bare-tape tail
  taxonomy (attested / unattested / truncated) with salvage rules;
  matching reference fixes (barrier failure poisons the writer; the
  catalog-less scanner tolerates and reports truncated tails instead of
  aborting).
- Startup replay repairs partially projected checkpoint records
  (journal-authoritative); daemon catalog copies are rebuilt from
  post-projection state so committed-object responses always carry
  their copies.

## v1.0.1 — 2026-07-19

Maintenance and capability release. Archived: DOI
[10.5281/zenodo.21438555](https://doi.org/10.5281/zenodo.21438555)
(concept DOI [10.5281/zenodo.21425126](https://doi.org/10.5281/zenodo.21425126)).

- Drive-targeted reads: a read session can now be opened on the tape
  currently loaded in a chosen drive (`DriveTarget`), with the same identity
  proof, pool guard, and readiness path as every other session open. With
  the existing load-to-drive command this makes cross-drive read-back
  checks — "read this tape in the other drive" — performable end to end,
  and supported `rem` verbs are provided for the composition.
- Audit query service: `QueryAudit` is now served over both transports,
  streaming a time window of the append-only audit log with exact filters
  (session, operation, event kind), plus a `rem audit query` verb. Replay
  streams record bodies with bounded memory; the retained per-segment
  bookkeeping is proportional to segment count and is documented in code.
- Telemetry at the moments that matter: TapeAlert and error-counter
  snapshots are now also recorded when an append or read fails, not only at
  clean session close.
- Two-library safety: robotics requests against a discovered but
  non-operated library are rejected, preventing bay aliasing between
  co-resident libraries.
- Verification housekeeping: the 2026-07-18 tautology audit deleted three
  proof-free theorems and sharpened the remaining claims. Zenodo DOIs are
  recorded in the README and citation metadata; internal working files are
  no longer tracked.

## v1.0.0 — 2026-07-18

First publication release. Archived: DOI
[10.5281/zenodo.21425127](https://doi.org/10.5281/zenodo.21425127)
(concept DOI [10.5281/zenodo.21425126](https://doi.org/10.5281/zenodo.21425126)).

- **RAO Format Specification 1.0** (`specs/publication/`): the archival
  object format — a byte-deterministic, chunk-aligned POSIX pax tar
  container with per-file SHA-256 identities, a CBOR manifest, closed-form
  byte-range addressing, and an encrypted representation sealing each
  object under a fresh key wrapped to multiple recipient public keys
  (HPKE). One encryption scheme; the header's `format_version` byte is
  `2`, with `1` permanently reserved from an unpublished pre-release
  lineage.
- **REM-PARITY Tape Format Specification 1.0**: filemark-delimited object
  layout, Reed–Solomon sidecar parity, self-describing bootstrap blocks,
  and fully specified catalog-less recovery from bare tape.
- **Pinned test vectors** with the archive's SHA-256 printed in both
  specifications, verified by an independent from-scratch Python
  implementation of the read paths.
- **A plain-language companion**, *The Remanence Formats, Explained*
  (`specs/publication/formats-explained.md`).
- Reference implementation: library discovery and robotics, pipelined
  fixed-block tape I/O, the object and parity formats, a rebuildable
  SQLite catalog, a gRPC daemon, operator CLIs, and the standalone
  `rao-recover` disaster-recovery binary.

The implementation itself remains pre-alpha (0.0.x): interfaces may
change; the published on-tape formats are stable as specified.
