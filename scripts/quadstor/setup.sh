#!/bin/bash
# Set up a QuadStor virtual tape library matching the Remanence dev target:
# - HP MSL G3 Series changer
# - 4 × HPE LTO-9 drives
# - 40 storage slots
# - 4 import/export ports
# - 10 virtual LTO-9 cartridges
# Backing storage: sparse file → loopback → LVM PV/VG/LV → QuadStor.
# (QuadStor accepts LVM volumes — it rejects raw loop/dm-linear/scsi_debug.)
#
# Idempotent. Safe to re-run after a partial setup.
#
# Usage:  sudo scripts-relative: scripts/quadstor/setup.sh
# Reset:  sudo scripts-relative: scripts/quadstor/reset.sh     (rebuilds just the VTL)
# Nuke:   sudo scripts-relative: scripts/quadstor/teardown.sh  (removes everything)

set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=common.sh
source "$DIR/common.sh"

need_sudo

log "QuadStor VTL setup starting (VTL='$VTL_NAME', $DRIVE_COUNT × $DRIVE_DEF, ${SLOT_COUNT} slots)"

# --- 1. Ensure QuadStor service is up --------------------------------
if ! systemctl is-active --quiet quadstorvtl; then
    log "quadstorvtl service not running — starting it"
    systemctl start quadstorvtl
    sleep 3
fi
systemctl is-active --quiet quadstorvtl || die "quadstorvtl failed to start"

# --- 2. Import device definitions (HP_MSL_Series, HP_LTO9) -----------
if def_exists "$CHANGER_DEF" changer; then
    log "changer def '$CHANGER_DEF' already present"
else
    log "adding changer def '$CHANGER_DEF' (HP MSL G3 Series)"
    "$DEVICEDEF" -a --changer \
        --name "$CHANGER_DEF" \
        --vendor HP --product "MSL G3 Series" --revision "D.00" \
        --seriallen 10 --inquirylen 0 \
        --drive-start 256 --slot-start 1024 --ieport-start 768 \
        --avoltag 0
fi

if def_exists "$DRIVE_DEF" drive; then
    log "drive def '$DRIVE_DEF' already present"
else
    log "adding drive def '$DRIVE_DEF' (HPE Ultrium 9-SCSI, LTO-9)"
    "$DEVICEDEF" -a --drive \
        --name "$DRIVE_DEF" \
        --vendor HPE --product "Ultrium 9-SCSI" --revision "HH90" \
        --seriallen 10 --inquirylen 0 \
        --mediatype "$CART_TYPE"
fi

# --- 3. Backing storage: LVM LV on a sparse loopback file ------------
mkdir -p "$BACKING_DIR"
if [[ ! -f "$BACKING_FILE" ]]; then
    log "creating ${BACKING_SIZE_GB}G sparse backing file at $BACKING_FILE"
    truncate -s "${BACKING_SIZE_GB}G" "$BACKING_FILE"
else
    log "backing file already exists at $BACKING_FILE"
fi

LOOP_DEV="$(backing_loop_dev)"
if [[ -z "$LOOP_DEV" ]]; then
    LOOP_DEV="$(losetup -f --show "$BACKING_FILE")"
    log "attached loop device: $LOOP_DEV"
else
    log "backing file already attached to $LOOP_DEV"
fi

if ! pvs --noheadings "$LOOP_DEV" >/dev/null 2>&1; then
    log "creating LVM PV on $LOOP_DEV"
    pvcreate "$LOOP_DEV" >/dev/null
fi
if vgs --noheadings "$LVM_VG" >/dev/null 2>&1; then
    # VG already exists — make sure it's OURS (backed by the loop on our backing file)
    # rather than someone else's VG that happens to share the name.
    actual_pv=$(vgs --noheadings -o pv_name "$LVM_VG" 2>/dev/null | awk '{print $1}')
    if [[ "$actual_pv" != "$LOOP_DEV" ]]; then
        die "VG '$LVM_VG' exists on '$actual_pv', not on our loop '$LOOP_DEV' ($BACKING_FILE). \
Refusing to reuse it. Pick a different LVM_VG via env var, or remove the existing VG manually."
    fi
    log "VG '$LVM_VG' already exists and is backed by $LOOP_DEV"
else
    log "creating volume group '$LVM_VG' on $LOOP_DEV"
    vgcreate "$LVM_VG" "$LOOP_DEV" >/dev/null
fi
if ! lvs --noheadings "$LVM_VG/$LVM_LV" >/dev/null 2>&1; then
    log "creating logical volume '$LVM_LV' (consumes all of $LVM_VG)"
    lvcreate -l 100%FREE -n "$LVM_LV" "$LVM_VG" >/dev/null
