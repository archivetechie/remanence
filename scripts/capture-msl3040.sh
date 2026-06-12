#!/bin/bash
# Read-only fixture capture for Remanence.
#
# Run this on the host that the production HP MSL3040 is attached to. It
# will identify every /dev/sg* node, classify each as changer or tape, and
# capture every SCSI response Remanence will eventually want to parse.
#
# **No state-changing commands are issued.** sg_inq, LOG SENSE, READ
# ELEMENT STATUS, and mtx status are all read-only. No MODE SELECT, no
# tape loads/unloads, no rewinds.
#
# Output: ./remanence-fixtures-<hostname>-<UTC timestamp>/  (also tar.gz'd)
# Once it finishes:  scp <host>:~/remanence-fixtures-*.tar.gz ./
#
# Usage:  sudo bash capture-msl3040.sh
#         sudo bash capture-msl3040.sh --include /dev/sg5 /dev/sg7
#         sudo bash capture-msl3040.sh --exclude /dev/sg1
#         sudo bash capture-msl3040.sh --with-init           # also run INITIALIZE
#                                                              ELEMENT STATUS — robot moves!
# By default the script always issues a set of DVCID probe CDB variants
# against every changer (no robot motion). --with-init additionally runs
# INITIALIZE ELEMENT STATUS (07h) before the probes, which makes the
# library physically re-scan all slots; only opt in if you can tolerate
# a minute or so of robot activity.

set -uo pipefail   # NOT -e — we want to keep going if one capture fails

# -------- parse args -------------------------------------------------
INCLUDE=()
EXCLUDE=()
WITH_INIT=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --include)   shift; while [[ $# -gt 0 && "$1" != --* ]]; do INCLUDE+=("$1"); shift; done ;;
        --exclude)   shift; while [[ $# -gt 0 && "$1" != --* ]]; do EXCLUDE+=("$1"); shift; done ;;
        --with-init) WITH_INIT=1; shift ;;
        -h|--help)
            head -32 "$0" | sed 's/^#//'; exit 0 ;;
        *) echo "unknown flag $1" >&2; exit 2 ;;
    esac
done

# -------- prereqs ----------------------------------------------------
if [[ $EUID -ne 0 ]]; then
    echo "ERROR: must be run as root (try: sudo bash $0)" >&2
    exit 1
fi
for cmd in sg_inq sg_logs sg_modes sg_raw lsscsi; do
    command -v "$cmd" >/dev/null || { echo "ERROR: missing $cmd (apt install sg3-utils lsscsi)" >&2; exit 1; }
done
command -v mtx >/dev/null || echo "warn: mtx not installed; skipping mtx status"

# -------- output dir -------------------------------------------------
HOST=$(hostname -s)
STAMP=$(date -u +%Y%m%dT%H%M%SZ)
OUT="$PWD/remanence-fixtures-${HOST}-${STAMP}"
mkdir -p "$OUT"
LOG="$OUT/capture.log"
exec > >(tee -a "$LOG") 2>&1

say() { printf '\n=== %s ===\n' "$*"; }
run() { printf '  $ %s\n' "$*"; "$@"; rc=$?; printf '    (rc=%d)\n' "$rc"; return $rc; }

say "Capture starting at $(date -u) on $HOST -> $OUT"

# -------- host metadata ----------------------------------------------
say "host metadata"
{
    echo "hostname:    $(hostname -f 2>/dev/null || hostname)"
    echo "kernel:      $(uname -a)"
    echo "lsb:         $(lsb_release -ds 2>/dev/null || cat /etc/os-release 2>/dev/null | head -3)"
    echo "date_utc:    $(date -u)"
    echo "sg3-utils:   $(sg_inq --version 2>&1 | head -1)"
} | tee "$OUT/host.txt"

run lsscsi -g     > "$OUT/lsscsi-g.txt"     2>&1 || true
run lsscsi -gvv   > "$OUT/lsscsi-gvv.txt"   2>&1 || true
dmesg -T 2>/dev/null | grep -iE 'scsi|hp[: ]|msl|lto|ult' | tail -200 > "$OUT/dmesg-scsi.txt" || true

# -------- pick devices to capture ------------------------------------
in_list() { local needle=$1; shift; for x in "$@"; do [[ "$x" == "$needle" ]] && return 0; done; return 1; }

