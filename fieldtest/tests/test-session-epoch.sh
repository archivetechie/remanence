#!/usr/bin/env bash
# Exercise the owner orchestration without any rem/drive execution. The runner
# is a phase spy, not a CLI-output stub. Native/receipt fixtures use real printers.
set -euo pipefail
repo="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)"
python3 - "$repo" <<'PY'
import importlib.util, json, os, pathlib, shutil, subprocess, sys, tempfile
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
    # Preflight rejects every present chaos variable (including empty values)
    # and the missing field opt-in before attestation, capabilities or any CLI.
    clean_env={k:v for k,v in env.items() if k not in ('REM_CHAOS_ENABLED','REM_CHAOS_ALLOW_REAL','REM_CHAOS_STATE','REM_FIELD_ALLOW_FAIL')}
    preflight=['bash',str(scripts/'session-epoch-preflight.sh')]
    for key in ['REM_CHAOS_ENABLED','REM_CHAOS_ALLOW_REAL','REM_CHAOS_STATE']:
        for value in ['', '1']:
            result=subprocess.run(preflight,env=dict(clean_env,REM_FIELD_ALLOW_FAIL='1',**{key:value}),capture_output=True,text=True)
            assert result.returncode==2 and key in result.stderr,(key,result.stderr)
    result=subprocess.run(preflight,env=clean_env,capture_output=True,text=True)
    assert result.returncode==2 and 'REM_FIELD_ALLOW_FAIL=1' in result.stderr
    # Run the real plan/orchestrator with script/CLI environment spies only.
    pkg=home/'pkg'; (pkg/'scripts').mkdir(parents=True); (pkg/'bin').mkdir(); (pkg/'plan').mkdir()
    shutil.copy(repo/'fieldtest/run.sh',pkg/'run.sh')
    shutil.copy(scripts/'lib.sh',pkg/'scripts/lib.sh')
    shutil.copy(repo/'fieldtest/plan/session-epoch-a.env',pkg/'plan/session-epoch-a.env')
    spy=pkg/'bin/rem'
    spy.write_text('''#!/bin/bash
set -eu
for key in REM_CHAOS_ENABLED REM_CHAOS_ALLOW_REAL REM_CHAOS_STATE; do
  [[ ! -v $key ]] || exit 91
done
[[ ${REM_FIELD_ALLOW_FAIL:-} == 1 ]]
echo cli >> "$EVENTS"
'''); spy.chmod(0o755)
    for name in ['00-preflight','01-allowlist','02-discovery','03-bringup','09-media-ready','10-init-pools','41-session-closeout','42-session-evidence','90-collect-evidence','33-diag-capture']:
        script=pkg/'scripts'/(name+'.sh')
        script.write_text('''#!/bin/bash
set -eu
source "$(dirname "$0")/lib.sh"
"$FIELD_BIN_DIR/rem"
[[ $(basename "$0") != 90-collect-evidence.sh ]] || touch "$FIELD_EVIDENCE_PACK"
'''); script.chmod(0o755)
    (home/'events').unlink()
    result=subprocess.run(['bash',str(pkg/'run.sh'),'plan/session-epoch-a.env'],env=dict(clean_env,REM_FIELD_ALLOW_FAIL='1'),capture_output=True,text=True)
    assert result.returncode==0,result.stdout+result.stderr
    assert len((home/'events').read_text().splitlines())==16
    # Real cleanup runner: stop-helper precedes release, and failure leaves the
    # hold closed while still stopping samplers. The evidence operation is a spy.
    shutil.copy(scripts/'session-epoch-runner.sh',pkg/'scripts/session-epoch-runner.sh')
    shutil.copy(spy,pkg/'bin/remfield-io')
    (pkg/'scripts/session-epoch-evidence.py').write_text('''import os, pathlib, sys
step, root = sys.argv[1:3]
gate = pathlib.Path(root)/'gate'
if step == 'stop-helper':
    assert not (gate/'release').exists(), 'hold released before helper cancellation'
    if os.environ.get('FAIL_STOP'): raise SystemExit(1)
elif step == 'stop-samplers':
    assert (gate/'release').exists() == (not os.environ.get('FAIL_STOP'))
else: raise AssertionError(step)
with (pathlib.Path(root)/'cleanup-order').open('a') as f: f.write(step+'\\n')
''')
    for fail in ['', '1']:
        cleanup=home/('cleanup-'+(fail or 'ok')); (cleanup/'gate').mkdir(parents=True)
        result=subprocess.run(['bash',str(pkg/'scripts/session-epoch-runner.sh'),'cleanup','abort-mid-object','AOX032L9',str(cleanup)],env=dict(clean_env,FIELD_BIN_DIR=str(pkg/'bin'),FAIL_STOP=fail),capture_output=True,text=True)
        assert result.returncode==bool(fail),result.stderr
        assert (cleanup/'cleanup-order').read_text().splitlines()==['stop-helper','stop-samplers'] if not fail else (cleanup/'cleanup-order').read_text().splitlines()==['stop-samplers']
    # Direct entry points share the dry refusal, before binary lookup or work.
    (pkg/'DRY-RUN-NOT-FOR-HARDWARE').touch()
    for name,args in [('41-session-closeout.sh',['kill-mid-write','--barcode','AOX032L9']),('42-session-evidence.sh',['--verify-prefixes']),('session-epoch-init.sh',[])]:
        shutil.copy(scripts/name,pkg/'scripts'/name)
        result=subprocess.run(['bash',str(pkg/'scripts'/name)]+args,env=clean_env,capture_output=True,text=True)
        assert result.returncode==2 and 'dry review package' in result.stderr,(name,result.stderr)
    # The real collector must fail loudly on incomplete evidence.
    result=subprocess.run(['python3',str(scripts/'session-epoch-evidence.py'),'manifest',str(home),'abort-mid-object','AOX032L9'],capture_output=True)
    assert result.returncode!=0 and not (home/'manifest.json').exists()
    # Remove an actual printer key: malformed native evidence never passes.
    spec=importlib.util.spec_from_file_location('field_evidence',scripts/'session-epoch-evidence.py'); mod=importlib.util.module_from_spec(spec); spec.loader.exec_module(mod)
    (home/'before.json').write_text(json.dumps(before)); bad=dict(after); bad.pop('closed_out'); (home/'after.json').write_text(json.dumps(bad))
    timed=home/'expired'; (timed/'gate').mkdir(parents=True)
    (timed/'gate/timeout.json').write_text('{}')
    try: mod.verify(timed,'kill-mid-write','')
    except AssertionError as error: assert 'hold expired' in str(error)
    else: raise AssertionError('expired hold accepted')
    assert json.loads((timed/'verify-gate-state.json').read_text())=={'holding':False,'timeout':True}
    try: mod.gate_state(timed,'kill',True)
    except AssertionError as error: assert 'hold expired' in str(error)
    else: raise AssertionError('kill accepted expired hold')
    (timed/'gate/timeout.json').unlink()
    try: mod.gate_state(timed,'missing',True)
    except AssertionError as error: assert 'holding.json missing' in str(error)
    else: raise AssertionError('kill accepted missing live hold')
    try: mod.verify(home,'abort-mid-object','')
    except (KeyError,AssertionError): pass
    else: raise AssertionError('missing native closeout proof accepted')
plan=(repo/'fieldtest/plan/session-epoch-a.env').read_text()
assert '/dev/' not in plan
assert 'AOX032L9 AOX036L9 AOX038L9 AOX058L9' in plan
for leg in ['00-preflight.sh --session-epoch-a','01-allowlist.sh --verify-session-epoch-a','10-init-pools.sh --session-epoch-a','kill-mid-write','abort-mid-object','failed-append','measure-full-epoch','--verify-prefixes','--capture-sense','90-collect-evidence.sh']:
    assert leg in plan,leg
positions=[plan.index(v) for v in ['00-preflight.sh','01-allowlist.sh','09-media-ready.sh','03-bringup.sh','02-discovery.sh','10-init-pools.sh','kill-mid-write','abort-mid-object','failed-append','measure-full-epoch','--verify-prefixes','--capture-sense','90-collect-evidence.sh']]
assert positions==sorted(positions)
print('PASS: session modes, argument/allowlist refusal, ordered gates, missing-evidence refusal, failure teardown and plan')
PY
