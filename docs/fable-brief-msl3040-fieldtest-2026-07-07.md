# Fable brief -- MSL3040 fieldtest and media-readiness follow-up

**Audience:** Claude Fable 5 design/review pass.
**Date:** 2026-07-07.
**Scope:** physical HPE MSL3040 field testing of Remanence on LTO-9 media,
especially the interaction between multi-object append, media-readiness fences,
operator UX, and live status.

This note summarizes the live field session, including operator observations
that are not fully represented in structured evidence. It should be read with:

- `docs/lto9-media-readiness-design-v0.1.md`
- `docs/layer5-multi-object-append-design-v0.1.md`
- `fieldtest/RUNBOOK.md`
- `fieldtest/TODAY-MSL3040-GUIDE.md`
- `~/drishti/docs/msl3040-remfield-rca-notes.md`
- field evidence pushed from `~/remfield/evidence/`

## 1. Physical setup

The production host exposes the MSL3040 as multiple logical libraries. The rem
test must only use the LTO-9 partition:

```text
DEC418146K_LL02
  changer: /dev/sg8
  drive 0x0001: /dev/sg7   LTO-9 serial 8031BDC7D1
  drive 0x0002: /dev/sg11  LTO-9 serial 8031BDC7DB
```

The LTO-7/D2 restore partition is production-owned and must not be touched by
Remanence field tests. Device numbers changed after recabling/rescan, so serials
and element addresses are the stable identity, not `/dev/sgN`.

Scratch media in the rem partition:

- `AOX034L9`
- `AOX030L9`
- `AOX031L9`
- `AOX032L9`
- `CLNU01L9` cleaning cartridge

## 2. Why this test exists

The original goal was to use a rare physical MSL3040 access window to prove that
Remanence works against a real tape library, not only the VTL/model path. During
planning, the operator challenged a major gap: if Remanence could write only one
object per tape, physical testing was not very meaningful because an 18 TB LTO-9
tape would never be used properly. That forced the multi-object append design and
fieldtest updates.

The current field suite therefore tries to prove:

- safe selected-library discovery in a host with LTO-9 and LTO-7 partitions;
- scratch-tape initialization and catalog visibility;
- plaintext and encrypted write/read/verify;
- multiple independent objects appended to one tape with dense tape-file
  numbers;
- benchmark paths;
- stewardship, cleaning, robotics, fault, and soak behavior;
- evidence collection good enough for later RCA.

## 3. Important field incidents so far

### 3.1 Library selection and production safety

Discovery initially found both logical libraries. We selected
`DEC418146K_LL02` because it has the scratch LTO-9 tapes and the intended LTO-9
drives. We explicitly blocked access to the LTO-7/D2 devices. Earlier confusion
around `/dev/sg7`, `/dev/sg11`, and `/dev/sg4` showed why serial-scoped selection
and allowlists are mandatory.

### 3.2 Pre-existing loaded media and drive ownership

Initial `tape init` attempts failed because tapes were already loaded in drives.
One LTO-9 drive was temporarily unavailable or confused by another process/state,
and at one point an enterprise/D2 process held a tape device. We backed off from
the LTO-7 side and proceeded only after the LTO-9 partition mapping was coherent.

### 3.3 LTO-9 media readiness and `Calib`

During init, `AOX030L9` entered a long state where the MSL3040 UI showed
`Calib`. Host-side commands saw Unit Attention, `TEST UNIT READY` becoming-ready
conditions, SG_IO transport timeout/completion-unknown errors, and kernel
`smartpqi` task abort/reset messages. One `mtx status` also blocked in
`sg_ioctl_common`, and the changer target later reset after a `MOVE MEDIUM`
opcode.

Conclusion from the session:

- This did not prove that the SCSI commands themselves were wrong.
- It proved the orchestration was not media-readiness-aware enough.
- The library UI exposed useful state that the host CLI did not show.
- Remanence needed a durable media-readiness operation, a no-move wait path, and
  conservative fences after unknown-completion errors.

Important terminology correction: not every readiness wait is calibration.
`02/04/01` is generic "logical unit is in process of becoming ready." LTO-9
first-load media optimization is one possible reason when corroborated by
generation/media context and library UI state. LTO-7 can briefly report
becoming-ready during normal mechanical transitions, but it does not have the
long LTO-9 first-load optimization behavior.

