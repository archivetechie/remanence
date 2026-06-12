# Reliability concern — tape recycle leaves catalog↔tape identity inconsistent

> Reported from the `system` steering harness (2026-06-09) after extensive
> re-validation. Not a fix; a precise problem statement + repro + hypothesis +
> fix directions for whoever owns the rem tape lifecycle. Severity: **medium-high**
> — it makes back-to-back scenario reruns flaky and will directly bite multi-copy
> self-heal work (system Scenario Q).

## Summary

After a VTL rebuild and/or repeated tape-recycle operations, writes to a recycled
tape fail with a **tape-identity mismatch**, and `rem tape init` **refuses to
re-init** the tape ("anomaly … refused-no-write"). The physical tape's BOT uuid and
the rem catalog's barcode→uuid mapping diverge, and nothing in the recycle path
reconciles them. Only a full catalog reset (`make reset` → `rem catalog reset` →
`make up`) restores a consistent slate.

## Observed symptoms (exact strings)

On a recycled/rebuilt tape, a daemon write session fails its identity precondition:
```
FAILED_PRECONDITION: tape identity: tape identity mismatch:
  expected 5e8f8d3b-9651-4d90-99b8-7f1218fb35cb, found 3b158ca7-57e1-4bdc-ad2a-f6d3793037f5
```
`recycle-scenario-*-tapes.sh` (which calls `rem tape init`) refuses to clobber:
```
tape init RMJ101L9: refused-no-write
  decision: anomaly: barcode assigned to 5e8f8d3b-…; BOT uuid=3b158ca7-…
```
A freshly rebuilt (blank) cartridge instead fails with no bootstrap at BOT:
```
FAILED_PRECONDITION: tape identity: absent bootstrap at BOT: read BOT:
  drive rejected the command: SCSI check condition (bytes_transferred=0): sense=[f0,00,08,…]
```
…or, at the pool level:
```
RESOURCE_EXHAUSTED: pool amber-aof has no writable tapes (N rejection(s))
```

## How to reproduce
1. Bring the system up green (`make up`); run a tape scenario (e.g. J/K/M) — passes.
2. Either rebuild the QuadStor VTL (cartridges get fresh BOT uuids) **or** run a
   scenario's `recycle-*-tapes.sh` and then the scenario again, back-to-back, a few
   times.
3. Writes start failing with the identity mismatch above; `rem tape init` reports
   `refused-no-write` / `anomaly`.
4. `make reset` (does `rem catalog reset`) + `make up` clears it; the scenario is
   green again on the clean slate.

## Root-cause hypothesis
- The rem catalog (`rem-state.sqlite`) persists a **barcode → BOT-uuid** binding.
- Recycling/rebuilding a tape writes a **new** BOT uuid to the medium, but the
  catalog still holds the **old** uuid for that barcode.
- The write-session identity check compares expected (catalog) vs found (medium) and
  rejects on mismatch — correct behavior, but there's **no reconciliation step** that
  updates (or invalidates) the catalog binding when a tape is legitimately recycled.
- `rem tape init` without `--clobber-data` treats the divergence as an anomaly and
  refuses, so the recycle scripts can't self-correct. `make reset` only works because
  it wipes the whole catalog, not because recycle reconciles.

## Daemon-state note (kept honest)
Several apparent "daemon outages" during re-validation were **clean systemd stops**,
not crashes: `systemctl --user status` showed `code=killed, signal=TERM` with
`Stopping… Stopped`, caused by tooling (the `recycle-*` scripts stop the daemon; some
scenario `make` targets don't restart it) and by command sequencing. The resulting
`UNAVAILABLE: … rem.sock: No such file or directory` is the socket being absent, not a
panic. **No rem process crash was confirmed.** One case left the user unit in `failed`
state under rapid churn — worth confirming whether a stop during an in-flight tape op
can exit non-zero, but treat "crash" as unproven.

## Impact
- Back-to-back scenario reruns are flaky without a full `reset`+`up` between them.
- Any workflow that **recycles a tape and re-reads/re-writes it** (notably multi-copy
  **self-heal**: lose a copy → rebuild) is directly exposed — the rebuilt/cleared
  tape will mismatch the catalog.

## Fix directions (for rem owners to weigh)
1. **Make recycle reconcile.** When a tape is legitimately re-initialized, update the
   catalog's barcode→uuid binding (or mark the old copies on it MISSING and rebind)
   so subsequent writes pass the identity check — no full catalog reset required.
2. **`rem tape init` semantics.** Give recycle a first-class "this medium was
   intentionally replaced" path (idempotent rebind) instead of the blanket
   `refused-no-write` anomaly, distinct from `--clobber-data` (which is about
   overwriting *data*, not *identity*).
3. **Targeted catalog reconcile** for a single barcode/tape (a scoped version of
   `catalog reset`) the recycle scripts can call.

## Workaround (current)
Drive flaky reruns from a clean `make reset && make up`, single run per scenario.
The `system` harness now does exactly this for re-validation.
