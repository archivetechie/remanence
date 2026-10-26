#!/usr/bin/env bash
set -euo pipefail

SCRIPT_NAME="33-diag-capture"
# shellcheck disable=SC1091
source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

usage() {
  cat <<'EOF'
Usage: 33-diag-capture.sh [--mam-only] [--with-diag-pages] [--note TEXT] [--help]

Read-only SCSI diagnostics capture over the selected library's changer and
drives, as raw evidence for drive and media health work:

  INQUIRY and every VPD page; LOG SENSE, all pages and subpages (decoded, hex,
  and one raw file per page); MODE SENSE, all pages (decoded, hex, raw);
  REPORT SUPPORTED OPCODES; REQUEST SENSE; TEST UNIT READY; READ ATTRIBUTE
  (cartridge memory, MAM) for every drive that has a cartridge loaded; and the
  st driver statistics from sysfs.

It never writes, loads, unloads or moves anything. Run it whenever a
cartridge of interest is loaded (after init, after a write leg, after a
fault leg); every run lands in its own evidence directory.

  --mam-only         only TEST UNIT READY + READ ATTRIBUTE on the drives
  --with-diag-pages  also list supported diagnostic pages (RECEIVE DIAGNOSTIC
                     RESULTS page 0 through sg_senddiag --list); off by default
  --note TEXT        free text recorded with this run (what is loaded, why)
EOF
}

