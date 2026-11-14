#!/usr/bin/env bash
# Exercise the kit's supported whole-state reset and refusal without device I/O.
set -euo pipefail
scripts="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../scripts" && pwd)"
python3 - "$scripts" "${1:-all}" <<'PY'
import fcntl
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
if os.environ['RESET_MODE'] == 'missing-manifest':
    print('catalog reset ok')
    sys.exit(0)
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
    for case in ['a', 'a-fallback', 'b', 'c', 'outside', 'home', 'kitdir', 'lock', 'missing-manifest', 'args', 'unreadable']:
        if selected != 'all' and selected != case:
            continue
        home = root / case
        (home / 'state').mkdir(parents=True)
        if case == 'a-fallback':
            scratch = home / 'ram'
            scratch.mkdir()
            (home / 'spool').symlink_to(scratch, target_is_directory=True)
        if case in {'outside', 'home', 'kitdir'}:
            scratch = root / 'outside-spool' if case == 'outside' else home if case == 'home' else home / 'evidence'
            scratch.mkdir(exist_ok=True)
            if case == 'kitdir':
                (scratch / 'earlier-leg.json').write_text('{"keep": true}\n')
            (home / 'spool').symlink_to(scratch, target_is_directory=True)
        if case == 'unreadable':
            # The artifact cannot be written, so the FAIL path reads no output.
            (home / 'evidence/05-reset-state').mkdir(parents=True)
            (home / 'evidence/05-reset-state').chmod(0o555)
        (home / 'spool/nested').mkdir(parents=True)
        (home / 'spool/nested/data').write_text('scratch')
        (home / 'spool/.hidden').write_text('hidden scratch')
        (home / 'state/selected-library').write_bytes(b'FALLBACK\n')
        (home / 'state/kit-sentinel').write_text('keep')
        (home / 'config.toml').write_bytes(b'# existing config, must stay byte-identical on refusal\n')
        before = {p: (home / p).read_bytes() for p in ['config.toml', 'state/selected-library']}
        env = dict(os.environ, REMFIELD_HOME=str(home), FIELD_BIN_DIR=str(bins), FIELDTEST_LOCK_HELD='1',
                   PATH=f"{bins}:{os.environ['PATH']}", LIVE_DAEMON='1' if case == 'b' else '0',
                   RESET_MODE='fail' if case in {'c', 'unreadable'} else 'receipt' if case == 'a' else 'missing-manifest' if case == 'missing-manifest' else 'no-receipt')
        env.pop('FIELDTEST_LIBRARY_SERIAL', None)
        if case != 'a-fallback':
            env['FIELDTEST_LIBRARY_SERIAL'] = 'DEC418146K_LL01'
        command = ['bash', str(scripts / '05-reset-state.sh')]
        if case == 'args':
            # --selftest makes the lock wrapper skip the lock: the leg must refuse any argument before it writes.
            env.pop('FIELDTEST_LOCK_HELD', None)
            for argument, expected in [('--selftest', 2), ('extra', 2), ('--help', 0)]:
                result = subprocess.run(command + [argument], env=env, capture_output=True, text=True, timeout=15)
                assert result.returncode == expected, (f'T13 {argument} exit', result)
                assert not (home / 'calls').exists(), f'T13 {argument} reached the reset'
                assert all((home / p).read_bytes() == value for p, value in before.items()), f'T13 {argument} rewrote kit config or selection'
                assert (home / 'spool/nested/data').read_text() == 'scratch' and (home / 'spool/.hidden').exists(), f'T13 {argument} touched the spool'
            print(f'PASS: T2({case}) reset-state')
            continue
        if case == 'lock':
            # The kit's work lock uses the same flock protocol as test-read-rate.
            env.pop('FIELDTEST_LOCK_HELD', None)
            with (home / 'work.lock').open('a') as lock:
                fcntl.flock(lock, fcntl.LOCK_EX)
                process = subprocess.Popen(command, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
                try:
                    try:
                        output = process.communicate(timeout=5)
                    except subprocess.TimeoutExpired:
                        output = None
                    assert output is None and process.poll() is None, 'T8 reset ran without waiting for work.lock'
                    assert not (home / 'calls').exists(), 'T8 reset called rem while work.lock held'
                except BaseException:
                    if process.poll() is None:
                        process.kill()  # Only this exact child PID, before it can take the lock.
                    process.communicate(timeout=5)
                    raise
                finally:
                    fcntl.flock(lock, fcntl.LOCK_UN)
            try:
                stdout, stderr = process.communicate(timeout=15)
            except BaseException:
                if process.poll() is None:
                    process.kill()
                process.communicate(timeout=5)
                raise
            result = subprocess.CompletedProcess(command, process.returncode, stdout, stderr)
        else:
            result = subprocess.run(command, env=env, capture_output=True, text=True, timeout=15)
        if case == 'unreadable':
            (home / 'evidence/05-reset-state').chmod(0o755)
        if case in {'outside', 'home', 'kitdir'}:
            assert (home / 'spool/nested/data').is_file(), f'T7({case}) spool target contents removed'
            assert (home / 'spool/nested/data').read_bytes() == b'scratch', f'T7({case}) spool contents changed'
            assert (home / 'spool/.hidden').read_bytes() == b'hidden scratch', f'T7({case}) hidden spool contents changed'
            assert (home / 'state/kit-sentinel').read_text() == 'keep', f'T7({case}) kit contents removed'

        calls = [json.loads(line) for line in (home / 'calls').read_text().splitlines()] if (home / 'calls').exists() else []
        rows = [json.loads(line) for line in (home / 'evidence/records.jsonl').read_text().splitlines()] if (home / 'evidence/records.jsonl').exists() else []
        if case == 'b':
            assert result.returncode == 1, ('T2(b) live daemon must refuse', result)
            assert not calls, 'T2(b) live daemon reached reset'
            assert '03-bringup.sh --stop' in result.stderr, result.stderr
            assert all((home / p).read_bytes() == value for p, value in before.items()), 'T2(b) refusal rewrote kit config or selection'
            assert (home / 'spool/nested/data').read_text() == 'scratch' and (home / 'spool/.hidden').exists()
            assert any(r['status'] == 'FAIL' for r in rows), rows
        elif case == 'unreadable':
            assert result.returncode == 2, ('T14 unreadable reset output must keep the reset exit code', result)
            assert len(calls) == 1, calls
            assert any(r['status'] == 'FAIL' and r['test_id'] == 'reset' and 'exit 2' in r['summary'] and 'unreadable' in r['summary'] for r in rows), ('T14 no FAIL record for an unreadable reset output', rows)
            assert (home / 'spool/nested/data').read_text() == 'scratch' and (home / 'spool/.hidden').exists()
        elif case == 'c':
            assert result.returncode != 0, 'T2(c) failed reset accepted'
            assert len(calls) == 1, calls
            assert (home / 'spool/nested/data').exists() and (home / 'spool/.hidden').exists()
            assert any(r['status'] == 'FAIL' and 'reset failed last line' in r['summary'] for r in rows), rows
        elif case == 'missing-manifest':
            assert result.returncode != 0, 'T9(b) missing manifest accepted'
            assert len(calls) == 1, calls
            assert any(r['status'] == 'FAIL' and 'authority-manifest.json' in r['summary'] for r in rows), ('T9(b) no manifest copy FAIL record', rows)
            assert not any(r['status'] == 'PASS' and r['test_id'] == 'reset' for r in rows), rows
            assert (home / 'spool/nested/data').read_text() == 'scratch' and (home / 'spool/.hidden').exists(), 'T9(b) spool touched after failed manifest copy'
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
            if case == 'kitdir':
                assert (scratch / 'earlier-leg.json').read_text() == '{"keep": true}\n', 'T7(kitdir) earlier evidence removed'
            if case in {'outside', 'home', 'kitdir'}:
                resolved = str(scratch.resolve())
                assert (home / 'spool').is_symlink() and scratch.is_dir(), f'T7({case}) spool link or target lost'
                assert any(r['status'] == 'WARN' and resolved in r['summary'] for r in rows), ('T7 warning missing resolved spool', rows)
                assert any(r['status'] == 'PASS' and r['test_id'] == 'reset' and 'spool left' in r['summary'] and 'emptied' not in r['summary'] for r in rows), ('T7 PASS must say spool left', rows)
            else:
                assert (home / 'spool').is_dir() and not list((home / 'spool').iterdir()), 'T2(a) spool not emptied'
            if case == 'a-fallback':
                assert (home / 'spool').is_symlink() and scratch.is_dir(), 'T7(a) spool link or target lost'
            assert (home / 'state/kit-sentinel').read_text() == 'keep'
            assert any(r['status'] == 'PASS' for r in rows), rows
            artifacts = [Path(r['detail_path']) for r in rows if r.get('detail_path')]
            assert artifacts, 'T9(a) missing reset artifact'
            for artifact in artifacts:
                try:
                    payload = json.loads(artifact.read_text())
                except (ValueError, OSError) as error:
                    raise AssertionError(f'T9(a) reset artifact is not JSON: {artifact}') from error
                assert payload['exit_code'] == 0 and 'catalog reset ok' in payload['stdout'], ('T9(a)', payload)
        print(f'PASS: T2({case}) reset-state')
PY
