# Resume comparison

This compares the second implementation's blind resume decisions
(`resume-decisions.json`) with the pinned resume expectations
(`../tape-images/resume/expected-cases.json`) and the reference's resumed tapes
(`../tape-images/resume/MANIFEST.tsv`). The decisions were written under opaque
ids before either file was read. The mapping is at the end.

## Per case

| Case | Opaque id | Result | Remarks |
| --- | --- | --- | --- |
| resume-open | resume-02 | agree | Accepted; re-reads ordinals 4–5 at LBAs 15–16; appends at LBA 18 as tape file 4; the epoch-1 sidecar [4, 8) is tape file 5 at LBA 21, 7 blocks; recovery row tape file 4, 2 blocks. Every tape file and `ALL` match the reference's resumed tape in size and SHA-256, and EOD is 29. Both implementations find the resumed tape equal to one uninterrupted session. |
| resume-closed | resume-05 | agree | Accepted; nothing to re-read (W = T = 4); appends at LBA 15 as tape file 3, superseding the torn records. Every tape file and `ALL` match the reference, and EOD is 26. |
| resume-w-greater-than-t | resume-07 | agree | `ResumeAppend` at §14 step 2, before any read. |
| resume-full-unprotected-epoch | resume-03 | agree | `ResumeAppend` at §14 step 2 (T − W = 4 is not below S × k). The expectation's "before any read or write" means no read at or beyond the append point and no write. This implementation reads the bootstrap at LBA 0 to learn S and k, which is consistent with that. Where a Resumer gets S and k is a text gap (GAPS.md E-2). |
| resume-noncontiguous-sidecars | resume-06 | agree | `ResumeAppend` at step 2, before any read. |
| resume-nonconsecutive-epochs | resume-04 | agree | `ResumeAppend` at step 2, before any read. |
| resume-final-object-not-at-t | resume-08 | agree | Refused at step 2, before any read. Both files leave the error name open: the expectation because §7.2 also applies, this implementation because the prefix's first ordinals are also not dense (GAPS.md E-5). |
| resume-boundary-read | resume-01 | agree | Refused at §14 step 3 when ordinal 6's re-read meets the filemark at LBA 17, and nothing is written. The text names no error for this refusal (GAPS.md E-1), and the expectation leaves the error informative. |

Result: 8 of 8 portable cases agree. The two accepted resumes reproduce the
reference's resumed tapes byte for byte. `resume-commit-records` is not
portable (the commit-record format is implementation-defined, §3.4) and is not
compared.

## Opaque ids

resume-01 = resume-boundary-read; resume-02 = resume-open; resume-03 =
resume-full-unprotected-epoch; resume-04 = resume-nonconsecutive-epochs;
resume-05 = resume-closed; resume-06 = resume-noncontiguous-sidecars; resume-07
= resume-w-greater-than-t; resume-08 = resume-final-object-not-at-t.

The blind cases gave every case an Object to append, so the inputs did not
reveal which resumes would be accepted.
