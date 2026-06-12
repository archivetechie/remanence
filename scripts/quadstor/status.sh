#!/bin/bash
# Show the current state of the QuadStor VTL on akash.
# Safe to run any time; makes no changes.
#
# Usage:  sudo /home/user/remanence/scripts/quadstor/status.sh

set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=common.sh
source "$DIR/common.sh"

need_sudo

echo "==== quadstorvtl.service ===="
systemctl is-active quadstorvtl || true
systemctl status quadstorvtl --no-pager 2>/dev/null | sed -n '1,4p' || true

echo
echo "==== loaded kernel modules ===="
lsmod | grep -E '^vtlitf|^vtldev|^iscsit' || echo "(none loaded)"

echo
echo "==== loop devices attached to $BACKING_FILE ===="
LOOP_DEV="$(backing_loop_dev || true)"
if [[ -n "$LOOP_DEV" ]]; then
    losetup -l "$LOOP_DEV"
else
    echo "(no loop device attached)"
fi

echo
echo "==== device definitions ===="
echo "-- changers --"
"$DEVICEDEF" -l --changer 2>/dev/null || echo "(none)"
echo
echo "-- drives --"
"$DEVICEDEF" -l --drive 2>/dev/null || echo "(none)"

echo
echo "==== storage pools ===="
"$SPCONFIG" -l 2>/dev/null || echo "(none)"

echo
echo "==== configured backing disks ===="
"$BDCONFIG" -l -c 2>/dev/null || echo "(none)"

echo
echo "==== virtual tape libraries ===="
"$VTCONFIG" -l 2>/dev/null || echo "(none)"

echo
if vtl_exists "$VTL_NAME"; then
    echo "==== VTL '$VTL_NAME' detail ===="
    "$VTCONFIG" -l -v "$VTL_NAME" 2>/dev/null || true
    echo
    echo "==== virtual cartridges in '$VTL_NAME' ===="
    "$VCCONFIG" -l -v "$VTL_NAME" 2>/dev/null || echo "(none)"
fi

echo
echo "==== local SCSI generic nodes (the dev target) ===="
if command -v lsscsi >/dev/null; then
    lsscsi -g
else
    ls -l /dev/sg* 2>/dev/null || echo "(no /dev/sg*)"
fi
