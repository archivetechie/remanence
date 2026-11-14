#!/usr/bin/env bash
# Pin stopped-state authority members and the single daemon-up collection guard.
set -euo pipefail
scripts="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../scripts" && pwd)"
python3 - "$scripts" "${1:-all}" <<'PY'
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile

scripts = Path(sys.argv[1])
selected = sys.argv[2]
with tempfile.TemporaryDirectory(prefix='collect-evidence-') as tmp:
    root = Path(tmp)
    bins = root / 'bin'
    bins.mkdir()
    (bins / 'pgrep').write_text('''#!/usr/bin/env python3
import os, sys
assert sys.argv[1:] == ['-f', 'rem-daemon --config ' + os.environ['REMFIELD_HOME'] + '/config.toml'], sys.argv
with open(os.environ['REMFIELD_HOME'] + '/probe-calls', 'a') as f: f.write('process\\n')
sys.exit(0 if os.environ['LIVE_DAEMON'] == '1' else 1)
''')
    (bins / 'pgrep').chmod(0o755)
    authority = ['state/audit/one.remlib', 'state/audit/two.remlib', 'state/journals/tapes/one.remtape',
                 'state/authority-manifest.json', 'state/authority-reset-receipt.json']
    for case in ['stopped', 'live', 'empty']:
        if selected != 'all' and selected != case:
            continue
        home = root / case
        (home / 'evidence').mkdir(parents=True)
        (home / 'state').mkdir()
        (home / 'log').mkdir()
        (home / 'evidence/existing.txt').write_text('existing evidence')
        (home / 'log/rem-daemon.log').write_text('daemon log')
        if case != 'empty':
            for name in authority + ['state/audit/skip.txt', 'state/journals/tapes/skip.txt']:
                p = home / name
                p.parent.mkdir(parents=True, exist_ok=True)
                p.write_text(name + '\n')
        pack = home / 'pack.tar.gz'
        env = dict(os.environ, REMFIELD_HOME=str(home), FIELDTEST_LOCK_HELD='1', FIELD_EVIDENCE_PACK=str(pack),
                   PATH=f"{bins}:{os.environ['PATH']}", LIVE_DAEMON='1' if case == 'live' else '0')
        result = subprocess.run(['bash', str(scripts / '90-collect-evidence.sh')], env=env, capture_output=True, text=True, timeout=15)
        assert result.returncode == 0, result.stdout + result.stderr
        with tarfile.open(pack) as tf:
            files = {m.name for m in tf.getmembers() if m.isfile()}
            summary = tf.extractfile('evidence/SUMMARY.md').read().decode()
            expected = {'evidence/existing.txt', 'evidence/SUMMARY.md', 'rem-daemon.log'}
            if case == 'stopped':
                for name in authority:
                    assert name in files, f'T3 stopped missing {name}: {sorted(files)}'
                    assert tf.extractfile(name).read() == (home / name).read_bytes()
                expected.update(authority)
            elif case == 'live':
                assert not any(name.startswith('state/') for name in files), f'T3 live packed authority: {files}'
                assert 'logs are taken at teardown' in summary.lower(), summary
            assert files == expected, (case, files, expected)
        assert not (home / 'config.toml').exists(), 'collector must work without config'
        assert (home / 'probe-calls').read_text().splitlines() == ['process'], 'collector must decide once'
        print(f'PASS: T3 {case} pack members')
PY
