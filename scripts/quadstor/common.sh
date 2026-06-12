#!/bin/bash
# Shared config for Remanence's QuadStor VTL helper scripts.
# Source this from setup.sh / reset.sh / teardown.sh / status.sh.

set -euo pipefail

# --- Names and sizes -------------------------------------------------
VTL_NAME="${VTL_NAME:-mainlib}"           # name of the virtual library
CHANGER_DEF="${CHANGER_DEF:-HP_MSL_Series}"
DRIVE_DEF="${DRIVE_DEF:-HP_LTO9}"
DRIVE_COUNT="${DRIVE_COUNT:-4}"            # 4 LTO-9 drives per the plan
SLOT_COUNT="${SLOT_COUNT:-40}"             # 40-slot changer
IEPORT_COUNT="${IEPORT_COUNT:-4}"          # 4 import/export ports

POOL_NAME="${POOL_NAME:-Default}"          # Default pool always exists

CART_TYPE="${CART_TYPE:-27}"               # 27 = LTO 9 18000GB (from vcconfig -h)
# vcconfig requires a 6-char prefix when count > 1; it auto-appends an L9 (or
# media-specific) suffix to produce the final 8-char label, e.g. RMN001L9.
CART_PREFIX="${CART_PREFIX:-RMN001}"
CART_COUNT="${CART_COUNT:-10}"             # number of vcartridges to add

# --- Backing storage -------------------------------------------------
# We back QuadStor with an LVM logical volume sitting on top of a loopback
# file. QuadStor's daemon rejects raw loop / dm-linear / scsi_debug devices
# but explicitly accepts LVM volumes (per its own docs), so the LV is the
# minimum-friction path that needs no external disk.
BACKING_DIR="${BACKING_DIR:-/var/lib/quadstor-backing}"
BACKING_FILE="${BACKING_FILE:-$BACKING_DIR/main.img}"
BACKING_SIZE_GB="${BACKING_SIZE_GB:-100}"  # sparse — actual disk use scales with vtape writes
LVM_VG="${LVM_VG:-qsvg}"
LVM_LV="${LVM_LV:-qslv}"
LV_PATH="/dev/mapper/${LVM_VG}-${LVM_LV}"   # canonical path QuadStor wants

# --- QuadStor binaries -----------------------------------------------
QS_BIN=/quadstorvtl/bin
DEVICEDEF="$QS_BIN/devicedef"
SPCONFIG="$QS_BIN/spconfig"
BDCONFIG="$QS_BIN/bdconfig"
VTCONFIG="$QS_BIN/vtconfig"
VCCONFIG="$QS_BIN/vcconfig"

# --- Helpers ---------------------------------------------------------
log() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*"; }
die() { log "ERROR: $*" >&2; exit 1; }

need_sudo() {
    if [[ $EUID -ne 0 ]]; then
        die "run with sudo (some QuadStor binaries need root)"
    fi
}

# Return the loop device path attached to $BACKING_FILE, or empty string.
backing_loop_dev() {
    losetup -j "$BACKING_FILE" 2>/dev/null | awk -F: 'NR==1{print $1}'
}

# Does a definition with $1 exist in `devicedef -l --$2` (changer/drive)?
def_exists() {
    "$DEVICEDEF" -l --"$2" 2>/dev/null | awk 'NR>1{print $1}' | grep -qx "$1"
}

# Does the VTL named $1 already exist?
vtl_exists() {
    "$VTCONFIG" -l 2>/dev/null | awk 'NR>1{print $1}' | grep -qx "$1"
}

# Is disk $1 already configured in QuadStor?
disk_configured() {
    "$BDCONFIG" -l -c 2>/dev/null | grep -qE "(^|[[:space:]])$1([[:space:]]|$)"
}
