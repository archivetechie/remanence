# Datamover leg A sense captures

These four files are unmodified captures from field leg A on 2026-11-05:

`/home/user/remfield/legA-20261105T072837Z/evidence/41-session-closeout/session-epoch-a-20261105T072837Z/real-drive-vectors/`

Each JSON retains the full completion and captured identity, including the original
datamover `commands.jsonl` source path. Each `.bin` contains its raw sense bytes.
The host was datamover, RHEL 9.6, kernel 5.14.0-570; the drive was HPE Ultrium 9
(Ultrium 9-SCSI), firmware S2SD. These observations cover this one drive model,
firmware and kernel only.

Both completions are variable-block READ(6), CDB `08 00 04 00 00 00`, requesting
262144 bytes. SG reports 262144 bytes transferred in both cases. Current valid
sense reports a full 262144-byte residual: BLANK CHECK 08/00/05 at EOD and
FILEMARK at the filemark. Thus neither completion delivers data bytes.

No WRITE capture is available. Tests derived from these shapes for WRITE or
fixed-block READ are synthetic models, not additional hardware observations.