main() {
  local mam_only=0 with_diag=0 note=""
  while (($#)); do
    case "$1" in
      --help|-h) usage; exit 0 ;;
      --mam-only) mam_only=1; shift ;;
      --with-diag-pages) with_diag=1; shift ;;
      --note) note="${2:-}"; shift 2 ;;
      *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
  done

  fieldtest_init_layout
  fieldtest_detect_env || true
  local serial
  if ! serial="$(fieldtest_selected_library_serial)"; then
    echo "error: no selected library; run 01-allowlist.sh (or 03-bringup.sh) first" >&2
    exit 1
  fi
  # Direct-SCSI phase: the daemon must NOT be running. TEST UNIT READY and
  # REQUEST SENSE consume unit attentions the daemon relies on, and reading
  # TapeAlert clears it on most drives. Stop the daemon; cartridges stay seated.
  if [[ -S "$(fieldtest_socket_path)" ]]; then
    echo "error: rem-daemon appears to be running; run 03-bringup.sh --stop first" >&2
    exit 1
  fi

  local missing=() tool
  for tool in sg_inq sg_logs sg_modes sg_read_attr sg_opcodes sg_requests sg_turs; do
    command -v "$tool" >/dev/null 2>&1 || missing+=("$tool")
  done
  if ((${#missing[@]})); then
    fieldtest_evidence_record "$SCRIPT_NAME" tools FAIL "missing sg3_utils tools: ${missing[*]} (install sg3_utils)"
    exit 1
  fi
  # READ ATTRIBUTE (0x8C) is outside the kernel's unprivileged SG_IO allowlist,
  # unlike INQUIRY, LOG SENSE and MODE SENSE. A copy of sg_read_attr in the kit's
  # bin/ with cap_sys_rawio (same treatment as rem) makes MAM readable without
  # root; otherwise MAM is recorded as SKIP with the remedy.
  SG_READ_ATTR="${SG_READ_ATTR:-$(fieldtest_bin_dir)/sg_read_attr}"
  [[ -x "$SG_READ_ATTR" ]] || SG_READ_ATTR="sg_read_attr"

  local stamp libs_json
  stamp="$(fieldtest_timestamp_id)"
  libs_json="$(fieldtest_artifact_path "$SCRIPT_NAME" libraries "$stamp")"
  if ! fieldtest_capture_json "$libs_json" "$(fieldtest_rem_bin)" libraries --json; then
    fieldtest_evidence_record "$SCRIPT_NAME" discovery FAIL "rem libraries --json failed" "$libs_json"
    exit 1
  fi

  # One line per endpoint of the selected library: role path serial bay
  local endpoints
  endpoints="$(
    python3 - "$libs_json" "$serial" <<'PY'
import json
import sys
from pathlib import Path

payload = json.loads(Path(sys.argv[1]).read_text())
selected = sys.argv[2]
seen = set()
for ep in payload.get("observed_endpoints", []):
    path = ep.get("path") or ""
    if not path or path in seen:
        continue
    kind = ep.get("kind")
    if kind == "library" and ep.get("serial") == selected and not ep.get("drive_serial"):
        print(f"changer {path} {ep.get('serial')} -")
        seen.add(path)
    elif ep.get("library_serial") == selected and ep.get("drive_serial"):
        print(f"drive {path} {ep.get('drive_serial')} {ep.get('bay', '-')}")
        seen.add(path)
PY
  )"
  if [[ -z "$endpoints" ]]; then
    fieldtest_evidence_record "$SCRIPT_NAME" discovery FAIL "no changer or drive endpoints resolved for library $serial (capabilities missing?)" "$libs_json"
    exit 1
  fi

  local run_dir
  run_dir="$(fieldtest_script_evidence_dir "$SCRIPT_NAME")/$stamp"
  mkdir -p "$run_dir"
  printf '%s\n' "$endpoints" >"$run_dir/endpoints.txt"
  [[ -n "$note" ]] && printf '%s\n' "$note" >"$run_dir/note.txt"

  if [[ $mam_only -eq 0 ]]; then
    capture_host "$run_dir"
  fi

  local role path dev_serial bay
  while read -r role path dev_serial bay; do
    [[ -n "$role" ]] || continue
    local dev_dir="$run_dir/$role-$dev_serial"
    mkdir -p "$dev_dir"
    if [[ $mam_only -eq 0 ]]; then
      capture_device "$dev_dir" "$path" "$role" "$with_diag"
      fieldtest_evidence_record "$SCRIPT_NAME" "$role-$dev_serial" PASS "captured INQUIRY/VPD, LOG SENSE, MODE SENSE, opcodes, sense ($path)" "$dev_dir" \
        "{\"device\":\"$path\",\"role\":\"$role\",\"bay\":\"$bay\"}"
    fi
    if [[ $role == drive ]]; then
      capture_mam "$dev_dir" "$path" "$dev_serial" "$bay"
    fi
  done <<<"$endpoints"

  fieldtest_evidence_record "$SCRIPT_NAME" run PASS "diagnostics capture complete" "$run_dir" \
    "{\"library\":\"$serial\",\"mam_only\":$mam_only,\"with_diag_pages\":$with_diag}"
}

capture_host() {
  local dir="$1/host"
  mkdir -p "$dir"
  fieldtest_capture_text "$dir/lsscsi-g.json" lsscsi -g || true
  fieldtest_capture_text "$dir/lsscsi-gvv.json" lsscsi -gvv || true
  fieldtest_capture_text "$dir/uname.json" uname -a || true
  fieldtest_capture_text "$dir/sg-parameters.json" bash -c 'for f in /sys/module/sg/parameters/*; do printf "%s=%s\n" "$(basename "$f")" "$(cat "$f" 2>/dev/null)"; done' || true
  fieldtest_capture_text "$dir/st-parameters.json" bash -c 'for f in /sys/module/st/parameters/* /sys/bus/scsi/drivers/st/*/options; do [ -e "$f" ] && printf "%s=%s\n" "$f" "$(cat "$f" 2>/dev/null)"; done; true' || true
  fieldtest_capture_text "$dir/sg3-utils-version.json" sg_logs --version || true
}

# $1 dir, $2 device, $3 role, $4 with_diag
capture_device() {
  local dir="$1" dev="$2" role="$3" with_diag="$4"
  fieldtest_capture_text "$dir/inquiry.json" sg_inq -d "$dev" || true
  fieldtest_capture_text "$dir/inquiry-hex.json" sg_inq -H "$dev" || true
  if command -v sg_vpd >/dev/null 2>&1; then
    fieldtest_capture_text "$dir/vpd-all.json" sg_vpd --all "$dev" || true
    fieldtest_capture_text "$dir/vpd-all-hex.json" sg_vpd --all -H "$dev" || true
  fi
  fieldtest_capture_text "$dir/vpd-80.json" sg_inq -p 0x80 "$dev" || true
  fieldtest_capture_text "$dir/vpd-83.json" sg_inq -p 0x83 "$dev" || true
  fieldtest_capture_text "$dir/tur.json" sg_turs -v "$dev" || true
  fieldtest_capture_text "$dir/request-sense.json" sg_requests -v "$dev" || true
  fieldtest_capture_text "$dir/request-sense-hex.json" sg_requests -H "$dev" || true
  fieldtest_capture_text "$dir/opcodes.json" sg_opcodes "$dev" || true
  fieldtest_capture_text "$dir/opcodes-hex.json" sg_opcodes -H "$dev" || true

  # TapeAlert ordering probe: the TapeAlert Response page (0x12) is documented
  # as persistent while 0x2E clears on read on some drives. Read 0x12, then 0x2E
  # twice, then 0x12 again, in that order, so the evidence shows what cleared.
  if [[ $role == drive ]]; then
    date -u +%Y-%m-%dT%H:%M:%SZ >"$dir/log-tapealert-order.txt"
    fieldtest_capture_text "$dir/log-0x12-before.json" sg_logs -p 0x12 "$dev" || true
    sg_logs -p 0x12 -r "$dev" >"$dir/log-0x12-before.bin" 2>/dev/null || true
    fieldtest_capture_text "$dir/log-0x2e-first.json" sg_logs -p 0x2e "$dev" || true
    sg_logs -p 0x2e -r "$dev" >"$dir/log-0x2e-first.bin" 2>/dev/null || true
    fieldtest_capture_text "$dir/log-0x2e-second.json" sg_logs -p 0x2e "$dev" || true
    fieldtest_capture_text "$dir/log-0x12-after.json" sg_logs -p 0x12 "$dev" || true
  fi

  # LOG SENSE: supported pages and subpages, everything decoded, everything in hex,
  # then one raw file per page so a parser can be tested against exact bytes.
  # The discovered list is joined with the pages HPE LTO drives and MSL changers
  # are known to serve, because some changer firmware refuses page 0x00.
  fieldtest_capture_text "$dir/log-supported.json" sg_logs -p 0x00 "$dev" || true
  fieldtest_capture_text "$dir/log-supported-subpages.json" sg_logs -p 0x00,0xff "$dev" || true
  fieldtest_capture_text "$dir/log-all.json" sg_logs -A "$dev" || true
  fieldtest_capture_text "$dir/log-all-hex.json" sg_logs -A -H "$dev" || true
  mkdir -p "$dir/log-raw"
  local known_pages
  if [[ $role == drive ]]; then
    known_pages="0x02 0x03 0x06 0x0c 0x0d,0x01 0x11 0x12 0x14 0x16 0x17 0x17,0x01 0x17,0x02 0x18 0x1a 0x1b 0x30 0x31 0x32 0x33 0x34 0x37 0x37,0x10 0x37,0x20 0x37,0x40 0x37,0x50 0x37,0x60 0x37,0x80 0x37,0x90 0x37,0xa0 0x38 0x39 0x3a 0x3b 0x3c 0x3d 0x3e"
  else
    known_pages="0x07 0x0d 0x2e 0x30 0x34 0x36 0x38 0x3a 0x3b 0x3e"
  fi
  local pages
  pages="$(
    python3 - "$known_pages" "$dir/log-supported.json" "$dir/log-supported-subpages.json" <<'PY'
import json
import re
import sys
from pathlib import Path

pages = []
for f in sys.argv[2:]:
    try:
        text = json.loads(Path(f).read_text()).get("stdout", "")
    except Exception:
        continue
    for m in re.finditer(r"^\s+(0x[0-9a-fA-F]{2})(?:,(0x[0-9a-fA-F]{2}))?\s", text, re.M):
        page, sub = m.group(1).lower(), (m.group(2) or "").lower()
        key = (page, sub)
        if key not in pages:
            pages.append(key)
for spec in sys.argv[1].split():
    page, _, sub = spec.partition(",")
    key = (page.lower(), sub.lower())
    if key not in pages:
        pages.append(key)
for page, sub in pages:
    print(f"{page} {sub}")
PY
  )"
  local page sub spec name
  while read -r page sub; do
    [[ -n "$page" ]] || continue
    if [[ -n "$sub" ]]; then spec="$page,$sub"; name="${page}-${sub}"; else spec="$page"; name="$page"; fi
    sg_logs -p "$spec" -r "$dev" >"$dir/log-raw/$name.bin" 2>"$dir/log-raw/$name.err" || true
    [[ -s "$dir/log-raw/$name.err" ]] || rm -f "$dir/log-raw/$name.err"
    sg_logs -p "$spec" "$dev" >"$dir/log-raw/$name.txt" 2>&1 || true
  done <<<"$pages"

  # MODE SENSE: all pages, decoded, hex, raw; the three control pages individually.
  fieldtest_capture_text "$dir/mode-all.json" sg_modes -a "$dev" || true
  fieldtest_capture_text "$dir/mode-all-hex.json" sg_modes -a -H "$dev" || true
  sg_modes -a -r "$dev" >"$dir/mode-all.bin" 2>/dev/null || true
  fieldtest_capture_text "$dir/mode-all-subpages.json" sg_modes -A "$dev" || true
  if [[ $role == drive ]]; then
    local mp
    for mp in 0x0a 0x0f 0x10 0x1c 0x1d; do
      fieldtest_capture_text "$dir/mode-$mp.json" sg_modes -p "$mp" "$dev" || true
    done
    fieldtest_capture_text "$dir/mode-0x1c-changeable.json" sg_modes -p 0x1c -c "$dev" || true
    capture_st_stats "$dir" "$dev"
  else
    local mp
    for mp in 0x0a 0x1c 0x1d 0x1e 0x1f; do
      fieldtest_capture_text "$dir/mode-$mp.json" sg_modes -p "$mp" "$dev" || true
    done
  fi

  if [[ $with_diag -eq 1 ]]; then
    fieldtest_capture_text "$dir/diag-pages.json" sg_senddiag --list "$dev" || true
  fi
}

