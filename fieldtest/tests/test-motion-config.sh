#!/usr/bin/env bash
# Require the kit's configuration on every direct changer call, including wrappers.
set -euo pipefail
root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
python3 - "$root" <<'PY'
"""Scan logical shell lines for debug binaries and aliases used for changer motion."""
from pathlib import Path
import re
import shlex
import sys

root = Path(sys.argv[1])
helper = re.compile(r'''["']?\$\(\s*fieldtest_rem_debug_bin\s*\)["']?''')
assignment = re.compile(r'''\b([A-Za-z_]\w*)=["']?$''')
variable = re.compile(r'''["']?\$(?:\{([A-Za-z_]\w*)\}|([A-Za-z_]\w*))["']?''')
motions = {'move', 'load', 'unload', 'export', 'import'}
operators = ';&|()<>'


def logical_lines(text):
    """Join continued lines while preserving the first physical line number."""
    pending = ''
    start = 1
    for number, line in enumerate(text.splitlines(), 1):
        if not pending:
            start = number
        stripped = line.rstrip()
        if stripped.endswith('\\'):
            pending += stripped[:-1] + ' '
        else:
            yield start, pending + line
            pending = ''
    if pending:
        yield start, pending


def arguments(suffix):
    """Read only the invocation's argv, stopping at shell control or redirection."""
    lexer = shlex.shlex(suffix, posix=True, punctuation_chars=operators)
    lexer.whitespace_split = True
    result = []
    for token in lexer:
        if all(char in operators for char in token):
            break
        result.append(token)
    return result


count = 0
failures = []
for path in sorted((root / 'scripts').glob('*.sh')):
    aliases = set()
    for number, line in logical_lines(path.read_text()):
        if line.lstrip().startswith('#'):
            continue
        invocations = []
        for match in helper.finditer(line):
            assigned = assignment.search(line[:match.start()])
            if assigned:
                aliases.add(assigned[1])
            else:
                invocations.append(match)
        invocations.extend(match for match in variable.finditer(line)
                           if (match[1] or match[2]) in aliases)
        for match in invocations:
            args = arguments(line[match.end():])
            index = 0
            while index < len(args):
                if args[index] in {'--allow', '--allow-derived'}:
                    index += 2
                elif args[index].startswith('--allow=') or args[index].startswith('--allow-derived='):
                    index += 1
                else:
                    break
            if index >= len(args) or args[index] not in motions:
                continue
            count += 1
            if not any(arg == '--config' or arg.startswith('--config=') for arg in args[index + 1:]):
                failures.append(f'fieldtest/{path.relative_to(root)}:{number}: {args[index]} missing --config')
for failure in failures:
    print(failure, file=sys.stderr)
if count < 6:
    print(f'expected at least six direct changer invocations, found {count}', file=sys.stderr)
if failures or count < 6:
    raise SystemExit(1)
print(f'PASS: all {count} direct changer invocations carry --config')
PY
