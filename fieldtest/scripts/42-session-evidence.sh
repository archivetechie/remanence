#!/usr/bin/env bash
# Re-read every committed prefix, or export the actual READ-at-FM/EOD vectors
# observed by the compile-time field trace. No independent device commands.
set -euo pipefail
scripts="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
source "$scripts/lib.sh"
case ${1:-} in --verify-prefixes|--capture-sense) ;; *) exit 2;; esac
run_id=${FIELD_SESSION_RUN_ID:-manual}
[[ $run_id =~ ^[A-Za-z0-9_-][A-Za-z0-9_.-]*$ ]] || exit 2
root="$(fieldtest_evidence_dir)/41-session-closeout/$run_id"
[[ -d $root ]] || exit 1
if [[ $1 == --verify-prefixes ]]; then
  while IFS= read -r receipt; do
    dir=$(dirname "$receipt")
    object=$(python3 -c 'import json,sys; print(json.dumps(json.load(open(sys.argv[1]))))' "$receipt")
    bytes=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["logical_size_bytes"])' "$receipt")
    fieldtest_require_space "$((bytes + FIELDTEST_GIB))" "final prefix restore"
    work=$(mktemp -d "$(fieldtest_spool_dir)/prefix-restore.XXXXXX")
    trap 'rm -rf -- "$work"' EXIT
    "$(fieldtest_io_bin)" --endpoint "$(fieldtest_rem_endpoint)" read --object "$object" --out "$work/prefix-final.bin" > "$dir/prefix-final.json"
    python3 - "$dir" "$work" <<'PY'
import hashlib,json,pathlib,sys
p=pathlib.Path(sys.argv[1]); h=hashlib.sha256()
with (pathlib.Path(sys.argv[2])/'prefix-final.bin').open('rb') as f:
    while chunk:=f.read(1048576): h.update(chunk)
assert h.hexdigest()==json.loads((p/'receipt.json').read_text())['content_sha256']
(pathlib.Path(sys.argv[2])/'prefix-final.bin').unlink()
PY
    rmdir "$work"
    trap - EXIT
  done < <(find "$root" -name receipt.json -type f | sort)
else
  python3 - "$root" <<'PY'
import json,pathlib,sys
root=pathlib.Path(sys.argv[1]); out=root/'real-drive-vectors'; out.mkdir(exist_ok=False)
found=set()
for path in sorted(root.glob('*/commands.jsonl')):
    identity=json.loads((path.parent/'identity.json').read_text())
    for line in path.read_text().splitlines():
        row=json.loads(line); cdb=row['cdb']; sense=row.get('sense_raw')
        if cdb[0]!=8 or cdb[1]&1 or not sense or len(sense)<14 or sense[0]&0x7f not in (0x70,0x71): continue
        kind='filemark' if sense[2]&0x80 else 'eod' if sense[2]&0xf==8 and sense[12:14]==[0,5] else None
        if kind is None or kind in found: continue
        assert row['drive']['serial'] and row['drive']['firmware'] and row['drive']['library_serial']=='DEC418146K_LL01'
        # Preserve raw bytes and transport counts; do not synthesize sense.
        (out/f'{kind}.bin').write_bytes(bytes(sense))
        (out/f'{kind}.json').write_text(json.dumps({'source':str(path),'identity':identity,'completion':row},indent=2)+'\n')
        found.add(kind)
assert found=={'filemark','eod'}, 'missing real variable READ filemark/EOD sense; field leg is incomplete'
PY
fi
