#!/usr/bin/env bash
# Model durable state rollback against two physical tape counters, entirely with
# isolated CLI/process stubs. No production binary or device is reachable.
set -euo pipefail
scripts="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../scripts" && pwd)"
python3 - "$scripts" "${1:-all}" <<'PY'
import fcntl
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import uuid

scripts = Path(sys.argv[1])
selected = sys.argv[2]
T = 'aabbccdd-1111-4222-8333-123456abcdef'
T2 = 'ffeeddcc-1111-4222-8333-123456abcdef'
# These counters and saved object bytes model physical tapes and never roll back.
stub = r'''#!/usr/bin/env python3
import fcntl, hashlib, json, os, sys, uuid
from pathlib import Path
home = Path(os.environ['REMFIELD_HOME'])
state = home / 'state'
a = sys.argv[1:]
case = os.environ['RESTORE_CASE']
T = 'aabbccdd-1111-4222-8333-123456abcdef'
T2 = 'ffeeddcc-1111-4222-8333-123456abcdef'
known = json.loads((state / 'known.json').read_text())
physical = json.loads((home / 'physical.json').read_text())

def log(event, **extra):
    with (home / 'calls.jsonl').open('a') as stream:
        stream.write(json.dumps(dict(event=event, known=known, physical=physical, **extra)) + '\n')

def write(label, tape, data):
    physical[tape] += 1
    known[tape] = physical[tape]
    (home / 'physical.json').write_text(json.dumps(physical))
    (state / 'known.json').write_text(json.dumps(known))
    (state / 'calibration/epoch').write_text(str(known[T]))
    if label == 'B':
        (state / 'after-B').write_text('newer-only')
        if case == 'debt-newer':
            top = json.loads((home / 'top.json').read_text())
            top['data']['closeout_debt'] = [{'tape_uuid': T2}]
            (home / 'top.json').write_text(json.dumps(top))
    object_id = str(uuid.uuid5(uuid.NAMESPACE_DNS, label))
    (home / 'media' / object_id).write_bytes(data)
    return object_id

name = Path(sys.argv[0]).name
if name == 'pgrep':
    assert a == ['-f', 'rem-daemon --config ' + str(home / 'config.toml')], a
    sys.exit(0 if (home / 'live').exists() else 1)
if name == '03-bringup.sh':
    action = 'stop' if a == ['--stop'] else 'start'
    assert a == (['--stop'] if action == 'stop' else []), a
    log(action)
    if case == 'stop-fail' and known[T] == 2 and action == 'stop':
        sys.exit(3)
    if case == 'start-fail' and known[T] == 1 and action == 'start':
        sys.exit(4)
    if action == 'stop':
        if case != 'd':
            (home / 'live').unlink(missing_ok=True)
        rows = [dict(script='03-bringup', test_id='stop', status='PASS'),
                dict(script='03-bringup', test_id='stop-inventory', status='PASS')]
        if case == 'warn':
            rows.append(dict(script='03-bringup', test_id='stop', status='WARN'))
        rows.append(dict(script='another-script', test_id='stop', status='PASS'))
        with (home / 'evidence/records.jsonl').open('a') as stream:
            stream.write(''.join(json.dumps(row) + '\n' for row in rows))
    else:
        with (state / 'state.lock').open('a') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        assert (state / 'state.lock').stat().st_ino == int((home / 'lock-inode').read_text()), 'state lock inode replaced'
        if not (state / 'write-fence').exists():
            top = json.loads((home / 'top.json').read_text())
            if 'active_tape_io_fences' in top['data']:
                top['data']['active_tape_io_fences'] = []
            (home / 'top.json').write_text(json.dumps(top))
        (home / 'live').touch()
        (home / 'config.toml').write_text('# regenerated on start\n')
    sys.exit(0)
assert (home / 'live').exists(), 'CLI reached stopped daemon'
assert '--endpoint' in a and a[a.index('--endpoint')+1] == 'unix:' + str(home / 'rem.sock'), a
if name == 'remfield-io':
    action = a[2]
    if action == 'list':
        assert a[3:] == ['--pool', 'fieldtest-a'], a
        log('list')
        print(json.dumps({'tapes': [{'tape_uuid': T, 'state': 'ready'}]}))
    elif action == 'write':
        assert a[3:6] == ['--library', 'SELECTED', '--file'] and a[-2:] == ['--pool', 'fieldtest-a'], a
        path = Path(a[6])
        label = path.stem
        assert label in {'A', 'B'}, 'unexpected pool write under older'
        tape = T2 if label == 'B' and case == 'e' else T
        log('pool-' + label, tape=tape)
        if case == 'write-' + label or (case == 'k' and label == 'A'):
            print('injected write error', file=sys.stderr)
            sys.exit(2)
        object_id = write(label, tape, path.read_bytes())
        print(json.dumps(dict(object_id=object_id, tape_uuid=None if case == 'null-' + label else uuid.UUID(tape).hex, pool_id='fieldtest-a')))
    elif action == 'read':
        assert a[3] == '--object' and a[5] == '--out' and len(a) == 7, a
        locator = json.loads(a[4])
        label = next(label for label in 'ABC' if locator['object_id'] == str(uuid.uuid5(uuid.NAMESPACE_DNS, label)))
        expected_tape = T2 if case == 'e' and label == 'B' else T
        assert locator['pool_id'] == 'fieldtest-a' and uuid.UUID(locator['tape_uuid']) == uuid.UUID(expected_tape), locator
        log('read-' + label, fenced=(state / 'write-fence').exists())
        older = known[T] == 1 and physical[T] == 2
        if case == 'read-' + label or (case == 'read-A-older' and older):
            print('injected read error', file=sys.stderr)
            sys.exit(2)
        data = (home / 'media' / locator['object_id']).read_bytes()
        if case == 'sha-' + label or (case == 'sha-A-older' and older):
            data += b'corruption'
        Path(a[6]).write_bytes(data)
        print(json.dumps({'content_sha256': hashlib.sha256(data).hexdigest()}))
    else:
        raise AssertionError(a)
elif name == 'rem':
    if a[0] == 'top':
        assert a[1:] == ['--endpoint', 'unix:' + str(home / 'rem.sock'), '--once', '--json'], a
        log('top')
        print((home / 'top.json').read_text())
    elif a[0] == 'catalog':
        assert a[1:] == ['--endpoint', 'unix:' + str(home / 'rem.sock'), 'tapes', '--json'], a
        log('catalog')
        print(json.dumps({'tapes': known}))
    else:
        assert a[:3] == ['put', '--endpoint', 'unix:' + str(home / 'rem.sock')], a
        assert len(a) == 9 and a[3] == '--tape' and uuid.UUID(a[4]) == uuid.UUID(T), a
        assert a[5:8] == ['--pool', 'fieldtest-a', '--json'], a
        path = Path(a[8])
        assert path.name == 'C.bin', path
        older = physical[T] > known[T]
        log('put-C', older=older, source_sha256=hashlib.sha256(path.read_bytes()).hexdigest())
        assert (state / 'calibration/epoch').read_text() == str(known[T]), 'calibration did not roll back'
        assert (state / 'after-B').exists() == (known[T] >= 2), 'restore did not remove newer-only entries'
        if older and case != 'c':
            print(f'writing 1 file ({path.stat().st_size} bytes) to tape {T} ...', file=sys.stderr)
            if case == 'b':
                message = 'daemon returned failed_precondition: another reason'
            else:
                if case != 'g':
                    (state / 'write-fence').write_text('AppendAdmissionRefused')
                top = json.loads((home / 'top.json').read_text())
                if case not in {'g', 'h', 'missing-fences'}:
                    top['data']['active_tape_io_fences'] = [{'tape_uuid': T}]
                (home / 'top.json').write_text(json.dumps(top))
                refusal_tape = T2 if case == 'wrong-refusal-tape' else T
                guidance = '' if case == 'missing-guidance' else 'To keep the data, start from a newer copy of the state directory. rem tape supersede-tail ...'
                message = f'daemon returned failed_precondition: TailBeyondKnownState: tape {refusal_tape}, expected EOD LBA 1, found [2]. {guidance}'
                if case == 'g':
                    message += '; durable library fence failed: injected failure'
            envelope = dict(schema='rem.error.v1', code='failed_precondition', message=message)
            if case == 'wrong-schema':
                envelope['schema'] = 'rem.other.v1'
            if case == 'wrong-code':
                envelope['code'] = 'internal'
            if case == 'plain-refusal':
                print(message, file=sys.stderr)
            else:
                print(json.dumps(envelope), file=sys.stderr)
            sys.exit(1)
        if case == 'newer-put-fail':
            print('injected newer put error', file=sys.stderr)
            sys.exit(2)
        # rem put constructs an object; its bytes deliberately differ from C's source.
        data = b'constructed-object\n' + path.read_bytes()
        object_id = write('C', T, data)
        copies = [{'tape_uuid': T}]
        if case == 'newer-off-T':
            copies.append({'tape_uuid': T2})
        print(json.dumps(dict(objects_committed=0 if case == 'newer-empty' else 1,
              objects=[dict(object_id=object_id, content_sha256=hashlib.sha256(data).hexdigest(), copies=copies)])))
else:
    raise AssertionError((name, a))
'''