# st driver statistics for the st node that shares this sg node's SCSI device.
capture_st_stats() {
  local dir="$1" dev="$2" sg st
  sg="$(basename "$dev")"
  st=""
  local candidate
  for candidate in "/sys/class/scsi_generic/$sg/device/scsi_tape/"st*; do
    [[ -e "$candidate" ]] || continue
    candidate="$(basename "$candidate")"
    if [[ "$candidate" =~ ^st[0-9]+$ ]]; then
      st="$candidate"
      break
    fi
  done
  [[ -n "$st" ]] || return 0
  fieldtest_capture_text "$dir/st-stats.json" bash -c "for f in /sys/class/scsi_tape/$st/stats/*; do printf '%s=%s\n' \"\$(basename \"\$f\")\" \"\$(cat \"\$f\")\"; done" || true
  printf '%s\n' "$st" >"$dir/st-node.txt"
}

# $1 dir, $2 device, $3 drive serial, $4 bay
capture_mam() {
  local dir="$1" dev="$2" dev_serial="$3" bay="$4"
  if ! sg_turs "$dev" >/dev/null 2>&1; then
    fieldtest_evidence_record "$SCRIPT_NAME" "mam-$dev_serial" SKIP "no cartridge ready in drive $dev_serial ($dev); MAM not read" "$dir" \
      "{\"device\":\"$dev\",\"bay\":\"$bay\"}"
    return 0
  fi
  local mam="$dir/mam"
  mkdir -p "$mam"
  if ! fieldtest_capture_text "$mam/attributes.json" "$SG_READ_ATTR" "$dev"; then
    if grep -qiE "not permitted|EPERM|Operation not permitted" "$mam/attributes.json"; then
      fieldtest_evidence_record "$SCRIPT_NAME" "mam-$dev_serial" SKIP "READ ATTRIBUTE refused without cap_sys_rawio; copy sg_read_attr into $(fieldtest_bin_dir)/ and setcap it like rem, then rerun --mam-only" "$mam/attributes.json" \
        "{\"device\":\"$dev\",\"bay\":\"$bay\"}"
      return 0
    fi
  fi
  fieldtest_capture_text "$mam/attributes-hex.json" "$SG_READ_ATTR" -H "$dev" || true
  "$SG_READ_ATTR" -r "$dev" >"$mam/attributes.bin" 2>/dev/null || true
  local sa
  for sa in 1 2 3 5; do
    fieldtest_capture_text "$mam/service-action-$sa.json" "$SG_READ_ATTR" --sa="$sa" "$dev" || true
    fieldtest_capture_text "$mam/service-action-$sa-hex.json" "$SG_READ_ATTR" --sa="$sa" -H "$dev" || true
  done
  fieldtest_capture_text "$mam/log-0x17.json" sg_logs -p 0x17 "$dev" || true
  fieldtest_capture_text "$mam/log-0x17-sub1.json" sg_logs -p 0x17,0x01 "$dev" || true
  fieldtest_capture_text "$mam/log-0x17-sub2.json" sg_logs -p 0x17,0x02 "$dev" || true
  fieldtest_capture_text "$mam/log-0x31.json" sg_logs -p 0x31 "$dev" || true
  fieldtest_capture_text "$mam/log-0x2e.json" sg_logs -p 0x2e "$dev" || true
  local barcode
  barcode="$(python3 - "$mam/attributes.json" <<'PY'
import json
import re
import sys
from pathlib import Path

try:
    text = json.loads(Path(sys.argv[1]).read_text()).get("stdout", "")
except Exception:
    text = ""
m = re.search(r"Barcode:\s*(\S+)", text)
print(m.group(1) if m else "")
PY
  )"
  fieldtest_evidence_record "$SCRIPT_NAME" "mam-$dev_serial" PASS "read cartridge memory in drive $dev_serial${barcode:+ (barcode $barcode)}" "$mam" \
    "{\"device\":\"$dev\",\"bay\":\"$bay\",\"barcode\":\"$barcode\"}"
}

main "$@"
