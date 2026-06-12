#!/bin/bash
# Full teardown of the QuadStor virtual library setup, in reverse order:
# - delete vcartridges
# - delete the VTL
# - remove the backing disk from the pool
# - detach + delete the loopback file
# - disable the systemd loop unit
# - (optionally) remove the device defs
#
# Leaves QuadStor itself (the .deb) installed. To uninstall, see INSTALL.md.
#
# Usage:  sudo /home/user/remanence/scripts/quadstor/teardown.sh
#         sudo /home/user/remanence/scripts/quadstor/teardown.sh --keep-backing
#         sudo /home/user/remanence/scripts/quadstor/teardown.sh --remove-defs

set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=common.sh
source "$DIR/common.sh"

need_sudo

KEEP_BACKING=0
REMOVE_DEFS=0
for a in "$@"; do
    case "$a" in
        --keep-backing) KEEP_BACKING=1 ;;
        --remove-defs)  REMOVE_DEFS=1 ;;
        *) die "unknown flag $a" ;;
    esac
done

# 1. vcartridges + VTL
if vtl_exists "$VTL_NAME"; then
    log "removing vcartridges from '$VTL_NAME'"
    "$VCCONFIG" -l -v "$VTL_NAME" 2>/dev/null \
        | awk 'NR>1 && $1!="" {print $1}' \
        | while read -r label; do
            "$VCCONFIG" -x -v "$VTL_NAME" -p "$label" || true
        done
    log "deleting VTL '$VTL_NAME'"
    "$VTCONFIG" -x -v "$VTL_NAME" || true
else
    log "no VTL '$VTL_NAME' present"
fi

# 2. Remove the LV from QuadStor's pool (setup added LV_PATH, not LOOP_DEV).
if disk_configured "$LV_PATH"; then
    log "removing $LV_PATH from pool '$POOL_NAME'"
    "$BDCONFIG" -x -d "$LV_PATH" || true
fi

# 3. Deactivate + remove the LVM stack so the loop is no longer in use.
if lvs --noheadings "$LVM_VG/$LVM_LV" >/dev/null 2>&1; then
    log "removing LV $LVM_VG/$LVM_LV"
    lvchange -an "$LVM_VG/$LVM_LV" >/dev/null 2>&1 || true
    lvremove -f "$LVM_VG/$LVM_LV" >/dev/null 2>&1 || true
fi
if vgs --noheadings "$LVM_VG" >/dev/null 2>&1; then
    log "removing VG $LVM_VG"
    vgchange -an "$LVM_VG" >/dev/null 2>&1 || true
    vgremove -f "$LVM_VG" >/dev/null 2>&1 || true
fi

# 4. Detach the loop device.
LOOP_DEV="$(backing_loop_dev || true)"
if [[ -n "$LOOP_DEV" ]]; then
    # Make sure no PV signature is left to confuse a future setup run.
    pvremove -f "$LOOP_DEV" >/dev/null 2>&1 || true
    log "detaching loop device $LOOP_DEV"
    losetup -d "$LOOP_DEV" || true
fi

if [[ "$KEEP_BACKING" -eq 0 && -f "$BACKING_FILE" ]]; then
    log "deleting backing file $BACKING_FILE"
    rm -f "$BACKING_FILE"
fi

# 5. systemd loop unit
SYSTEMD_UNIT=/etc/systemd/system/quadstor-loop.service
if [[ -f "$SYSTEMD_UNIT" ]]; then
    log "disabling and removing $SYSTEMD_UNIT"
    systemctl disable quadstor-loop.service >/dev/null 2>&1 || true
    rm -f "$SYSTEMD_UNIT"
    systemctl daemon-reload
fi

# 6. Device defs (optional)
if [[ "$REMOVE_DEFS" -eq 1 ]]; then
    if def_exists "$CHANGER_DEF" changer; then
        log "removing changer def '$CHANGER_DEF'"
        "$DEVICEDEF" --delete --changer --name "$CHANGER_DEF" || true
    fi
    if def_exists "$DRIVE_DEF" drive; then
        log "removing drive def '$DRIVE_DEF'"
        "$DEVICEDEF" --delete --drive --name "$DRIVE_DEF" || true
    fi
fi

log "teardown complete"
