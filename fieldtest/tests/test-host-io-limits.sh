#!/usr/bin/env bash
# Exercise preflight host-limit capture with sysfs and ioctl fixtures only.
set -euo pipefail
root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
python3 - "$root" <<'PY'
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
from unittest.mock import mock_open, patch

sys.dont_write_bytecode = True
root = Path(sys.argv[1])
spec = importlib.util.spec_from_file_location('host_limits', root / 'scripts/host-io-limits.py')
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
with tempfile.TemporaryDirectory() as temporary:
    sysfs = Path(temporary)
    for path, data in {'module/sg/parameters/allow_dio': '0', 'module/sg/parameters/def_reserved_size': '32768', 'module/sg/parameters/scatter_elem_sz': '32768', 'class/scsi_host/host2/sg_tablesize': '257'}.items():
        file = sysfs / path
        file.parent.mkdir(parents=True, exist_ok=True)
        file.write_text(data)
    sg = sysfs / 'class/scsi_generic/sg7'
    sg.mkdir(parents=True)
    device = sysfs / 'devices/pci/host2/target2:0:1/2:0:1:0'
    device.mkdir(parents=True)
    (sg / 'device').symlink_to(device)
    discovery = {'observed_endpoints': [
        {'library_serial': 'LL01', 'drive_serial': 'TEST', 'path': '/dev/sg7'},
        {'library_serial': 'OTHER', 'drive_serial': 'DO-NOT-OPEN', 'path': '/dev/sg8'}]}
    calls = []
    def reserve(node, wanted):
        calls.append((node, wanted))
        assert node == '/dev/sg7' and wanted == 0xFFFFFF
        return 8 * 1024 * 1024
    report = module.capture(discovery, 'LL01', sysfs, reserve)
    assert calls == [('/dev/sg7', 0xFFFFFF)]
    drive, = report['drives']
    assert drive['scsi_host'] == 'host2' and drive['sg_tablesize'] == '257'
    assert drive['max_sectors'].startswith('unavailable')
    assert report['sg_module_parameters']['scatter_elem_sz'] == '32768'
    assert '8388608' in report['warnings'][0] and 'larger physical records are refused' in report['warnings'][0]
    def denied(*args):
        raise PermissionError('fixture denial')
    assert 'query failed' in module.capture(discovery, 'LL01', sysfs, denied)['warnings'][0]
    assert module.capture({}, 'LL01', sysfs, reserve)['warnings']
    (sysfs / 'class/scsi_host/host2/max_sectors').write_text('16384')
    assert module.capture(discovery, 'LL01', sysfs, reserve)['drives'][0]['max_sectors'] == '16384'
    # Pin the real helper's ioctl sequence. No actual fd or device is opened.
    commands = []
    def ioctl(fd, operation, size, mutate):
        commands.append(operation)
        if operation == 0x2275:
            assert size[0] == 0xFFFFFF
        elif operation == 0x2272:
            size[0] = 8388608
        else:
            raise AssertionError('unexpected ioctl (including SG_IO)')
    with patch('builtins.open', mock_open()), patch.object(module.fcntl, 'ioctl', ioctl):
        assert module.reserved_grant('/fixture/sg', 0xFFFFFF) == 8388608
    assert commands == [0x2275, 0x2272]
text = (root / 'scripts/00-preflight.sh').read_text()
assert text.index('host-io-limits.py') < text.index('"$bin" library "$selected_serial" --slots')
print('PASS: selected-drive host limits, grants, missing sysfs, permission warnings; no tape commands')
PY
