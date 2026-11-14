#!/usr/bin/env bash
# Exercise the kit's supported whole-state reset and refusal without device I/O.
set -euo pipefail
scripts="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../scripts" && pwd)"
python3 - "$scripts" "${1:-all}" <<'PY'
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import tomllib

scripts = Path(sys.argv[1])
selected = sys.argv[2]
assert (scripts / '05-reset-state.sh').is_file(), '05-reset-state.sh does not exist'
with tempfile.TemporaryDirectory(prefix='reset-state-') as tmp:
    root = Path(tmp)
    bins = root / 'bin'
    bins.mkdir()
    rem = bins / 'rem'
    rem.write_text('''#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
args = sys.argv[1:]
home = Path(os.environ['REMFIELD_HOME'])
if args[0] == 'daemon' and args[-1] == 'health':
    sys.exit(0)
with (home / 'calls').open('a') as f:
    f.write(json.dumps(args) + '\\n')
assert args == ['reset-catalog', '--config', str(home / 'config.toml'), '--i-understand-this-erases-the-catalog'], args
if os.environ['RESET_MODE'] == 'fail':
    print('reset failed first line', flush=True)
    print('reset failed last line', file=sys.stderr)
    sys.exit(2)
(home / 'state/authority-manifest.json').write_text('{"manifest":"new"}\\n')
if os.environ['RESET_MODE'] == 'receipt':
    (home / 'state/authority-reset-receipt.json').write_text('{"receipt":"new"}\\n')
print('catalog reset ok')
''')
    pgrep = bins / 'pgrep'
    pgrep.write_text('''#!/usr/bin/env python3
import os, sys
assert sys.argv[1:] == ['-f', 'rem-daemon --config ' + os.environ['REMFIELD_HOME'] + '/config.toml'], sys.argv
if os.environ['LIVE_DAEMON'] == '1':
    print('12345')
    sys.exit(0)
sys.exit(1)
''')
    rem.chmod(0o755)
    pgrep.chmod(0o755)
    for case in ['a', 'a-fallback', 'b', 'c']:
        if selected != 'all' and selected != case:
            continue
        home = root / case
        (home / 'state').mkdir(parents=True)
        if case == 'a-fallback':
            scratch = root / 'scratch-spool'
            scratch.mkdir()
            (home / 'spool').symlink_to(scratch, target_is_directory=True)
        (home / 'spool/nested').mkdir(parents=True)
        (home / 'spool/nested/data').write_text('scratch')
        (home / 'spool/.hidden').write_text('hidden scratch')
        (home / 'state/selected-library').write_bytes(b'FALLBACK\n')
        (home / 'state/kit-sentinel').write_text('keep')
        (home / 'config.toml').write_bytes(b'# existing config, must stay byte-identical on refusal\n')
        before = {p: (home / p).read_bytes() for p in ['config.toml', 'state/selected-library']}
        env = dict(os.environ, REMFIELD_HOME=str(home), FIELD_BIN_DIR=str(bins), FIELDTEST_LOCK_HELD='1',
                   PATH=f"{bins}:{os.environ['PATH']}", LIVE_DAEMON='1' if case == 'b' else '0',
                   RESET_MODE='fail' if case == 'c' else 'receipt' if case == 'a' else 'no-receipt')
        env.pop('FIELDTEST_LIBRARY_SERIAL', None)
        if case != 'a-fallback':
            env['FIELDTEST_LIBRARY_SERIAL'] = 'DEC418146K_LL01'
        result = subprocess.run(['bash', str(scripts / '05-reset-state.sh')], env=env, capture_output=True, text=True, timeout=15)
        calls = [json.loads(line) for line in (home / 'calls').read_text().splitlines()] if (home / 'calls').exists() else []
        rows = [json.loads(line) for line in (home / 'evidence/records.jsonl').read_text().splitlines()]
        if case == 'b':
            assert result.returncode == 1, ('T2(b) live daemon must refuse', result)
            assert not calls, 'T2(b) live daemon reached reset'
            assert '03-bringup.sh --stop' in result.stderr, result.stderr
            assert all((home / p).read_bytes() == value for p, value in before.items()), 'T2(b) refusal rewrote kit config or selection'
            assert (home / 'spool/nested/data').read_text() == 'scratch' and (home / 'spool/.hidden').exists()
            assert any(r['status'] == 'FAIL' for r in rows), rows
        elif case == 'c':
            assert result.returncode != 0, 'T2(c) failed reset accepted'
            assert len(calls) == 1, calls
            assert (home / 'spool/nested/data').exists() and (home / 'spool/.hidden').exists()
            assert any(r['status'] == 'FAIL' and 'reset failed last line' in r['summary'] for r in rows), rows
        else:
            assert result.returncode == 0, result.stdout + result.stderr
            assert calls == [['reset-catalog', '--config', str(home / 'config.toml'), '--i-understand-this-erases-the-catalog']], calls
            serial = 'FALLBACK' if case == 'a-fallback' else 'DEC418146K_LL01'
            config = tomllib.loads((home / 'config.toml').read_text())
            assert config['libraries'] == [{'serial': serial}], config
            assert config['daemon']['state_dir'] == str(home / 'state'), config
            assert (home / 'state/selected-library').read_text() == serial + '\n'
            assert (home / 'evidence/authority-manifest.json').read_bytes() == (home / 'state/authority-manifest.json').read_bytes()
            if case == 'a':
                assert (home / 'evidence/authority-reset-receipt.json').read_bytes() == (home / 'state/authority-reset-receipt.json').read_bytes()
            else:
                assert not (home / 'evidence/authority-reset-receipt.json').exists()
            assert (home / 'spool').is_dir() and not list((home / 'spool').iterdir()), 'T2(a) spool not emptied'
            assert (home / 'state/kit-sentinel').read_text() == 'keep'
            assert any(r['status'] == 'PASS' for r in rows), rows
            artifacts = [Path(r['detail_path']) for r in rows if r.get('detail_path')]
            assert any('catalog reset ok' in p.read_text() for p in artifacts), artifacts
        print(f'PASS: T2({case}) reset-state')
PY