if (( ${#INCLUDE[@]} )); then
    SG_DEVS=("${INCLUDE[@]}")
else
    mapfile -t SG_DEVS < <(ls /dev/sg* 2>/dev/null | sort -V)
fi
FILTERED=()
for d in "${SG_DEVS[@]}"; do
    [[ -e "$d" ]] || continue
    in_list "$d" "${EXCLUDE[@]:-}" && continue
    FILTERED+=("$d")
done

say "devices to capture: ${FILTERED[*]:-<none>}"
[[ ${#FILTERED[@]} -eq 0 ]] && { echo "no /dev/sg* found"; exit 1; }

# -------- classify each device --------------------------------------
declare -A KIND   # device -> changer|tape|disk|other
declare -A LABEL  # device -> stable filename slug
declare -i dn=0 cn=0
for d in "${FILTERED[@]}"; do
    pdt=$(sg_inq "$d" 2>/dev/null | awk -F': ' '/Peripheral device type:/{print $2; exit}')
    case "$pdt" in
        *"medium changer"*) KIND[$d]=changer; LABEL[$d]="changer$((++cn))" ;;
        *tape*)              KIND[$d]=tape;    LABEL[$d]="drive$((++dn))"   ;;
        *disk*)              KIND[$d]=disk;    LABEL[$d]="disk_skip"        ;;  # not interesting
        *)                   KIND[$d]=other;   LABEL[$d]="other_skip"       ;;
    esac
    printf '  %-12s pdt=%-20s -> %s (%s)\n' "$d" "${pdt:-?}" "${KIND[$d]}" "${LABEL[$d]}"
done

# -------- per-device captures ---------------------------------------
mkdir -p \
    "$OUT/inquiry/standard" \
    "$OUT/inquiry/vpd-00" \
    "$OUT/inquiry/vpd-80" \
    "$OUT/inquiry/vpd-83" \
    "$OUT/inquiry/vpd-85" \
    "$OUT/inquiry/vpd-c0" \
    "$OUT/inquiry/vpd-cc" \
    "$OUT/inquiry/vpd-d0" \
    "$OUT/read-element-status" \
    "$OUT/read-element-status-probes" \
    "$OUT/mode-sense" \
    "$OUT/log-sense"

for d in "${FILTERED[@]}"; do
    kind=${KIND[$d]}
    lbl=${LABEL[$d]}
    [[ "$kind" == "disk" || "$kind" == "other" ]] && { say "skip $d ($kind)"; continue; }

    say "device $d ($kind, $lbl)"

    # ---- INQUIRY: standard + VPD 0x00, 0x80, 0x83 ------------------
    sg_inq -v          "$d" > "$OUT/inquiry/standard/${lbl}.txt" 2>&1 || true
    sg_inq --raw       "$d" > "$OUT/inquiry/standard/${lbl}.bin" 2>/dev/null || true

    # VPD pages we want from every device. Per HPE 20-STG-TAPESCSIREF-ED5:
    #   00=supported-pages list, 80=unit serial, 83=device identification,
    #   85=management network address, c0=firmware build info,
    #   cc=HPE Specific Inquiry, d0=HPE Specific.
    for page in 00 80 83 85 c0 cc d0; do
        sg_inq --page=0x$page -v   "$d" > "$OUT/inquiry/vpd-${page}/${lbl}.txt" 2>&1 || true
        sg_inq --page=0x$page --raw "$d" > "$OUT/inquiry/vpd-${page}/${lbl}.bin" 2>/dev/null || true
    done

    # ---- changer-specific: READ ELEMENT STATUS with DVCID + VOLTAG -
    if [[ "$kind" == "changer" ]]; then
        # Two CDB variants. The "safe" form (256 elements, 64 KB alloc) works
        # on every library we've tested including QuadStor. The "big" form
        # mirrors the original plan.txt CDB and gives the real MSL3040 room
        # to return larger payloads if its firmware uses them; QuadStor
        # refuses it with a CHECK CONDITION which we tolerate.
        #
        # CDB layout (SMC-3 §6.13):
        #   b8                opcode READ ELEMENT STATUS
        #   10                byte1: VOLTAG=1, element-type=0 (ALL)
        #   00 00             starting element address = 0
        #   XX XX             number of elements
        #   02                byte6: DVCID=1 (include drive serials)
        #   YY YY YY          allocation length (24-bit)
        #   00 00             reserved, control

        # NO-CURDATA: 256 elements, 64 KB alloc, DVCID=1, CurData=0.
        # Renamed from "safe" because we now know HPE silently drops DVCID
        # descriptors without CurData=1 — see dvcid-with-curdata probes
        # in read-element-status-probes/ for the form that actually works.
        sg_raw -v -r 65536 "$d" b8 10 00 00 01 00 02 01 00 00 00 00 \
            > "$OUT/read-element-status/${lbl}-dvcid-no-curdata.txt" 2>&1 || true
        sg_raw -o "$OUT/read-element-status/${lbl}-dvcid-no-curdata.bin" -r 65536 \
            "$d" b8 10 00 00 01 00 02 01 00 00 00 00 \
            > /dev/null 2>&1 || true

        # BIG: 0xFFFF elements, ~1 MB alloc (was 0xffff00 = 16 MB in CDB
        # while sg_raw -r was 1 MB — mismatch; reconciled here to 1 MB).
        sg_raw -v -r 1048576 "$d" b8 10 00 00 ff ff 02 10 00 00 00 00 \
            > "$OUT/read-element-status/${lbl}-dvcid-big.txt" 2>&1 || true
        sg_raw -o "$OUT/read-element-status/${lbl}-dvcid-big.bin" -r 1048576 \
            "$d" b8 10 00 00 ff ff 02 10 00 00 00 00 \
            > /dev/null 2>&1 || true

        # NO-DVCID variant — for firmware-quirk diagnosis
        sg_raw -o "$OUT/read-element-status/${lbl}-novcid.bin" -r 65536 \
            "$d" b8 10 00 00 01 00 00 01 00 00 00 00 \
            > /dev/null 2>&1 || true

        # mtx status (decoded, cross-check)
        if command -v mtx >/dev/null; then
            mtx -f "$d" status > "$OUT/read-element-status/${lbl}-mtx-status.txt" 2>&1 || true
        fi

        # -- DVCID PROBES ---------------------------------------------
        # HPE's SCSI ref says DVCID returns a 34-byte (Vendor+Product+Serial)
        # block per drive descriptor, but firmware 3350 silently omits it
        # for our normal CDB. These probes try every reasonable combination
        # to figure out what actually triggers the DVCID block.
        #
        # CDB byte 6 bits (per HPE Read Element Status (B8h) §):
        #   bit 4 = Mixed,  bit 1 = DVCID,  bit 0 = CurData
        # CDB byte 1 element-type-code low nibble:
        #   0=all, 1=transport, 2=storage, 3=ie, 4=data transfer (drives)
        say "  DVCID probes on $d"
        for probe in \
            "dt_only:b8 14 00 00 00 ff 02 01 00 00 00 00" \
            "dt_curdata:b8 14 00 00 00 ff 03 01 00 00 00 00" \
            "all_curdata:b8 10 00 00 01 00 03 01 00 00 00 00" \
            "all_mixed_dvcid:b8 10 00 00 01 00 12 01 00 00 00 00" \
            "all_mixed_dvcid_curdata:b8 10 00 00 01 00 13 01 00 00 00 00" \
        ; do
            name="${probe%%:*}"
            cdb="${probe#*:}"
            sg_raw -v -r 65536 "$d" $cdb \
                > "$OUT/read-element-status-probes/${lbl}-${name}.txt" 2>&1 || true
            sg_raw -o "$OUT/read-element-status-probes/${lbl}-${name}.bin" -r 65536 \
                "$d" $cdb \
                > /dev/null 2>&1 || true
        done

        # -- OPTIONAL: INITIALIZE ELEMENT STATUS first, then RES ------
        # 07h (INIT) commands the library to physically re-scan every slot.
        # ROBOT MOVES. Opt in via --with-init only.
        if (( WITH_INIT == 1 )); then
            say "  INITIALIZE ELEMENT STATUS on $d (robot will move)"
            sg_raw -v "$d" 07 00 00 00 00 00 \
                > "$OUT/read-element-status-probes/${lbl}-init-elem-status.txt" 2>&1 || true
            # After INIT, run the standard DVCID-safe variant again — if HPE's
            # DVCID drop is "stale-inventory" related, this is where it would
            # finally include identifiers.
            sg_raw -v -r 65536 "$d" b8 10 00 00 01 00 02 01 00 00 00 00 \
                > "$OUT/read-element-status-probes/${lbl}-post-init-dvcid.txt" 2>&1 || true
            sg_raw -o "$OUT/read-element-status-probes/${lbl}-post-init-dvcid.bin" -r 65536 \
                "$d" b8 10 00 00 01 00 02 01 00 00 00 00 \
                > /dev/null 2>&1 || true
        fi

        # -- changer LOG SENSE + MODE SENSE ---------------------------
        # Per HPE doc Table of Contents, the changer exposes these pages.
        sg_logs -a "$d" > "$OUT/log-sense/${lbl}-all.txt"        2>&1 || true
        sg_logs -L "$d" > "$OUT/log-sense/${lbl}-supported.txt"  2>&1 || true
        # 00=supported-pages, 07=event log, 0D=temperature, 2E=tape alert,
        # 30=statistics, 34=error log, 36=HPE event log, 38=HPE event desc,
        # 3A=HPE key def, 3B=HPE key value def, 3E=HPE device status
        for p in 00 07 0d 2e 30 34 36 38 3a 3b 3e; do
            sg_logs --page=0x$p     "$d" > "$OUT/log-sense/${lbl}-p${p}.txt" 2>&1 || true
            sg_logs --page=0x$p -H "$d" > "$OUT/log-sense/${lbl}-p${p}.hex" 2>&1 || true
        done
        # Changer MODE SENSE pages of interest: 0a=control extension,
        # 1c=tape alert, 1d=element address assignment (defines bay layout!),
        # 1e=transport geometry, 1f=device capabilities.
        for mp in 0a 1c 1d 1e 1f; do
            sg_modes --page=0x$mp -v "$d" > "$OUT/mode-sense/${lbl}-mp${mp}.txt" 2>&1 || true
        done
    fi

    # ---- tape-specific: log pages + mode pages ---------------------
    if [[ "$kind" == "tape" ]]; then
        # All log pages, supported-pages list + each one verbose
        sg_logs -a "$d"        > "$OUT/log-sense/${lbl}-all.txt"        2>&1 || true
        sg_logs -L "$d"        > "$OUT/log-sense/${lbl}-supported.txt"  2>&1 || true

        # Specific pages worth capturing per HPE 20-STG-TAPESCSIREF-ED5 +
        # the original plan.txt list:
        #   02 error counters write, 03 error counters read,
        #   06 non-medium errors, 0C sequential-access,
        #   0D temperature, 11 dt device status, 17 volume statistics,
        #   2E tape alert, 30 statistics, 31 hp-vendor extras,
        #   34 error log, 36 HPE event log, 38 HPE event description,
        #   3A HPE key def, 3B HPE key value def, 3E HPE device status.
        for p in 02 03 06 0c 0d 11 17 2e 30 31 34 36 38 3a 3b 3e; do
            sg_logs --page=0x$p     "$d" > "$OUT/log-sense/${lbl}-p${p}.txt" 2>&1 || true
            sg_logs --page=0x$p -H "$d" > "$OUT/log-sense/${lbl}-p${p}.hex" 2>&1 || true
        done

        # MODE SENSE pages for tape drives:
        # 0a=control extension, 0f=data compression, 10=device config,
        # 1c=informational exceptions.
        for mp in 0a 0f 10 1c; do
            sg_modes --page=0x$mp -v "$d" > "$OUT/mode-sense/${lbl}-mp${mp}.txt" 2>&1 || true
        done
    fi
done

# -------- pack it up -------------------------------------------------
say "packing tarball"
TAR="${OUT}.tar.gz"
tar -czf "$TAR" -C "$(dirname "$OUT")" "$(basename "$OUT")"
ls -lh "$TAR" "$OUT"

cat <<EOF

DONE. Output:
  directory: $OUT
  tarball:   $TAR
  log:       $LOG

To copy back to akash (run from akash):
  scp ${USER:-root}@$HOST:$TAR ~/remanence/fixtures/real-hardware/
  cd ~/remanence/fixtures/real-hardware && tar xzf $(basename "$TAR")
EOF
