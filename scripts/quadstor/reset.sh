#!/bin/bash
# Reset the QuadStor virtual library: delete all vcartridges + the VTL, then re-create them.
# Leaves device definitions and the backing disk intact (those are slow to set up).
#
# Use when you want a clean library to play with but don't want to wait for the
# disk-initialization step in setup.sh.
#
# Usage:  sudo scripts-relative: scripts/quadstor/reset.sh

set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=common.sh
source "$DIR/common.sh"

need_sudo

if ! vtl_exists "$VTL_NAME"; then
    log "VTL '$VTL_NAME' does not exist — nothing to reset; running setup.sh"
    exec "$DIR/setup.sh"
fi

# 0. Ownership check — refuse to wipe a VTL that isn't ours.
# A VTL named $VTL_NAME (default 'mainlib') could belong to someone else.
# Verify the VTL is using our changerdef + drivedef before deleting.
# vtconfig reports the HP_MSL_Series changer def as VTL Type "HP MSL G3 Series",
# so match that reported string (or the literal def name) — mirrors the drive
# check below. The earlier "MSL G3 Series.*${CHANGER_DEF##*_}" pattern wrongly
# required "Series" to appear twice and never matched.
vtl_info=$("$VTCONFIG" -l -v "$VTL_NAME" 2>/dev/null || true)
if ! echo "$vtl_info" | grep -qE "MSL G3 Series|$CHANGER_DEF"; then
    die "VTL '$VTL_NAME' exists but doesn't appear to use our changer def ('$CHANGER_DEF'). \
Refusing to delete. If this is wrong, override VTL_NAME via env var."
fi
if ! echo "$vtl_info" | grep -qE "Ultrium 9-SCSI|$DRIVE_DEF"; then
    die "VTL '$VTL_NAME' exists but doesn't appear to use our drive def ('$DRIVE_DEF'). \
Refusing to delete."
fi

# 1. Delete every vcartridge in this VTL.
# vcconfig columns are: Pool Label Element Address ... — the cartridge
# barcode `-p` wants is the LABEL ($2), not the pool ($1).
log "deleting all virtual cartridges from '$VTL_NAME'"
"$VCCONFIG" -l -v "$VTL_NAME" 2>/dev/null \
    | awk 'NR>1 && $2!="" {print $2}' \
    | while read -r label; do
        log "  rm vcartridge $label"
        "$VCCONFIG" -x -v "$VTL_NAME" -p "$label" || true
    done

# Cartridge deletes are async ("Delete ... was started"); the VTL delete
# below refuses while any vcartridge is still active. Wait for the count
# to drain (bounded) before deleting the VTL.
for _ in $(seq 1 60); do
    remaining=$("$VCCONFIG" -l -v "$VTL_NAME" 2>/dev/null \
        | awk 'NR>1 && $2!="" {c++} END {print c+0}')
    [ "$remaining" -eq 0 ] && break
    log "  waiting for $remaining vcartridge(s) to finish deleting…"
    sleep 1
done

# 2. Delete the VTL itself
log "deleting VTL '$VTL_NAME'"
"$VTCONFIG" -x -v "$VTL_NAME"

# 3. Re-run setup to recreate VTL + cartridges
log "re-running setup.sh to recreate VTL"
exec "$DIR/setup.sh"