copy_stub = r'''#!/usr/bin/env python3
"""Copy under the state lock, then inject a same-size staged-content mismatch."""
import fcntl
import os
from pathlib import Path
import subprocess
import sys

home = Path(os.environ['REMFIELD_HOME'])
with (home / 'state/state.lock').open('a') as lock:
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        pass
    else:
        raise AssertionError('copy ran without the state lock held')
result = subprocess.run([os.environ['RESTORE_REAL_CP'], *sys.argv[1:]])
if result.returncode == 0 and os.environ['RESTORE_CASE'] == 'j':
    destination = Path(sys.argv[-1])
    if destination.name.startswith('incoming-'):
        path = destination / '.hidden'
        data = path.read_bytes()
        path.write_bytes(bytes([data[0] ^ 1]) + data[1:])
sys.exit(result.returncode)
'''

guard_stub = r'''#!/usr/bin/env python3
"""Inject only the selected restore move, device mismatch, or free-space boundary."""
import json
import os
from pathlib import Path
import subprocess
import sys

home = Path(os.environ['REMFIELD_HOME'])
state = home / 'state'
case = os.environ['RESTORE_CASE']
name = Path(sys.argv[0]).name
args = sys.argv[1:]
real = os.environ['RESTORE_REAL_' + name.upper()]
if (name == 'mv' and case == 'l' and len(args) == 3 and args[0] == '--'
        and Path(args[1]).parent.name == 'incoming-older' and Path(args[2]) == state):
    counter = home / 'incoming-moves'
    count = int(counter.read_text()) + 1 if counter.exists() else 1
    counter.write_text(str(count))
    if count == 2:
        print('injected failure on second incoming-older move', file=sys.stderr)
        sys.exit(1)
if name == 'stat' and case == 'm' and args == ['-c', '%d', '--', str(state)]:
    device = int(subprocess.check_output([real, '-c', '%d', '--', str(home)], text=True))
    print(device + 1)
    sys.exit(0)
if name == 'df' and ((case == 'n' and args == ['-PB1', '--', str(home)])
                     or (case == 'o' and args == ['-PB1', '--', str(home / 'spool')])):
    used = int(subprocess.check_output([os.environ['RESTORE_REAL_DU'], '-sb', '--', str(state)], text=True).split()[0])
    available = 1073741824 + 4 * used - 1
    with (home / 'space-probes.jsonl').open('a') as stream:
        stream.write(json.dumps(dict(used=used, available=available)) + '\n')
    total = 2 * available
    print('Filesystem 1-blocks Used Available Capacity Mounted on')
    print(f'restore-test {total} {total - available} {available} 50% {args[-1]}')
    sys.exit(0)
os.execv(real, [real, *args])
'''

