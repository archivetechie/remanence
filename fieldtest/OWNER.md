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
record; a refusal, timeout, or incomplete parity epoch close is a WARN naming
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
