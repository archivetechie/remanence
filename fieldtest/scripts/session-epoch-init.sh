#!/usr/bin/env bash
# Reinitialize the four approved scratch identities with parity; stopped daemon
# ownership is required for retirement and direct init. Preserve the old catalog.
set -euo pipefail
scripts="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
source "$scripts/lib.sh"
[[ ${FIELD_SESSION_EPOCH_ARM:-} == session-epoch-a ]] || exit 2
rem=$(fieldtest_rem_bin)
config=$(fieldtest_config_path)
serial=$(fieldtest_selected_library_serial)
[[ $serial == DEC418146K_LL01 ]] || exit 2
evidence="$(fieldtest_evidence_dir)/session-epoch-reinit-$(fieldtest_timestamp_id)"
mkdir -p "$evidence"
"$rem" catalog tapes --endpoint "$(fieldtest_rem_endpoint)" --json > "$evidence/before.json"
"$scripts/03-bringup.sh" --stop
for barcode in AOX032L9 AOX036L9 AOX038L9 AOX058L9; do
  fieldtest_require_allowlisted "$barcode"
  uuid=$(python3 - "$evidence/before.json" "$barcode" <<'PY'
import json, sys
rows = [t for t in json.load(open(sys.argv[1]))["data"]["tapes"] if t.get("voltag") == sys.argv[2] and t.get("state") != "retired"]
assert len(rows) <= 1, "ambiguous old identity"
print(rows[0]["tape_uuid"] if rows else "")
PY
)
  extra=()
  if [[ -n $uuid ]]; then
    "$rem" tape retire "$uuid" --config "$config" --reason session-epoch-a-approved-reinit --i-understand-copies-become-unreadable --json > "$evidence/$barcode-retire.json"
  else
    # A reset catalog has no old identities. Bind clobber to the actual BOT
    # UUID from a read-only probe; never guess it from old run evidence.
    "$rem" library "$serial" --json --slots > "$evidence/$barcode-slots.json"
    slot=$(python3 - "$evidence/$barcode-slots.json" "$barcode" <<'PYPROBE'
import json,sys
rows=[s for s in json.load(open(sys.argv[1]))['slots'] if s.get('cartridge')==sys.argv[2] and s.get('full')]
assert len(rows)==1, 'exact barcode must be in one storage slot before re-init'
print(rows[0]['element_address'])
PYPROBE
)
    "$(fieldtest_rem_debug_bin)" --allow "$serial" tape bot-probe "$barcode" --config "$config" --library "$serial" --expected-home-slot "$slot" --json > "$evidence/$barcode-bot.json"
    old=$(python3 - "$evidence/$barcode-bot.json" "$barcode" <<'PYPROBE'
import json,sys
p=json.load(open(sys.argv[1])); assert p['barcode']==sys.argv[2] and p['library_serial']=='DEC418146K_LL01'
assert p['physical_disposition'] in ['bootstrap_valid','blank_check_eod'], 'unexplained BOT stops re-init'
print(p['tape_uuid'] or '')
PYPROBE
)
    [[ -z $old ]] || extra=(--clobber-unknown-tape "$old")
  fi
  "$rem" --allow "$serial" tape init "$barcode" --config "$config" --library "$serial" --parity default --force --clobber-data --confirm-clobber "CLOBBER $barcode" "${extra[@]}" > "$evidence/$barcode-init.txt"
  fieldtest_drain_drives "$serial"
done
"$scripts/03-bringup.sh"
"$rem" catalog tapes --endpoint "$(fieldtest_rem_endpoint)" --json > "$evidence/after.json"
python3 - "$evidence" <<'PY'
import json, pathlib, sys
p=pathlib.Path(sys.argv[1])
before=json.loads((p/'before.json').read_text())['data']['tapes']
after=json.loads((p/'after.json').read_text())['data']['tapes']
for barcode in ['AOX032L9','AOX036L9','AOX038L9','AOX058L9']:
    old=next((t for t in before if t.get('voltag')==barcode and t['state']!='retired'), None)
    new=next(t for t in after if t.get('voltag')==barcode and t['state']!='retired')
    assert (old is None or new['tape_uuid']!=old['tape_uuid']) and new['pool_id'], 're-init/pool assignment failed'
PY
