#!/usr/bin/env bash
# Verify the shared health probe preserves shell options with either health status.
set -euo pipefail
scripts="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../scripts" && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf -- "$tmp"' EXIT
mkdir -p "$tmp/bin"
cat > "$tmp/bin/rem" <<'STUB'
#!/usr/bin/env bash
[[ "$*" == 'daemon --endpoint unix:probe-only health' ]] || exit 98
exit "$HEALTH_RC"
STUB
chmod +x "$tmp/bin/rem"
FIELD_BIN_DIR="$tmp/bin" bash -s -- "$scripts/lib.sh" <<'BASH'
source "$1"
set +e
for mode in off on; do
  if [[ $mode == on ]]; then set -e; else set +e; fi
  before=$-
  for expected in 1 0; do
    export HEALTH_RC=$expected
    if is_socket_live unix:probe-only; then rc=0; else rc=$?; fi
    [[ $rc == "$expected" ]] || { echo "T10: expected status $expected, got $rc" >&2; exit 1; }
    [[ $- == "$before" ]] || { echo "T10: probe changed shell options ($mode, health $expected): $before -> $-" >&2; exit 1; }
  done
done
printf '%s\n' 'PASS: T10 socket health statuses preserve shell options off and on'
BASH