cases = ['a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k', 'l', 'm', 'n', 'o', 'warn', 'stop-fail', 'start-fail',
         'write-A', 'write-B', 'null-A', 'null-B', 'read-A', 'read-B', 'sha-A', 'sha-B',
         'read-A-older', 'sha-A-older', 'wrong-refusal-tape', 'missing-guidance',
         'newer-put-fail', 'newer-off-T', 'newer-empty', 'read-C', 'args',
         'wrong-schema', 'wrong-code', 'plain-refusal', 'missing-fences', 'missing-debt', 'debt-newer']
assert selected in {'all', *cases}, selected
with tempfile.TemporaryDirectory(prefix='restore-tail-') as temporary:
    root = Path(temporary)
    # Mirror scripts/ depth so neither binary resolver can reach the real repo.
    kit_scripts = root / 'isolated/fieldtest/scripts'
    shutil.copytree(scripts, kit_scripts)
    bins = root / 'bin'
    bins.mkdir()
    for name in ['rem', 'remfield-io', 'pgrep']:
        (bins / name).write_text(stub)
        (bins / name).chmod(0o755)
    (bins / 'cp').write_text(copy_stub)
    (bins / 'cp').chmod(0o755)
    for name in ['mv', 'stat', 'df']:
        (bins / name).write_text(guard_stub)
        (bins / name).chmod(0o755)
    (kit_scripts / '03-bringup.sh').write_text(stub)
    (kit_scripts / '03-bringup.sh').chmod(0o755)
    for case in cases:
        if selected not in {'all', case}:
            continue
        home = root / case
        state = home / 'state'
        for name in ['journals', 'audit', 'index', 'cache/tapes', 'calibration', 'native-dir']:
            (state / name).mkdir(parents=True, exist_ok=True)
        (home / 'media').mkdir()
        (home / 'evidence').mkdir()
        (home / 'live').touch()
        (home / 'config.toml').write_text('# initial config\n')
        (state / 'known.json').write_text(json.dumps({T: 0, T2: 0}))
        (home / 'physical.json').write_text(json.dumps({T: 0, T2: 0}))
        (state / 'calibration/epoch').write_text('0')
        (state / '.hidden').write_bytes(b'hidden daemon state')
        (state / '.hidden').chmod(0o640)
        (state / 'native-dir/file').write_bytes(b'native nested state')
        (state / 'native-link').symlink_to('native-dir')
        if case == 'o':
            (state / 'padding').write_bytes(b'\0' * 65536)
        (state / 'state.lock').touch()
        (home / 'lock-inode').write_text(str((state / 'state.lock').stat().st_ino))
        top = json.loads((scripts.parent / 'tests/fixtures/stop-top.json').read_text())
        if case == 'i':
            top['data']['closeout_debt'] = [{'tape_uuid': T2}]
        if case == 'missing-debt':
            del top['data']['closeout_debt']
        if case == 'missing-fences':
            del top['data']['active_tape_io_fences']
        (home / 'top.json').write_text(json.dumps(top))
        kit_files = {'selected-library': b'SELECTED\n', 'run.pid': b'123456\n', 'soak.pid': b'654321\n',
                     'fence-counters.one': b'{"kit":1}\n', 'fence-counters.two': b'{"kit":2}\n',
                     'media-readiness.jsonl': b'{"kit":"media"}\n'}
        for name, data in kit_files.items():
            (state / name).write_bytes(data)
        before = {p.relative_to(state): p.read_bytes() for p in state.rglob('*') if p.is_file()}
        env = dict(os.environ, REMFIELD_HOME=str(home), FIELD_BIN_DIR=str(bins), REMFIELD_ENV='vtl',
                   FIELDTEST_LOCK_HELD='1', FIELD_RESTORE_TAIL_MB='0.001', RESTORE_CASE=case,
                   RESTORE_REAL_CP=shutil.which('cp'),
                   FIELD_MIN_FREE_GB='0', FIELD_MIN_FREE_PCT='0', PATH=f"{bins}:{os.environ['PATH']}")
        for name in ['mv', 'stat', 'df', 'du']:
            env['RESTORE_REAL_' + name.upper()] = shutil.which(name)
        if case == 'o':
            env['FIELD_MIN_FREE_GB'] = '1'
        for name in ['FIELDTEST_LIBRARY_SERIAL', 'FIELDTEST_FENCE_COUNTERS_FILE', 'REM_CHAOS_ENABLED', 'REM_CHAOS_ALLOW_REAL', 'REM_CHAOS_STATE']:
            env.pop(name, None)
        command = ['bash', str(kit_scripts / '14-restore-tail.sh')]
        if case == 'args':
            for arg in ['--help', '--selftest', 'extra']:
                result = subprocess.run(command + [arg], env=env, capture_output=True, text=True, timeout=20)
                assert result.returncode != 0, (arg, result)
                assert not (home / 'calls.jsonl').exists(), 'argument reached a CLI'
            print('PASS: args refused')
            continue
        with (state / 'state.lock').open('a') as lock:
            if case == 'f':
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            result = subprocess.run(command, env=env, capture_output=True, text=True, timeout=30)
        call_path = home / 'calls.jsonl'
        call_text = '' if case == 'm' and not call_path.exists() else call_path.read_text()
        calls = [json.loads(line) for line in call_text.splitlines()]
        events = [row['event'] for row in calls]
        records = [json.loads(line) for line in (home / 'evidence/records.jsonl').read_text().splitlines()]
        rows = [row for row in records if row['script'] == '14-restore-tail']
        finals = [row for row in rows if row['test_id'] == 'restore-tail-final']
        assert len(finals) == 1, (case, result, rows)
        assert (result.returncode == 0) == (case == 'a'), (case, result)
        assert finals[0]['status'] == ('PASS' if case == 'a' else 'FAIL'), (case, finals)
        assert all(row['test_id'].startswith('restore-tail-') or row['test_id'] == 'media-budget'
                   or (case == 'o' and row['test_id'] == 'disk-guard') for row in rows), rows
        for row in rows:
            if row['detail_path']:
                assert Path(row['detail_path']).exists(), (case, row)
        snapshots = list((home / 'restore-tail').glob('*'))
        newer_restore_cases = {'a', 'read-A-older', 'sha-A-older', 'newer-put-fail', 'newer-off-T', 'newer-empty', 'read-C'}
        puts = [row for row in calls if row['event'] == 'put-C']
        if case != 'l':
            known = json.loads((state / 'known.json').read_text())
        if case == 'a':
            assert events == ['list', 'pool-A', 'read-A', 'top', 'catalog', 'stop', 'start', 'pool-B', 'read-B',
                              'top', 'catalog', 'stop', 'start', 'put-C', 'top', 'read-A', 'catalog', 'stop', 'start',
                              'put-C', 'read-B', 'read-C'], events
            assert len(puts) == 2 and [p['older'] for p in puts] == [True, False], puts
            assert puts[0]['source_sha256'] == puts[1]['source_sha256'], puts
            assert [r['known'][T] for r in calls if r['event'].startswith('pool-')] == [0, 1], calls
            assert not snapshots, 'PASS left snapshots behind'
            assert not (state / 'write-fence').exists(), 'PASS left the write fence'
            assert (home / 'live').exists(), 'daemon not up on exit'
            digests = list((home / 'evidence/14-restore-tail').glob('*/read-C-sha256.json'))
            digest = json.loads(digests[0].read_text())
            assert digest['content_sha256'] == digest['read_sha256'] != puts[0]['source_sha256'], digest
            assert any(r['event'] == 'read-A' and r.get('fenced') for r in calls), calls
        if case in {'b', 'c', 'g', 'h', 'wrong-refusal-tape', 'missing-guidance', 'wrong-schema', 'wrong-code', 'plain-refusal', 'missing-fences'}:
            assert len(snapshots) == 1, snapshots
            snapshot = snapshots[0]
            expected = 'accepted' if case == 'c' else 'other'
            assert (snapshot / 'OUTCOME').read_text().strip() == expected
            evidence_outcome = list((home / 'evidence/14-restore-tail').glob('*/OUTCOME'))
            assert evidence_outcome[0].read_text().strip() == expected
            assert json.loads((snapshot / 'older/known.json').read_text())[T] == 1
            assert json.loads((snapshot / 'newer/known.json').read_text())[T] == 2
            assert not (state / 'after-B').exists(), 'unwanted C outcome restored newer'
            assert len(puts) == 1 and puts[0]['older'], calls
            assert events[-1] == ('top' if case in {'h', 'missing-fences'} else 'put-C'), 'unwanted C outcome performed another step'
            assert 'leg does nothing more; plan teardown then stops the daemon' in finals[0]['summary'], finals
            assert (home / 'live').exists(), 'unwanted C outcome changed daemon liveness'
            assert 'state in place: older' in finals[0]['summary'], finals
        if case in {'d', 'f', 'warn'}:
            assert events == ['list', 'pool-A', 'read-A', 'top', 'catalog', 'stop'], events
            assert all(not list(p.iterdir()) for p in snapshots), 'copied before stop guards passed'
            # Only the successful A write can have changed daemon state.
            expected_state = dict(before)
            expected_state[Path('known.json')] = json.dumps({T: 1, T2: 0}).encode()
            expected_state[Path('calibration/epoch')] = b'1'
            actual = {p.relative_to(state): p.read_bytes() for p in state.rglob('*') if p.is_file()}
            assert actual == expected_state, 'stop guard failure touched state'
            if case == 'd':
                assert (home / 'live').exists()
            if case == 'f':
                assert not (home / 'live').exists()
                assert any(row['test_id'] == 'restore-tail-stop-older' and row['status'] == 'FAIL'
                           and 'state lock is held or unavailable' in row['summary'] for row in rows), rows
        if case == 'e':
            assert events[-1] == 'pool-B' and not puts, events
            assert any(row['test_id'] == 'restore-tail-placement-B' and row['status'] == 'FAIL' for row in rows), rows
            assert len(snapshots) == 1 and (snapshots[0] / 'older').is_dir()
            assert not (snapshots[0] / 'newer').exists(), 'B off T advanced to newer snapshot'
            assert known == {T: 1, T2: 1}, known
        if case == 'l':
            assert len(snapshots) == 1, snapshots
            snapshot = snapshots[0]
            for name in ['older', 'newer', 'incoming-older', 'displaced-1']:
                assert (snapshot / name).is_dir(), (case, name)
            assert (home / 'incoming-moves').read_text() == '2'
            context = ('state in place: partial (restoring older; daemon down; neither copy whole in state/); '
                       f'incoming: {snapshot}/incoming-older; displaced: {snapshot}/displaced-1')
            halt = 'run no other plan with this kit home; send the pack; the snapshots stay on the host'
            for test_id in ['restore-tail-restore-older', 'restore-tail-final']:
                failures = [row for row in rows if row['test_id'] == test_id and row['status'] == 'FAIL']
                assert len(failures) == 1 and context in failures[0]['summary'] and halt in failures[0]['summary'], failures
            assert not puts and events[-1] == 'stop', events
            assert not (home / 'live').exists(), 'interrupted restore restarted daemon'
            names = lambda directory: {path.name for path in directory.iterdir()}
            assert names(snapshot / 'displaced-1') == names(snapshot / 'newer'), 'displaced entries differ from newer'
            remaining = names(snapshot / 'incoming-older')
            moved = names(state) - set(kit_files) - {'state.lock'}
            assert moved and remaining and moved.isdisjoint(remaining), (moved, remaining)
            assert moved | remaining == names(snapshot / 'older'), 'partial entries do not partition older'
        if case == 'm':
            assert not events, events
            assert 'state and home must be on the same filesystem' in finals[0]['summary'], finals
            assert 'snapshots: none yet' in finals[0]['summary'], finals
        if case in {'n', 'o'}:
            assert events == ['list', 'pool-A', 'read-A'], events
            assert len(snapshots) == 1 and not list(snapshots[0].iterdir()), 'space refusal allowed a snapshot'
            if case == 'n':
                assert any(row['test_id'] == 'restore-tail-space' and row['status'] == 'FAIL'
                           and 'four times' in row['summary'] for row in rows), rows
            else:
                assert any(row['test_id'] == 'disk-guard' and row['status'] == 'FAIL'
                           and 'restore-tail state copies' in row['summary'] for row in rows), rows
                probes = [json.loads(line) for line in (home / 'space-probes.jsonl').read_text().splitlines()]
                assert len(probes) == 2, probes
                payload_bytes = 8 * int(1048576 * 0.001)
                assert probes[0]['available'] - payload_bytes >= 1073741824, probes
                assert probes[1]['available'] - 4 * probes[1]['used'] == 1073741824 - 1, probes
                print(f'MEASURE: kit floor payload estimate={payload_bytes}; spool probes={probes}')
            assert not any(row['test_id'] == 'restore-tail-space' and row['status'] == 'PASS' for row in rows), rows
        if case in {'write-A', 'k', 'null-A', 'read-A', 'sha-A'}:
            assert 'stop' not in events and 'pool-B' not in events and not puts, events
        if case in {'write-B', 'null-B', 'read-B', 'sha-B'}:
            assert events.count('stop') == 1 and not puts, events
        if case in {'read-A-older', 'sha-A-older'}:
            assert len(puts) == 1, 'failed older A read attempted C under newer'
            assert known[T] == 2 and (state / 'after-B').exists() and not (state / 'write-fence').exists()
            assert (home / 'live').exists()
            assert events[-3:] == ['catalog', 'stop', 'start'], events
        if case == 'stop-fail':
            assert events[-1] == 'stop' and not puts and known[T] == 2, events
            assert 'state in place: current (nothing swapped)' in finals[0]['summary'], finals
        if case == 'start-fail':
            assert events[-1] == 'start' and events.count('stop') == 1 and known[T] == 1, events
            assert 'state in place: current (nothing swapped)' in finals[0]['summary'], finals
        if case.startswith('newer-') or case == 'read-C':
            assert len(puts) == 2 and puts[-1]['older'] is False and len(snapshots) == 1, calls
            assert 'state in place: newer' in finals[0]['summary'], finals
            assert (state / 'after-B').exists() and not (state / 'write-fence').exists()
        if case in newer_restore_cases and case != 'a':
            assert len(snapshots) == 1, 'FAIL removed snapshots'
        for snapshot in snapshots:
            for name in ['older', 'newer']:
                saved = snapshot / name
                if saved.exists():
                    assert not any((saved / name).exists() for name in kit_files), 'kit file entered snapshot'
                    assert not (saved / 'state.lock').exists(), 'state lock entered snapshot'
                    assert (saved / '.hidden').read_bytes() == b'hidden daemon state'
                    assert (saved / '.hidden').stat().st_mode & 0o777 == 0o640
                    assert (saved / 'native-link').is_symlink()
        assert all((state / name).read_bytes() == data for name, data in kit_files.items()), 'kit files changed'
        if case != 'l':
            assert (state / '.hidden').stat().st_mode & 0o777 == 0o640
            assert (state / 'native-link').is_symlink()
        if puts:
            assert [r['event'] for r in calls if r['event'].startswith('pool-')] == ['pool-A', 'pool-B'], events
        if case in {'i', 'missing-debt'}:
            assert events == ['list', 'pool-A', 'read-A', 'top'], events
            assert all(not list(snapshot.iterdir()) for snapshot in snapshots), 'debt allowed a snapshot'
            assert any(row['test_id'] == 'restore-tail-stop-older' and row['status'] == 'FAIL' for row in rows), rows
            assert (home / 'live').exists()
        if case == 'j':
            assert events[-1] == 'stop' and events.count('stop') == 2 and not puts, events
            assert known == {T: 2, T2: 0} and (state / 'after-B').exists(), known
            assert (state / '.hidden').read_bytes() == before[Path('.hidden')], 'manifest failure moved staged data'
            snapshot = snapshots[0]
            assert (snapshot / 'incoming-older').is_dir() and not list(snapshot.glob('displaced-*'))
            assert any(row['test_id'] == 'restore-tail-restore-older' and row['status'] == 'FAIL'
                       and 'manifest verification failed' in row['summary'] for row in rows), rows
        if case == 'debt-newer':
            assert events[-1] == 'top' and events.count('stop') == 1 and not puts, events
            assert (snapshots[0] / 'older').is_dir() and not (snapshots[0] / 'newer').exists()
            assert any(row['test_id'] == 'restore-tail-stop-newer' and row['status'] == 'FAIL' for row in rows), rows
            assert known[T] == 2 and (home / 'live').exists()
        if case in {'i', 'j', 'k', 'm', 'n', 'o', 'missing-debt', 'debt-newer'}:
            assert 'state in place: current (nothing swapped)' in finals[0]['summary'], finals
        assert (state / 'state.lock').stat().st_ino == int((home / 'lock-inode').read_text()), 'state lock inode changed'
        if case != 'a' and finals[0]['summary'].find('snapshots:') >= 0:
            if case not in {'write-A', 'k', 'null-A', 'm'}:
                assert 'T=' + uuid.UUID(T).hex in finals[0]['summary'], finals
            halt = 'run no other plan with this kit home; send the pack; the snapshots stay on the host'
            assert (halt in finals[0]['summary']) == ('state in place: older' in finals[0]['summary'] or 'state in place: partial (' in finals[0]['summary']), finals
        for row in rows:
            if row['status'] == 'FAIL' and not (case == 'o' and row['test_id'] == 'disk-guard'):
                assert all(token in row['summary'] for token in ['state in place:', 'snapshots:', 'T=']), row
        print(f'PASS: restore-tail {case}')
PY
