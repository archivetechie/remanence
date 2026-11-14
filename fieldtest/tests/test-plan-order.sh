#!/usr/bin/env bash
# Model daemon ownership for every shipped plan, including composite leg handoffs.
set -euo pipefail
root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
python3 - "$root" "${1:-all}" <<'PY'
import os
from pathlib import Path
import shlex
import subprocess
import sys

root = Path(sys.argv[1])
selected = sys.argv[2]
daemon_legs = set('02-discovery 11-happy-path 12-multiobject 13-append-loop 20-bench-write 21-bench-read 22-bench-dual 30-stewardship 31-cleaning 34-verify-throughput 36-magazine-wait 40-faults 41-session-closeout 50-soak'.split())
neutral = set('00-preflight 01-allowlist 42-session-evidence 90-collect-evidence 91-cleanup'.split())

def check(legs, library, minimum):
    if library == 'DEC418146K_LL01':
        assert minimum == '1', 'single-drive LL01 requires exported FIELD_MIN_DRIVES=1'
    # Reset policy, preflight ancestry, then state opening from leg zero.
    parsed = [shlex.split(leg) for leg in legs]
    preflights = [i for i, (_, script, *args) in enumerate(parsed)
                  if script == '00-preflight.sh' and '--session-epoch-a' in args]
    resets = [i for i, (_, script, *args) in enumerate(parsed) if script == '05-reset-state.sh']
    for i in resets:
        assert parsed[i][0] == 'fatal', 'reset leg must use fatal policy'
    for i in resets:
        assert any(p < i for p in preflights), 'reset leg must follow session-epoch preflight'
    if preflights:
        assert resets, 'session-epoch needs reset before any state-opening leg'
        for _, script, *args in parsed[:resets[0]]:
            state_opening = script in {'09-media-ready.sh', '10-init-pools.sh'} or (script == '03-bringup.sh' and '--stop' not in args)
            assert not state_opening, 'session-epoch needs reset before any state-opening leg'
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
        elif name in {'05-reset-state', '09-media-ready', '10-init-pools', '33-diag-capture'}:
            assert not up, f'{leg} needs daemon stop before direct SCSI'
        else:
            assert name in neutral, f'unclassified leg {leg}; declare its ownership'

if selected in {'all', 'baseline'}:
    for plan in sorted((root / 'plan').glob('*.env')):
        env = dict(os.environ)
        env.pop('FIELD_MIN_DRIVES', None)
        output = subprocess.check_output(['bash', '-c', 'source "$1"; printf "%s\\n" "$PLAN_LIBRARY" "${FIELD_MIN_DRIVES:-}"; env | grep "^FIELD_MIN_DRIVES=" >/dev/null || exit 2; printf "%s\\n" "${PLAN_LEGS[@]}"', 'plan-test', str(plan)], env=env, text=True)
        library, minimum, *legs = output.splitlines()
        check(legs, library, minimum)
        print(f'PASS: {plan.name} daemon ownership and single-drive setting')
    # Refused cases include reset and direct TEST UNIT READY while the daemon is up.
    for legs, minimum in [(['fatal 03-bringup.sh', 'fatal 05-reset-state.sh'], '1'), (['fatal 00-preflight.sh --session-epoch-a', 'fatal 03-bringup.sh', 'fatal 05-reset-state.sh'], '1'), (['fatal 00-preflight.sh --session-epoch-a', 'fatal 09-media-ready.sh --count 4 --no-wait', 'fatal 05-reset-state.sh'], '1'), (['fatal 00-preflight.sh --session-epoch-a'], '1'), (['fatal 03-bringup.sh', 'fatal 09-media-ready.sh --count 4 --no-wait'], '1'), (['fatal 02-discovery.sh'], '1'), (['fatal 03-bringup.sh', 'fatal 10-init-pools.sh'], '1'), (['fatal 03-bringup.sh', 'fatal 33-diag-capture.sh'], '1'), (['fatal 32-robotics.sh'], '1'), ([], '')]:
        try:
            check(legs, 'DEC418146K_LL01', minimum)
        except AssertionError:
            pass
        else:
            raise AssertionError(f'bad plan accepted: {legs}, {minimum}')

# Each refused fixture pins the assertion for its own rule.
preflight = 'fatal 00-preflight.sh --session-epoch-a'
reset = 'fatal 05-reset-state.sh'
cases = {
    'R1': ([preflight, 'continue 05-reset-state.sh'], 'reset leg must use fatal policy'),
    'R2': ([reset, preflight], 'reset leg must follow session-epoch preflight'),
    'R3': (['fatal 09-media-ready.sh --count 4 --no-wait', preflight, reset], 'session-epoch needs reset before any state-opening leg'),
    'R4': ([preflight, 'fatal 10-init-pools.sh', reset], 'session-epoch needs reset before any state-opening leg'),
    'R5': ([reset], 'reset leg must follow session-epoch preflight'),
    'A1': ([preflight, 'fatal 03-bringup.sh --stop', reset, 'fatal 09-media-ready.sh --count 4 --no-wait', 'fatal 03-bringup.sh'], None),
}
assert selected in {'all', 'baseline', *cases}, selected
for name, (legs, message) in cases.items():
    if selected not in {'all', name}:
        continue
    if message is None:
        check(legs, 'DEC418146K_LL01', '1')
    else:
        try:
            check(legs, 'DEC418146K_LL01', '1')
        except AssertionError as error:
            assert str(error) == message, f'T11 {name} wrong rule: {error}; expected {message}'
        else:
            raise AssertionError(f'T11 {name} bad plan accepted: {legs}')
    print(f'PASS: T11 {name}')
PY
