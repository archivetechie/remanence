#!/usr/bin/env bash
# Written owner exclusion and capability custody, checked before hardware probes.
set -euo pipefail
scripts="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
source "$scripts/lib.sh"
[[ ${FIELD_SESSION_EPOCH_ARM:-} == session-epoch-a ]] || { echo 'field leg A is not owner armed' >&2; exit 2; }
[[ ${REM_FIELD_ALLOW_FAIL:-} == 1 ]] || { echo 'failed-append requires REM_FIELD_ALLOW_FAIL=1' >&2; exit 2; }
fieldtest_init_layout
fieldtest_python >/dev/null || exit 2
out="$(fieldtest_evidence_dir)/session-epoch-preflight"
mkdir -p "$out"
python3 - "${FIELD_MIRIA_ATTESTATION:?set written attestation path}" "$out" <<'PY'
import datetime, os, pathlib, shutil, sys
source, out = map(pathlib.Path, sys.argv[1:])
text = source.read_text()
for literal in ["Miria has no drive access and will issue no moves", "DEC418146K_LL01", datetime.date.today().isoformat()]:
    assert literal in text, f"attestation must include {literal!r}"
assert source.stat().st_uid in (0, os.getuid())
shutil.copyfile(source, out / "miria-attestation.txt")
PY
for bin in "$(fieldtest_rem_bin)" "$(fieldtest_rem_debug_bin)" "$(fieldtest_rem_daemon_bin)"; do
  [[ $(stat -c %U "$bin") == root && ! -w $bin ]] || { echo "capability binary must be root-owned and not writable: $bin" >&2; exit 2; }
  getcap "$bin" | tee -a "$out/capabilities.txt" | grep -q 'cap_sys_rawio=ep'
done
# The plan's reset leg archives the old catalogue before any startup.
