# Comparison with the expected outcomes

This compares the blind decisions (`decisions.json`, SHA-256
`a0629383f7f037336c4089e25db775b56cbdcff9a2020955dd6b572703406ff7`) with each case's `expected.json`. The
repository's `fault-map.json` files are identical to the blind case files,
and `decide` on them gives the same decisions under the case-id mapping, so
the blind decisions apply to the real ids unchanged.

How each key was compared:

- Each expected key is read as its sentence says. The interpretation used
  for every key is in `comparison.json`, beside the value it was compared
  with.
- Keys named `informative` or `note`, and the one case with
  `"pinned": false`, are not compared. How they relate to my decisions is
  recorded for each.
- The copy named in an expectation such as "recovered (tail copy)" is
  informative. It compares as "recovered".
- Three pinned keys have no field in the decisions schema:
  `object_sha256_matches`, `losses_per_stripe` and `records`. They were
  computed after the decisions, from the damaged tape and the blind trace,
  in the same way as `bytes_match`.
- A result of agree-open means that every pinned key agrees, at least one
  of them through a reading of an aspect I marked undecided. The gap in the
  text still stands.

## Per case

| Case | Blind id | Result | Pinned keys | My decision |
| --- | --- | --- | --- | --- |
| replicas-a-b | case-01 | agree | `inventory` "replica C", `degraded` true | Inventory from C only, degraded (§8.5). |
| object-head | case-02 | agree | `recovered`, `object_sha256_matches` | (1, 0) recovered; the Object reassembled with the recovered block has the pinned SHA-256. |
| sidecar-primary-and-footer | case-03 | agree | `epoch_0` "recovered through the directory", `epoch_1` recovered | Both recovered; epoch 0 by the directory-assisted tail rescue (§13.3 step 3). |
| editions-conflict | case-04 | agree | `error` TerminalIndexReplicaConflict | Scanner error TerminalIndexReplicaConflict (§8.5). |
| replicas-all | case-05 | agree-open | `outcome`, `walk_classes`, `object_identity`, `terminal_authority_recovered` | BotStructuralRecoveryRequired; files 0–3 as named; files 5 and 7 IndexSeparationExtent; files 4, 6 and 8 undecided, and the expectation's "terminal components" is my first reading (GAPS B-2). |
| replica-c-payload | case-06 | agree | `inventory` "an agreeing valid replica" | Inventory from A or B. The informative note matches my undecided `scanner.degraded` (GAPS B-6). |
| filemark-after-b | case-07 | agree | `inventory` "replica A accepted", `degraded` true | Inventory from A only, degraded; B lacks its trailing filemark and C is shifted. |
| sidecar-primary | case-08 | agree | `epoch_0` "recovered (tail copy)", `epoch_1`, `object_sha256_matches` | Both recovered; epoch 0 from the tail copy through the footer (§13.3 step 1); Object SHA-256 matches. |
| bootstrap-hinted | case-09 | agree | `inventory` "as the image's expected.json" | Inventory from A, B or C through the hint path (GAPS B-1); its rows are the image's files 0–3 with blocks 1, 4, 7, 3 and starts 0, 2, 7, 15, as `images/a4-minimal/expected.json` lists. |
| walk-sidecar-isolation | case-10 | agree | `walked_map_validated`, `epoch_0` SidecarMetadataUnavailable, `epoch_1` recovered | The walked map validates (§13.1); epoch 0 metadata-unavailable, epoch 1 recovered (§12.5). |
| walk-directory-rescue | case-11 | agree | `recovered` | (1, 0) recovered through the walk and the directory. The informative note matches the trace. |
| parity-and-data | case-12 | agree | `recovered` | (1, 2) recovered. The note (parity (0, 1) at LBA 10) matches the shards used. |
| bootstrap-unhinted | case-13 | agree | `error` NoBootstrapFound | Scanner error NoBootstrapFound (§8.4.1, §15). |
| short-epoch-recoverable | case-14 | agree | `recovered`, `object_sha256_matches` | (1, 1) recovered with the implicit zero as a trusted shard; Object SHA-256 matches. |
| sidecar-footer | case-15 | agree | `epoch_0` "recovered (primary copy, step 2)", `epoch_1` | Both recovered; epoch 0 from the primary copy (§13.3 step 2). |
| replica-c | case-16 | agree | `inventory` "an agreeing valid replica", `degraded` true | Inventory from A or B, degraded. |
| separation | case-17 | agree-open | `inventory_unaffected`, `verifier_reports_separation`, `verifier_suffix_complete` false | Inventory unaffected; A-B reported unreadable. My verifier result is undecided between degraded and an error (GAPS B-4); both readings say not complete, which §10.6 decides. |
| editions-survivor | case-18 | agree | `inventory` "the agreeing survivors A and C", `degraded` true | Inventory from A or C, degraded. |
| burst-m | case-19 | agree | `recovered`, `losses_per_stripe` [2, 2] | Both recovered; two erasures in each stripe (the failed block and one parity shard), from the blind trace. |
| filemark-prefix | case-20 | disagree (c) | `records` 38, `every_replica_invalid`, `walk`, `terminal_authority_recovered` false | Three keys agree. `walk` disagrees: I classify tape file 1 as a failed classification; the expectation says an 11-block Object candidate. Class (c); see below. |
| burst-m-plus-one | case-21 | agree | `error` Unrecoverable, `stripe` 0, `lost` 3, `limit` 2 | Same values (GAPS B-12: `lost` read as the stripe's erasures). |
| bootstrap-wrong-scheme | case-22 | agree | `error` SchemeMismatch | (1, 0) SchemeMismatch when the acquired index is pinned against the supplied scheme (§13.3); reached through the hint path (GAPS B-1). |
| short-epoch-burst | case-23 | agree | `error` Unrecoverable, `stripe` 1, `lost` 3, `limit` 2 | Same values; the implicit zero is not counted (GAPS B-12). |
| parity-map-both | case-24 | agree | `inventory_unaffected`, `recovered` | Inventory from A, B or C, not degraded; (1, 0) recovered. |
| parity-map-and-sidecar | case-25 | not compared (pinned: false) | none (`pinned`: false) | Not compared. `epoch_1` recovered agrees. The informative note matches GAPS B-7; my blind decision for epoch 0 is SidecarMetadataUnavailable, one of the two readings. |

## The disagreement

**filemark-prefix (case-20), key `walk`: class (c), the text does not
decide it.**

The removed filemark merges the 4-block Object and the 7-block sidecar into
one 11-block tape file 1. Its head is the Object's first block, and its last
block is the sidecar's footer, which parses. The footer's total, 7,
differs from the measured length, 11.

Two readings survive:

1. Mine: the last block of tape file 1 parses as a sidecar footer, so item 6 recognises the file as a sidecar; its total (7) differs from the measured length (11), a hard error, and the Scanner reports the failed classification and moves to the next tape file. Item 7 does not apply, the walk cannot produce a valid map, and its error is FilemarkMapReconstruct.
2. The expectation's: the hard error is scoped to rung 6, so no rung classifies the file; item 7 then classifies it by elimination as an 11-block Object candidate, and the walk continues.

The expectation took the second reading (Object candidate by elimination). Section 12.3 does not define whether a rung 'recognises' a file when its magic and footer parse or only when it classifies the file, and 'reports the failed classification' can name the failure of the rung or of the file. The sentences
are:

- Section 12.3: "Items 1 to 6 recognise kinds that are disjoint by magic; items 5 and 6 are two ways of recognising a sidecar."
- Section 12.3: "Item 7 applies to a tape file that none of items 1 to 6 recognises."
- Section 12.3: "If it parses as a sidecar footer, the footer's total MUST equal the measured block count, and the Scanner verifies the tail header copy against the footer, field for field."
- Section 12.3: "In items 2 through 6, a count-mismatch “hard error” is scoped to that rung and that tape file's classification: the Scanner reports the failed classification and continues the walk with the next tape file."

The first reading leans on "continues the walk with the next tape file"
and on recognition "by magic". The second leans on "scoped to that rung".
No sentence excludes either. The other three pinned keys of this case
agree: 38 records, every replica invalid, and terminal authority not
recovered. GAPS entry B-9 records the gap.

## Summary

| Result | Cases |
| --- | ---: |
| agree | 21 |
| agree-open | 2 |
| disagree (a) | 0 |
| disagree (b) | 0 |
| disagree (c) | 1 |
| not compared (`pinned`: false) | 1 |

No decision was found wrong under the text, so none is revised.
`decisions.json` stands as the blind record, and `DECISION-LOG.md` has no
rows.

Seven expectations rest on a reading of a gap this implementation recorded
before it saw them:

- case-09 (`bootstrap-hinted`) and case-22 (`bootstrap-wrong-scheme`) need
  the Section 8.4 hint path (B-1).
- case-05 (`replicas-all`) needs the permission that lets a footer
  establish a terminal type (B-2).
- case-19 (`burst-m`), case-21 (`burst-m-plus-one`) and case-23
  (`short-epoch-burst`) need `lost` to count the stripe's erasures, the
  failed block included (B-12).
- case-20 (`filemark-prefix`) needs the fall-through reading of Section
  12.3 (B-9).

In each case the expectation and my blind decision took the same reading,
except case-20. The informative notes of case-06 and case-25 describe the
same gaps as B-6 and B-7.
