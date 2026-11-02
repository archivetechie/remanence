# Running a field plan yourself

This package runs one test plan on the field host with no remote session. You
install it as root, start it as the kit user and copy back one evidence file.
Everything else the plan does is in `plan/<name>.env` and in the kit scripts
under `scripts/`.

## 1. Check the package

The sha256 of the tarball comes to you separately from the tarball itself. Check it before anything else:

```
sha256sum remfield-run-<commit>.tar.gz     # must match what you were given
```

## 2. Install it read-only (root, once per package)

```
install -d -o root -g remfield -m 0750 /opt/remfield
tar -xzf remfield-run-<commit>.tar.gz -C /opt/remfield
chown -R root:remfield /opt/remfield/remfield-run-<commit>
chmod -R u=rwX,g=rX,o= /opt/remfield/remfield-run-<commit>
cd /opt/remfield/remfield-run-<commit>
sha256sum -c SHA256SUMS
setcap cap_sys_rawio+ep bin/rem bin/rem-debug bin/rem-daemon
ln -sfn remfield-run-<commit> /opt/remfield/current
```

The kit user can run these binaries but can't replace them, so the raw-SCSI
capability can't end up on any other program. Once a package is installed this
way, the kit user no longer needs a sudo rule for `setcap`. Delete that rule.

## 3. Before each run (root)

- Quiesce Miria on the partition the plan uses.
- Give the kit user the plan's changer and drive device nodes, and nothing else:
  `setfacl -m u:remfield:rw /dev/sgN ...`. Get the nodes from discovery; sg
  numbers move after a drive firmware flash.
- Make sure the spool is the capped home or a tmpfs you sized. The kit's disk
  guard refuses payloads that would leave the shared `/home` short. See
  "Disk guard" in RUNBOOK.md.
- Raise `max_sectors_kb` for throughput plans. See "Host I/O setup" in RUNBOOK.md.

The plan refuses to start unless `~/remfield/allowlist.txt` lists exactly the
plan's barcodes, with no cleaning cartridge. It also refuses if the kit state has
selected a different library. Check the allowlist with
`sudo -u remfield cat ~remfield/remfield/allowlist.txt`. To write a new one, run
`FIELD_BIN_DIR=/opt/remfield/current/bin /opt/remfield/current/scripts/01-allowlist.sh`
interactively as remfield; it asks you to type the barcodes.

## 4. Start it (as the kit user, in tmux)

```
sudo -iu remfield tmux new -s run '/opt/remfield/current/run.sh plan/smoke.env; exec bash'
```

Detach with `Ctrl-b d`, and reattach with `sudo -iu remfield tmux attach -t run`.
The window stays open after the run, so you can read the result. The run log is
`~/remfield/evidence/run-<plan>-<stamp>.log`, and it goes into the evidence pack.

To stop early, press `Ctrl-c` in the tmux window, or send TERM to the `run.sh` process.
Either way the current leg stops. If something failed, run.sh captures read-only
diagnostics. Its `03-bringup.sh --stop` teardown asks the live daemon to unload
each allowlisted cartridge seated in the selected library to its recorded source
slot, then stops the daemon and packs the evidence. Each unload has an evidence
record; a refusal or timeout is a WARN naming
the bay. The unload phase is bounded and daemon stop follows even if it fails. Cartridges outside the allowlist
are left alone. Graceful daemon shutdown itself leaves cartridges seated. A finished run prints a PASS/FAIL table, the evidence
pack's path and its sha256.

### The magazine plan (you at the library)

`plan/magazine.env` checks that the daemon waits out an open magazine instead of
failing a move. It needs you at the library and at the tmux window together, so
reach the window from a laptop or phone next to the library. Before you start, make
sure all four allowlisted tapes are in storage slots and one LL01 drive is empty.

```
sudo -iu remfield tmux new -s run '/opt/remfield/current/run.sh plan/magazine.env; exec bash'
```

When the magazine leg starts, it names the tape, its slot and the drive it will use.
Then:

1. Pull out ONE magazine that holds none of the four allowlisted tapes. Opening a
   magazine pauses the robot for every partition, so do it when the other
   partitions are not moving tapes. Touch nothing inside it.