fi
[[ -b "$LV_PATH" ]] || die "LV path $LV_PATH not present after lvcreate"

# Persist the loop + LVM across reboots
SYSTEMD_UNIT=/etc/systemd/system/quadstor-loop.service
if [[ ! -f "$SYSTEMD_UNIT" ]]; then
    log "installing systemd unit to reattach loop + activate LVM on boot"
    cat >"$SYSTEMD_UNIT" <<EOF
[Unit]
Description=Attach QuadStor backing loopback + LVM
DefaultDependencies=no
Before=quadstorvtl.service
After=local-fs.target lvm2-monitor.service

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/bin/sh -c 'losetup -j $BACKING_FILE | grep -q . || losetup -f $BACKING_FILE; vgchange -ay $LVM_VG'
ExecStop=/bin/sh -c 'vgchange -an $LVM_VG; d=\$(losetup -j $BACKING_FILE | cut -d: -f1); [ -n "\$d" ] && losetup -d "\$d" || true'

[Install]
WantedBy=multi-user.target
EOF
    systemctl daemon-reload
    systemctl enable quadstor-loop.service >/dev/null
fi

# --- 4. Add the LV to QuadStor's pool --------------------------------
if disk_configured "$LV_PATH"; then
    log "LV $LV_PATH already in pool '$POOL_NAME'"
else
    log "adding $LV_PATH to pool '$POOL_NAME' (kicks off disk init)"
    "$BDCONFIG" --rescan >/dev/null 2>&1 || true
    "$BDCONFIG" -a -d "$LV_PATH" -g "$POOL_NAME"
fi

# --- 5. Wait for QuadStor's disk init to finish ----------------------
# Init speed ~150 MB/s. A 100G LV takes ~11 min the first time.
# Subsequent runs find Status=Active and skip the wait immediately.
log "waiting for disk init (status: 'Active' when done)…"
deadline=$(( SECONDS + 1800 ))
while (( SECONDS < deadline )); do
    status_line=$("$BDCONFIG" -l -c 2>/dev/null | awk -v p="$LV_PATH" '$0 ~ p {print; exit}')
    status=$(echo "$status_line" | awk '{print $NF}')
    if [[ "$status" != "Initializing" && -n "$status_line" ]]; then
        log "disk init done — status: $status"
        break
    fi
    used=$(echo "$status_line" | awk '{print $(NF-1)}')
    log "  …still initializing (used: ${used:-?}/${BACKING_SIZE_GB} GB)"
    sleep 10
done
(( SECONDS >= deadline )) && die "disk init timed out after 30 min"

# --- 6. Create the VTL -----------------------------------------------
# Note: use long-form --drivedef=. The short -T flag is documented as drivedef
# but actually maps to drive-vendor-type (a numeric ID), and the daemon then
# rejects the command with "Invalid message msg_data".
if vtl_exists "$VTL_NAME"; then
    log "VTL '$VTL_NAME' already exists"
else
    log "creating VTL '$VTL_NAME': $DRIVE_COUNT × $DRIVE_DEF, $SLOT_COUNT slots, $IEPORT_COUNT IE ports"
    "$VTCONFIG" --add \
        --vtl="$VTL_NAME" \
        --changerdef="$CHANGER_DEF" \
        --slots="$SLOT_COUNT" \
        --ieports="$IEPORT_COUNT" \
        --drivedef="$DRIVE_DEF" \
        --drive-count="$DRIVE_COUNT"
fi

# --- 7. Add virtual cartridges ---------------------------------------
# vcconfig requires a 6-char prefix when count > 1; it auto-appends a 2-char
# media-type suffix (e.g. "L9" for LTO-9) to produce labels like RMN001L9.
EXISTING_CARTS=$("$VCCONFIG" -l -v "$VTL_NAME" 2>/dev/null | awk 'NR>1 && $1!="" {c++} END {print c+0}')
if (( EXISTING_CARTS >= CART_COUNT )); then
    log "$EXISTING_CARTS vcartridges already present (target: $CART_COUNT)"
else
    NEEDED=$(( CART_COUNT - EXISTING_CARTS ))
    log "adding $NEEDED LTO-9 vcartridges (prefix=$CART_PREFIX) to '$VTL_NAME'"
    "$VCCONFIG" -a -v "$VTL_NAME" -g "$POOL_NAME" \
        -p "$CART_PREFIX" -t "$CART_TYPE" -c "$NEEDED"
fi

# --- 8. Summary ------------------------------------------------------
log "setup complete — current state:"
"$DIR/status.sh"