## 4. Implemented response before this brief

Recent implementation work added:

- LTO-9 media-readiness classifier and durable `media_readiness_ops` state.
- `rem tape wait-ready --resume <operation_id> --wait`.
- init gates that stop before destructive escalation on readiness conditions.
- field script `09-media-ready.sh`.
- field script auto-wait/retry for daemon write/read readiness fences.
- startup/session-open/close/unmount readiness admission checks.
- fail-closed behavior for unclassified init failures.
- clearer operator guidance in wait-ready JSON/human output.
- Drishti/RCA notes for kernel/HBA/log correlation.

The field package deployed to the LTO server included build commit
`79353ec666251c00a9298372e4efc163fb21e10c` for the auto-wait wrapper work, and
later follow-up builds for init/readiness fixes.

## 5. Observed test outcomes

### 5.1 Init

`10-init-pools.sh --count 4` eventually passed:

- `AOX034L9`: initialized with `clobber-data`; earlier plain init correctly
  refused because physical data existed past bootstrap.
- `AOX030L9`: initialized plain.
- `AOX031L9`: initialized plain.
- `AOX032L9`: initialized plain.
- all four were visible in the daemon catalog after bringup.

Operator observation: the init/calibration window was long. One tape appeared to
be initializing for at least 45 minutes before the operator stopped watching.
This is operationally confusing if the product UI only shows a job as queued or
busy.

### 5.2 Happy path

The first happy-path run failed on a readiness fence after init. Running
`09-media-ready.sh --resume <operation_id>` cleared the fence, and a manual read
retry of the written object succeeded with matching SHA-256.

After the field scripts learned to auto-wait/retry readiness fences,
`11-happy-path.sh` completed successfully. It still hit multiple readiness fences
during plain write, plain read, encrypted write/read, and verify, but each
auto-wait completed and the script passed:

- plaintext write PASS;
- plaintext read/fidelity PASS;
- range read PASS;
- encrypted round-trip PASS;
- verify PASS.

Operator observation: `rem top` showed only generic `busy`/`loaded` states. It
did not say whether the drive was writing, reading, seeking, waiting for media
readiness, closing, unloading, or writing filemarks.

### 5.3 Append loop

`13-append-loop.sh` completed successfully on the physical MSL3040:

```text
[PASS] 13-append-loop media-budget: found 2 appendable ready tape(s) in fieldtest-a for append loop
[PASS] 13-append-loop dense-files: wrote 6 objects to one tape with dense tape-file numbers
[PASS] 13-append-loop fidelity-0: append-loop object 0 restored with matching SHA-256
[PASS] 13-append-loop fidelity-1: append-loop object 1 restored with matching SHA-256
[PASS] 13-append-loop fidelity-2: append-loop object 2 restored with matching SHA-256
[PASS] 13-append-loop fidelity-3: append-loop object 3 restored with matching SHA-256
[PASS] 13-append-loop fidelity-4: append-loop object 4 restored with matching SHA-256
[PASS] 13-append-loop fidelity-5: append-loop object 5 restored with matching SHA-256
[PASS] 13-append-loop summary: append loop completed: 6 objects, 64 MiB each, pool fieldtest-a
```

This is the strongest physical result in the session so far for the
multi-object append design: six independent 64 MiB objects were appended to one
tape with dense tape-file numbers, and every object was restored with matching
SHA-256.

Operator observations during the append-loop run:

- output lines arrived slowly;
- `rem top` showed a drive as busy;
- MB/s barely moved during long periods;
- every write and every read appeared to hit a media-readiness fence before
  succeeding;
- the wait-ready artifacts appeared to succeed on the first readiness attempt,
  so this looked more like repeated short settle/poll overhead than a long
  calibration window;
- the operator asked whether these delays will affect normal tape read/write
  operations.

Current interpretation:

- Correctness is good: same-tape dense append and read-back fidelity both
  passed on physical LTO-9.
- `13-append-loop.sh` intentionally stresses repeated independent object writes.
  On physical media the default is multiple small-ish objects, not one sustained
  stream.