2. Type `open` and press Enter. Within two minutes the window shows
   `live status: waiting for library: medium magazine removed …`.
3. When the window asks, push the magazine back in, type `closed` and press Enter.
4. Wait. The library re-inventories, the daemon re-checks the slot and the drive
   and retries the move, and the leg reports PASS. It then unloads the tape back
   to its slot. The plan ends as any plan does, with the table and the evidence pack.

If you stop the plan with `Ctrl-c` while the magazine is out, push the magazine
back in first. Teardown can only return the tape once the library is ready again.

## 5. Return the evidence

Copy `~/remfield/evidence-pack-<plan>-<stamp>.tar.gz` (the path run.sh printed)
back from your own account. Send its sha256 with it.

## Undo

- `rm -rf /opt/remfield`
- `setfacl -x u:remfield /dev/sgN ...`
- Restart Miria and rescan its devices, as at the end of any window.

### Session epoch field leg A (MSL3040, LL01 LTO-9)

Use a committed **field-kit** package for migration step 3. Dry packages refuse
hardware use. This plan destroys and reinitializes AOX032L9, AOX036L9, AOX038L9
and AOX058L9 with default parity, then checks kill, stream abort and failed
append closeout. It also measures a near-full epoch and a live-close control.
Use dedicated schema-24/v2 kit state containing only these scratch identities.
Keep other clients idle. The old catalog and re-init results enter the evidence.

Before running, write a dated, signed text file containing the exact statement
`Miria has no drive access and will issue no moves`, the serial
`DEC418146K_LL01`, and today's date (`YYYY-MM-DD`). Confirm that Miria's drive
access and changer moves really are disabled for this window. The package's
capability binaries must be root-owned and unwritable by the kit user.

As the kit user, with the four-barcode allowlist already recorded:

```bash
export FIELD_MIRIA_ATTESTATION="$HOME/miria-exclusion.txt"
export FIELD_SESSION_EPOCH_ARM=session-epoch-a
export REM_FIELD_ALLOW_FAIL=1
/opt/remfield/current/run.sh plan/session-epoch-a.env
```

`REM_FIELD_ALLOW_FAIL=1` explicitly arms the one-shot failed-append test in
the separately compiled field binary. It does not select an epoch policy. Ordinary
production builds have no field dispatch hook. Each fault requires a durable
prefix receipt in an open session, then a completed data WRITE for a second
object whose Finish has been withheld. SIGKILL targets the verified daemon PID.
The restart closes out that exact UUID. No unexpected complete tail is approved.

Expect several long reads and sidecar writes, especially for the near-full
(default approximately 16 GiB) epoch. There is no one-minute pass threshold.
`rem top` shows closeout debt and its retry reason while work proceeds.
Stop with Ctrl-C or TERM to `run.sh`. Teardown stops the helper and waits for daemon-observed stream cancellation
before releasing a live test hold. It attempts Abort, returns allowlisted
cartridges, stops the daemon
and packs evidence. Do not manually unload a busy drive. If a shutdown deadline
expires, retain the crash evidence and inspect the reported state before reuse.
The service-manager stop timeout must exceed the 300-second default close
deadline plus a margin; the kit stop path supplies its own bounded wait.

The kit refuses before device access if `REM_CHAOS_ENABLED`,
`REM_CHAOS_ALLOW_REAL` or `REM_CHAOS_STATE` is present, even with an empty value.
The refusal names the variable; clear it deliberately before starting the kit.
`REM_FIELD_ALLOW_FAIL=1` arms only the field failure and is required by preflight.
The arm is consumed at object begin and checks the tape UUID and its catalog
barcode. Its nonce correlates helper/daemon evidence; it is not compared with a
separate daemon-held nonce or used as an authorization token.
`gate/holding.json` marks a live hold; `gate/timeout.json` records expiry.
Kill requires the live marker and refuses expiry. The helper and verifier also
refuse expiry, retaining the observed gate state in the evidence.
Dry review packages refuse direct script entry as well as `run.sh`.
