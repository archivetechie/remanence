#!/usr/bin/env bash
# Exercise the owner orchestration without any rem/drive execution. The runner
# is a phase spy, not a CLI-output stub. Native/receipt fixtures use real printers.
set -euo pipefail
repo="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)"
python3 - "$repo" <<'PY'
import importlib.util, json, os, pathlib, subprocess, sys, tempfile
sys.dont_write_bytecode=True
repo=pathlib.Path(sys.argv[1]); scripts=repo/'fieldtest/scripts'
fixtures=repo/'fieldtest/tests/fixtures'
for name in ['session-before.json','session-after.json','session-receipt.json']:
    assert (fixtures/name).is_file(), f'missing real-printer fixture {name}'
before=json.loads((fixtures/'session-before.json').read_text()); after=json.loads((fixtures/'session-after.json').read_text())
assert before['W']<before['T'] and after['W']==after['T']
assert after['closed_out']['check_before']==before['check']
with tempfile.TemporaryDirectory(prefix='session-shell-') as tmp:
    home=pathlib.Path(tmp); (home/'allowlist.txt').write_text('AOX032L9\nAOX036L9\nAOX038L9\nAOX058L9\n')
    runner=home/'runner'
    runner.write_text('''#!/bin/bash
set -eu
printf '%s\\n' "$1" >> "$EVENTS"
[[ "$1" != "${FAIL_STEP:-none}" ]]
'''); runner.chmod(0o755)
    env=dict(os.environ, REMFIELD_HOME=tmp, FIELD_SESSION_EPOCH_ARM='session-epoch-a', FIELD_SESSION_RUNNER=str(runner), EVENTS=str(home/'events'))
    command=['bash',str(scripts/'41-session-closeout.sh')]
    for args in [[],['wrong','--barcode','AOX032L9'],['kill-mid-write','--barcode','OTHER'],['kill-mid-write','--tape','AOX032L9']]:
        assert subprocess.run(command+args,env=env,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL).returncode!=0
    assert not (home/'events').exists(), 'refused arguments reached runner'
    assert subprocess.run(command+['kill-mid-write','--barcode','AOX032L9'],env=dict(env,FIELD_SESSION_EPOCH_ARM=''),capture_output=True).returncode!=0
    (home/'allowlist.txt').write_text('AOX036L9\n')
    assert subprocess.run(command+['kill-mid-write','--barcode','AOX032L9'],env=env,capture_output=True).returncode!=0
    (home/'allowlist.txt').write_text('AOX032L9\nAOX036L9\nAOX038L9\nAOX058L9\n')
    assert not (home/'events').exists(), 'unarmed or missing allowlist reached runner' 
    for mode in ['kill-mid-write','abort-mid-object','failed-append','measure-full-epoch']:
        for fail in ['', 'begin', 'prepare','dispatch','fault','closeout','verify','collect']:
            (home/'events').unlink(missing_ok=True)
            result=subprocess.run(command+[mode,'--barcode','AOX032L9'],env=dict(env,FAIL_STEP=fail),capture_output=True,text=True)
            sequence=(home/'events').read_text().splitlines()
            expected=['begin','prepare','dispatch','fault','closeout','verify','collect']
            if fail: expected=expected[:expected.index(fail)+1]
            assert sequence==expected+['cleanup'],(mode,fail,sequence,result.stderr)
            assert (result.returncode==0)==(not fail),(mode,fail,result.stderr)
    # The real collector must fail loudly on incomplete evidence.
    result=subprocess.run(['python3',str(scripts/'session-epoch-evidence.py'),'manifest',str(home),'abort-mid-object','AOX032L9'],capture_output=True)
    assert result.returncode!=0 and not (home/'manifest.json').exists()
    # Remove an actual printer key: malformed native evidence never passes.
    spec=importlib.util.spec_from_file_location('field_evidence',scripts/'session-epoch-evidence.py'); mod=importlib.util.module_from_spec(spec); spec.loader.exec_module(mod)
    (home/'before.json').write_text(json.dumps(before)); bad=dict(after); bad.pop('closed_out'); (home/'after.json').write_text(json.dumps(bad))
    try: mod.verify(home,'abort-mid-object','')
    except (KeyError,AssertionError): pass
    else: raise AssertionError('missing native closeout proof accepted')
plan=(repo/'fieldtest/plan/session-epoch-a.env').read_text()
assert '/dev/' not in plan
assert 'AOX032L9 AOX036L9 AOX038L9 AOX058L9' in plan
for leg in ['00-preflight.sh --session-epoch-a','01-allowlist.sh --verify-session-epoch-a','10-init-pools.sh --session-epoch-a','kill-mid-write','abort-mid-object','failed-append','measure-full-epoch','--verify-prefixes','--capture-sense','90-collect-evidence.sh']:
    assert leg in plan,leg
positions=[plan.index(v) for v in ['00-preflight.sh','01-allowlist.sh','02-discovery.sh','03-bringup.sh','09-media-ready.sh','10-init-pools.sh','kill-mid-write','abort-mid-object','failed-append','measure-full-epoch','--verify-prefixes','--capture-sense','90-collect-evidence.sh']]
assert positions==sorted(positions)
print('PASS: session modes, argument/allowlist refusal, ordered gates, missing-evidence refusal, failure teardown and plan')
PY
