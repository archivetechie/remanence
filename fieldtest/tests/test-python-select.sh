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

select_python() {
  env -i PATH="$root/bin:/usr/bin:/bin" HOME="$root" ${FIELD_PYTHON:+FIELD_PYTHON="$FIELD_PYTHON"} \
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
got="$(FIELD_PYTHON=mypython select_python)"
[[ $got == "$root/bin/mypython" ]] || { echo "FAIL: FIELD_PYTHON not honoured, got $got" >&2; exit 1; }

# 3. No candidate has tomllib: refuse with exit 2 and the install hint.
stub python3.11 FAIL
set +e
err="$(select_python 2>&1 >/dev/null)"; rc=$?
set -e
[[ $rc == 2 ]] || { echo "FAIL: expected exit 2 with no usable Python, got $rc" >&2; exit 1; }
[[ $err == *'dnf install python3.11'* ]] || { echo "FAIL: refusal lacks the install hint: $err" >&2; exit 1; }

echo "python selection: ok"
