# Code review -- LTO-9 media-readiness signal interrupts, Fable 5

**Date:** 2026-07-06
**Reviewer:** Claude Fable 5 via OpenRouter
(`anthropic/claude-fable-5`, routed as `anthropic/claude-5-fable-20260609`)
**Scope:** commit `d0648c7`, the direct-CLI signal/interrupt reconciliation
slice for `rem tape wait-ready` and `rem tape init`.

## Verdict

Initial review found one blocker and one high finding. Both were accepted and
folded in the follow-up diff.

## Findings and disposition

- **Blocker, accepted:** `tape init` created a durable
  `media_readiness_ops` row before installing the SIGINT/SIGTERM guard. A
  signal in that window could leave a `planned` row without an
  `aborted_unknown` transition. Fold: install the guard before operation-record
  creation in both `tape init` and `wait-ready`, then check the signal flag
  immediately after the record is available.
- **High, accepted:** without `SA_RESTART`, a signal can interrupt a blocking
  SG_IO/open path and surface as an ordinary hardware error. The old error
  paths recorded generic `transport_unknown` or command failures instead of
  `cancel_source=signal`. Fold: signal-aware error helpers now prefer
  `aborted_unknown` with `cancel_source=signal` whenever the signal flag is set
  after an interrupted hardware/prompt error.
- **Medium, accepted through blocker fold:** if signal-guard installation
  failed after operation creation, the operation could remain stranded in
  `planned`. Fold: guard installation now happens before new operation rows are
  written.
- **Medium, residual:** repeated Ctrl-C/SIGTERM does not escalate to default
  process termination from inside the handler. The current behavior stays
  async-signal-safe and normally aborts at the next checkpoint or 250 ms sleep
  slice; a truly wedged kernel ioctl still requires external SIGKILL/operator
  recovery.
- **Low, residual:** the handler assignment uses the Linux `sigaction` union
  through `sa_sigaction` without `SA_SIGINFO`. This is the established libc
  pattern here, but a future edit should not add `SA_SIGINFO` without changing
  the handler signature.

## Folded tests

- Signal-caused command failures now prefer `aborted_unknown` over command
  failure evidence.
- `wait-ready` signal poll errors now map to exit code 130 instead of generic
  exit code 1.

## Residual risk

This review does not close the remaining LTO-9 readiness gates: READ ELEMENT
STATUS secondary evidence and scenario/chaos coverage are still pending.
