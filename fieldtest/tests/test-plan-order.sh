#!/usr/bin/env bash
# Model daemon ownership for every shipped plan, including composite leg handoffs.
set -euo pipefail
root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
python3 - "$root" <<'PY'
import os
from pathlib import Path
import shlex
import subprocess
import sys

root = Path(sys.argv[1])
daemon_legs = set('02-discovery 11-happy-path 12-multiobject 13-append-loop 20-bench-write 21-bench-read 22-bench-dual 30-stewardship 31-cleaning 34-verify-throughput 36-magazine-wait 40-faults 41-session-closeout 50-soak'.split())
neutral = set('00-preflight 01-allowlist 42-session-evidence 90-collect-evidence 91-cleanup'.split())

def check(legs, library, minimum):
    if library == 'DEC418146K_LL01':
        assert minimum == '1', 'single-drive LL01 requires exported FIELD_MIN_DRIVES=1'
    up = False
    for leg in legs:
        _, script, *args = shlex.split(leg)
        name = script.removesuffix('.sh')
        if name == '03-bringup':
            if '--status' in args:
                assert up, leg
            else:
                up = '--stop' not in args
        elif name == '10-init-pools' and '--session-epoch-a' in args:
            assert up, 'session re-init reads the daemon catalog before stopping'
            # Check the actual composite script still stops before direct access,
            # and restarts before its final catalog query.
            text = (root / 'scripts/session-epoch-init.sh').read_text()
            assert text.index('"$scripts/03-bringup.sh" --stop') < text.index('"$rem" tape retire') < text.index('"$scripts/03-bringup.sh"\n')
            up = True
        elif name == '32-robotics':
            assert up, leg
            text = (root / 'scripts/32-robotics.sh').read_text()
            assert text.index('/03-bringup.sh" --stop') < text.index('"$(fieldtest_rem_debug_bin)" --allow') < text.index('/03-bringup.sh" >/dev/null')
        elif name == '35-read-rate':
            assert up, 'plan read-rate needs daemon snapshots before its internal stop'
            text = (root / 'scripts/35-read-rate.sh').read_text()
            assert '03-bringup.sh" --stop || fail' in text
            up = False
        elif name in daemon_legs:
            assert up, f'{leg} needs daemon bring-up first'
        elif name in {'09-media-ready', '10-init-pools', '33-diag-capture'}:
            assert not up, f'{leg} needs daemon stop before direct SCSI'
        else:
            assert name in neutral, f'unclassified leg {leg}; declare its ownership'

for plan in sorted((root / 'plan').glob('*.env')):
    env = dict(os.environ)
    env.pop('FIELD_MIN_DRIVES', None)
    output = subprocess.check_output(['bash', '-c', 'source "$1"; printf "%s\\n" "$PLAN_LIBRARY" "${FIELD_MIN_DRIVES:-}"; env | grep "^FIELD_MIN_DRIVES=" >/dev/null || exit 2; printf "%s\\n" "${PLAN_LEGS[@]}"', 'plan-test', str(plan)], env=env, text=True)
    library, minimum, *legs = output.splitlines()
    check(legs, library, minimum)
    print(f'PASS: {plan.name} daemon ownership and single-drive setting')
# Mutation cases include direct TEST UNIT READY while the daemon is up.
for legs, minimum in [(['fatal 03-bringup.sh', 'fatal 09-media-ready.sh --count 4 --no-wait'], '1'), (['fatal 02-discovery.sh'], '1'), (['fatal 03-bringup.sh', 'fatal 10-init-pools.sh'], '1'), (['fatal 03-bringup.sh', 'fatal 33-diag-capture.sh'], '1'), (['fatal 32-robotics.sh'], '1'), ([], '')]:
    try:
        check(legs, 'DEC418146K_LL01', minimum)
    except AssertionError:
        pass
    else:
        raise AssertionError(f'bad plan accepted: {legs}, {minimum}')
PY
