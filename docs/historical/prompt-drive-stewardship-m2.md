# Codex prompt — DS-M2: auto-cleaning (remanence)

**Status:** pending (dispatch after DS-M1 lands).
**Normative:** Read `docs/contract-drive-stewardship.md` first.
Design rationale: `docs/drive-stewardship-design-v0.1.md` (v0.3,
frozen) §4. Definition of done: `AGENTS.md`. Step 0 skeleton-first as
in the M1 prompt.

## Scope

1. **Cartridge lifecycle:** registration classification by
   `voltag_prefixes` ⇒ `kind='cleaning'`, `cleaning_state='unverified'`,
   `CleaningCartridgeRegistered` audit (with remaining-use estimate);
   IE-port state change triggers inventory refresh; `rem tape set-kind
   --cleaning <voltag>` override; **kind-flip guard** (contract §6);
   expiry ⇒ `cleaning_state='expired'` (voltag stays bound — NEVER
   tape-retire), `cln-cart-expired` alarm; first-run corroboration ⇒
   `ok` or `rejected` + `cart-not-cleaning-behavior`.
2. **Detection:** persist-on-observe `cleaning_due` at session close /
   on-alert / manual poll (managed drives only; foreign per contract
   §6). Monotonicity rule.
3. **Query contract:** `list_tapes` gains a `kind` filter; enumerate
   EVERY call site and set its kind explicitly (data-facing default
   `data`: ListTapes RPC, pool eligibility pool_write.rs:554,
   durability accounting, catalog views library.rs:142; cleaning
   selector + drive/top surfaces opt into `cleaning`). List the full
   call-site table in your report.
4. **Cleaning actor:** durable `clean_runs` state machine exactly per
   design §4.3 — phase transitions, per-drive/per-cart active
   uniqueness (partial indexes), cart reservation, **startup
   reconciliation before session admission**, robotics choke-point
   gate (ONE pre-move guard), fence as persisted first-class admission
   state (`DriveFenced`/`DriveUnfenced` audited, `FENCED` status
   live), fence-after-session for `now` / natural-idle for `periodic`,
   post-clean verification before any credit (`min_cycle_duration`
   floor, flag-22 path, home-slot check, managed-drive TapeAlert
   re-read), failure protocol (`failed` after one in-run retry;
   `needs-operator` keeps bay fenced + alarm; never a fenced bay
   without an open alarm), frequency caps (`min_interval`,
   `weekly_cap` ⇒ `drive-cleaning-abnormal-frequency` instead of
   cleaning), `no-cln-cart` alarm branch.
5. **Surface:** `CleanDrive` RPC (Robotics permission; refuses foreign
   + retired + `actionable=0`; joins active run), `rem drive clean`.
6. **Chaos extension:** `VirtualDrive` dirty state — scenario-armed
   flag 20 after N ops; cleaning-cart load clears dirty + auto-ejects
   after a realistic cycle time; armed-expired cart fast-ejects with
   flag 22 (design §8 DS-M2).
7. **O1 investigation:** check whether QuadStor emulates cleaning
   cartridges/TapeAlert 20–22; if yes add an `#[ignore]` VTL smoke;
   record the answer in your report either way.

## Out of scope

Live-status serving, TUI (M3); retention; any d2tape change; foreign
cleaning actions of any kind.

## Acceptance

fmt/clippy/-D warnings/full suite; plus, named: hermetic
detect→fence→clean→verify→record; **crash-resume** (kill
mid-`cleaning`, reconcile on boot); fence vs session-open race;
manual/auto join; expired-cart quarantine (voltag stays bound;
rediscovery does NOT reset uses); corroboration reject path;
frequency-cap alarm; kind-flip refusal; no-cart branch. Diff gate
before archive.

Verification member: harness scenario **CLN** —
`~/system/docs/prompt-drive-stewardship-scenarios.md` §CLN.
