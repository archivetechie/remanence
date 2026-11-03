#!/usr/bin/env bash
# The kit chooses a Python that can import tomllib (3.11+), because RHEL 9's
# default python3 is 3.9; with none available it refuses with an install hint.
set -euo pipefail
scripts="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../scripts" && pwd)"
root="$(mktemp -d "${TMPDIR:-/tmp}/python-select-test.XXXXXX")"
trap 'rm -rf -- "$root"' EXIT

stub() {  # stub NAME OK|FAIL
  local name=$1 result=$2
  if [[ $result == OK ]]; then
    printf '#!/bin/sh\nexit 0\n' >"$root/bin/$name"
  else
    printf '#!/bin/sh\necho "ModuleNotFoundError: No module named tomllib" >&2\nexit 1\n' >"$root/bin/$name"
  fi
  chmod +x "$root/bin/$name"
}

select_python() {  # FIELD_PYTHON is passed only when the caller sets it for this call
  env -i PATH="$root/bin:/usr/bin:/bin" HOME="$root" ${SELECT_FIELD_PYTHON:+FIELD_PYTHON="$SELECT_FIELD_PYTHON"} \
    bash -c 'source "$1/lib.sh" && fieldtest_python' _ "$scripts"
}

mkdir -p "$root/bin"

# 1. RHEL 9 shape: python3 (3.9) lacks tomllib, python3.11 has it. Newer names are
#    stubbed to fail so the host's own interpreters cannot answer for the stubs.
stub python3.13 FAIL; stub python3.12 FAIL; stub python3.11 OK; stub python3 FAIL
got="$(select_python)"
[[ $got == "$root/bin/python3.11" ]] || { echo "FAIL: expected python3.11, got $got" >&2; exit 1; }

# 2. FIELD_PYTHON wins when it can import tomllib.
stub mypython OK
got="$(SELECT_FIELD_PYTHON=mypython select_python)"
[[ $got == "$root/bin/mypython" ]] || { echo "FAIL: FIELD_PYTHON not honoured, got $got" >&2; exit 1; }

# 3. No candidate has tomllib: refuse with exit 2 and the install hint.
stub python3.11 FAIL
set +e
err="$(select_python 2>&1 >/dev/null)"; rc=$?
set -e
[[ $rc == 2 ]] || { echo "FAIL: expected exit 2 with no usable Python, got $rc" >&2; exit 1; }
[[ $err == *'dnf install python3.11'* ]] || { echo "FAIL: refusal lacks the install hint: $err" >&2; exit 1; }

# 4. A FIELD_PYTHON that cannot import tomllib is refused by name, not skipped.
stub oldpython FAIL; stub python3.11 OK
set +e
err="$(SELECT_FIELD_PYTHON=oldpython select_python 2>&1 >/dev/null)"; rc=$?
set -e
[[ $rc == 2 && $err == *'FIELD_PYTHON=oldpython cannot import tomllib'* ]] || {
  echo "FAIL: a bad FIELD_PYTHON must be refused by name (rc=$rc): $err" >&2; exit 1; }

# 5. The rule this selection exists for: every Python body launched with bare python3
#    stays valid for Python 3.9 and imports no tomllib; a body importing tomllib is
#    launched through the selected interpreter ("$py" or "$(fieldtest_python)").
python3 - "$scripts" <<'PYGUARD'
import ast, re, sys
from pathlib import Path
scripts = Path(sys.argv[1])
paths = sorted(scripts.glob("*.sh")) + [scripts.parent / "run.sh"]
opener = re.compile(r"^(?P<cmd>.*?)<<'(?P<tag>[A-Z_]+)'\s*$")
problems = []
for path in paths:
    lines = path.read_text().split("\n")
    i = 0
    while i < len(lines):
        m = opener.match(lines[i])
        if not m or "python" not in m["cmd"] and '"$py"' not in m["cmd"]:
            i += 1
            continue
        tag, cmd = m["tag"], m["cmd"]
        j = i + 1
        while j < len(lines) and lines[j].strip() != tag:
            j += 1
        body = "\n".join(lines[i + 1:j])
        uses_tomllib = re.search(r"^\s*(import|from)\s+tomllib\b|import [^\n]*\btomllib\b", body, re.M)
        selected = '"$py"' in cmd or "fieldtest_python" in cmd
        if uses_tomllib and not selected:
            problems.append(f"{path.name}:{i+1}: tomllib body not launched through the selected Python")
        if not selected:
            try:
                ast.parse(body, feature_version=(3, 9))
            except SyntaxError as e:
                problems.append(f"{path.name}:{i+1}: bare python3 body is not Python 3.9 syntax: {e}")
        i = j + 1
for p in problems:
    print(p, file=sys.stderr)
sys.exit(1 if problems else 0)
PYGUARD

echo "python selection: ok"
