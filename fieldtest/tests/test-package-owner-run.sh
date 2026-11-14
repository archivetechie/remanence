#!/usr/bin/env bash
# Refuse unreviewed sources in a clean two-commit repository before any cargo call.
set -euo pipefail
kit="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
python3 - "$kit" <<'PY'
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

kit = Path(sys.argv[1])
with tempfile.TemporaryDirectory(prefix='package-source-') as tmp:
    root = Path(tmp)
    repo = root / 'repo'
    repo.mkdir()
    # No inherited account/config files: identity is local to this fixture.
    env = dict(os.environ, GIT_CONFIG_NOSYSTEM='1', GIT_CONFIG_GLOBAL=os.devnull,
               GIT_AUTHOR_NAME='Kit Test', GIT_AUTHOR_EMAIL='kit-test@example.invalid',
               GIT_COMMITTER_NAME='Kit Test', GIT_COMMITTER_EMAIL='kit-test@example.invalid')
    def git(*args):
        return subprocess.check_output(['git', '-C', str(repo), *args], env=env, text=True).strip()
    git('init', '-q')
    (repo / 'first').write_text('first commit\n')
    git('add', 'first')
    git('commit', '-qm', 'first')
    first = git('rev-parse', 'HEAD')
    (repo / 'fieldtest').mkdir()
    package = repo / 'fieldtest/package-owner-run.sh'
    shutil.copyfile(kit / 'package-owner-run.sh', package)
    git('add', 'fieldtest/package-owner-run.sh')
    git('commit', '-qm', 'package fixture')
    head = git('rev-parse', 'HEAD')
    assert first != head and not git('status', '--porcelain')
    bins = root / 'bin'
    bins.mkdir()
    cargo_calls = root / 'cargo-calls'
    (bins / 'cargo').write_text('#!/bin/sh\nprintf "cargo called\\n" >> "$CARGO_CALLS"\nexit 99\n')
    (bins / 'cargo').chmod(0o755)
    env.update(PATH=f"{bins}:{os.environ['PATH']}", CARGO_CALLS=str(cargo_calls))
    for argument in [first, 'unresolvable-reviewed-commit']:
        for options in [['--expect-commit', argument], ['--dry-run', '--expect-commit', argument], ['--expect-commit', argument, '--dry-run']]:
            result = subprocess.run(['bash', str(package), *options, str(root / 'out')], env=env, capture_output=True, text=True)
            assert result.returncode == 1, f'T6 mismatch refusal expected exit 1, got {result.returncode}: {result.stderr}'
            assert 'HEAD' in result.stderr and argument in result.stderr and 'refus' in result.stderr.lower(), result.stderr
            assert not cargo_calls.exists(), 'T6 mismatch reached cargo'
            assert not (root / 'out').exists(), 'T6 mismatch created output'
    for args in [[str(root / 'out')], [], ['--unknown', str(root / 'out')], ['--expect-commit']]:
        result = subprocess.run(['bash', str(package), *args], env=env, capture_output=True, text=True)
        assert result.returncode == 2 and 'usage:' in result.stderr and '--expect-commit' in result.stderr, result.stderr
        assert not cargo_calls.exists(), 'T6 invalid arguments reached cargo'
    print('PASS: T6 resolved mismatch, unresolvable source, both option orders and required expected commit')
PY