- Each object can incur session open, readiness probe, optional wait-ready,
  positioning/append setup, short data transfer, filemarks, close/unmount, and
  later separate read setup.
- A drive can therefore be "busy" without streaming many bytes.
- The repeated fences may be conservative but successful, or they may indicate
  that Remanence is over-triggering readiness admission between closely spaced
  appends.
- This is different from a normal large sequential write, where once the drive
  is ready and streaming the throughput should dominate. It is directly relevant
  to production behavior if production submits many small objects as separate
  tape sessions instead of batching.

## 6. Evidence paths to inspect

On the LTO server before evidence push:

```text
~/remfield/evidence/records.jsonl
~/remfield/evidence/09-media-ready/
~/remfield/evidence/10-init-pools/
~/remfield/evidence/11-happy-path/
~/remfield/evidence/13-append-loop/
~/remfield/log/rem-daemon.log
~/remfield/state/media-readiness.jsonl
```

Recommended operator command before Fable review:

```sh
cd ~/remfield
./push-evidence.sh
```

Quick append-loop wait summary command:

```sh
cd ~/remfield
python3 - <<'PY'
import glob, json, os
from pathlib import Path

for p in sorted(glob.glob("evidence/13-append-loop/*wait-ready*.json"), key=os.path.getmtime)[-30:]:
    data = json.loads(Path(p).read_text())
    out = data.get("stdout", "")
    try:
        w = json.loads(out)
    except Exception:
        w = {}
    print(Path(p).name, "rc=", data.get("exit_code"), "attempts=", w.get("attempts"), "state=", w.get("state"), "ready=", w.get("ready"), "timed_out=", w.get("timed_out"))
PY
```

## 7. Questions for Fable

1. Are repeated media-readiness fences during `13-append-loop.sh` expected for
   LTO-9 after a close/open cycle, or do they indicate that Remanence is
   over-conservative in session-open admission?
2. Should same-pool append-loop writes keep the tape mounted and the drive actor
   open across multiple objects, at least for the benchmark/fieldtest path, to
   separate append-format behavior from repeated mount/session overhead?
3. If the wait-ready artifacts succeed on the first attempt, should the wrapper
   or daemon avoid a full 30-second polling cadence by doing an immediate
   follow-up TUR before sleeping?
4. Should the daemon distinguish `ready-after-load-settle`,
   `ready-after-positioning-settle`, and `lto9-media-optimization-possible`, or
   is a generic `media_readiness_state` plus raw sense enough?
5. What fields should `rem top`/`GetLiveStatus` expose so an operator can tell
   the difference between streaming, seeking, filemark/close, load/unload,
   readiness wait, and a true hang?
6. Does the current auto-wait/retry wrapper risk hiding a bug by making repeated
   readiness fences look normal? If so, what thresholds should become warnings
   or failures in the field scripts?
7. For production, should Remanence encourage batching small objects into larger
   tape sessions, or should the daemon itself provide an append-session API that
   amortizes load/readiness/position/filemark overhead?
8. What should be the acceptance criterion for physical append performance: raw
   streaming MB/s, per-object latency, dense tape-file correctness, or a
   workload-weighted mix?

## 8. Provisional recommendations

These are not final design decisions; they are the current working hypotheses to
challenge.

- Keep the readiness fence and auto-wait behavior. It prevented unsafe retries
  and let the field suite proceed.
- Do not call every readiness wait "calibration." Use generic
  `Waiting for media readiness` unless generation/media/library context supports
  `LTO-9 media optimization`.
- Add phase-level live status before drawing performance conclusions from
  `rem top`; generic `busy` is not enough.
- Treat append-loop latency as a distinct metric from streaming throughput.
- Add field evidence that breaks down per-object time into session open,
  readiness wait, data transfer, filemark/commit, close/unmount, and read
  positioning.
- If repeated one-attempt readiness waits continue, consider an immediate
  second readiness probe and/or a short grace window after a successful wait
  before creating another durable fence for the same loaded tape.
- For production ingestion, avoid tiny independent tape sessions unless the
  latency is acceptable. Tape is happiest when it can stream sequentially.
