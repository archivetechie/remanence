#!/usr/bin/env bash
# Reproducibly seed the rem_parity_* fuzz corpora from the published,
# pinned test-vector archive. Idempotent; never modifies the archive.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ARCHIVE="$ROOT/specs/publication/remanence-test-vectors.tar"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

tar -C "$TMP" -xf "$ARCHIVE" \
  rem-parity-1/positive/minimal-image \
  rem-parity-1/damage-matrix/bootstrap-copy \
  rem-parity-1/damage-matrix/multi-parity-map-selection

seed() { # seed <target> <file> <name>
  mkdir -p "$ROOT/fuzz/corpus/$1"
  cp "$2" "$ROOT/fuzz/corpus/$1/$3"
}

MIN="$TMP/rem-parity-1/positive/minimal-image"

# Bootstrap parser: both bootstrap tape files, first block each.
head -c 4096 "$MIN/tape-file-000-bootstrap.bin" > "$TMP/boot0"
head -c 4096 "$MIN/tape-file-003-final-bootstrap.bin" > "$TMP/boot3"
seed rem_parity_bootstrap_parse "$TMP/boot0" minimal-boot0
seed rem_parity_bootstrap_parse "$TMP/boot3" minimal-boot3

# Structure-aware bootstrap target: its input describes a block rather than
# being one, so seeds are in that description format. The valuable seed is a
# real pinned CBOR payload in raw-passthrough mode — because the harness
# recomputes both CRCs, mutations of it stay framing-valid and land in the
# decoder instead of dying at the integrity check.
python3 - "$MIN/tape-file-000-bootstrap.bin" "$TMP/structured-raw" "$TMP/structured-built" <<'PY'
import struct, sys
src, raw_out, built_out = sys.argv[1], sys.argv[2], sys.argv[3]
blk = open(src, "rb").read()
payload_len = struct.unpack_from("<I", blk, 40)[0]
payload = blk[0x34 : 0x34 + payload_len]
minor = blk[10:12]
block_size = blk[32:36]
sequence = blk[36:40]
uuid = blk[16:32]
# control bit1 set = raw-payload mode; header description mirrors the vector.
open(raw_out, "wb").write(b"\x02" + minor + block_size + sequence + uuid + payload)
# control 0 = structured-payload mode; give the builder real bytes to chew on.
open(built_out, "wb").write(b"\x00" + minor + block_size + sequence + uuid + payload[:256])
PY
seed rem_parity_bootstrap_structured "$TMP/structured-raw" minimal-boot0-raw
seed rem_parity_bootstrap_structured "$TMP/structured-built" minimal-boot0-built

# Sidecar parser: whole sidecar tape file (block-count prefix byte 0x06 = 7 blocks).
printf '\x06' | cat - "$MIN/tape-file-002-sidecar.bin" > "$TMP/sidecar-seed"
seed rem_parity_sidecar_parse "$TMP/sidecar-seed" minimal-sidecar

# Parity-map parser: the multi-parity-map damage vector's source artifact.
MPS="$TMP/rem-parity-1/damage-matrix/multi-parity-map-selection/source-artifact.bin"
if [[ -f "$MPS" ]]; then
  printf '\x02' | cat - "$MPS" > "$TMP/map-seed"
  seed rem_parity_map_parse "$TMP/map-seed" multi-map-source
fi

# Scan walk: synthesize a well-formed tuple stream over the minimal image's
# structure (uuid + bootstrap/object/sidecar/bootstrap files with filemarks).
python3 - "$TMP" <<'PY'
import pathlib, sys
tmp = pathlib.Path(sys.argv[1])
uuid = b"rem-fuzz-tape-01"
# tags: 2=bootstrap-magic block, 6=data block, 3=sidecar-magic block, 0=filemark, 1=unreadable
tape = bytes([2,0, 0,0, 6,65, 6,66, 0,0, 3,0, 6,67, 0,0, 2,1, 0,0, 1,0])
(tmp / "walk-seed").write_bytes(uuid + tape)
PY
seed rem_parity_scan_walk "$TMP/walk-seed" structured-walk

echo "seeded:"
for t in rem_parity_bootstrap_parse rem_parity_bootstrap_structured rem_parity_sidecar_parse rem_parity_map_parse rem_parity_scan_walk; do
  echo "  $t: $(ls "$ROOT/fuzz/corpus/$t" | wc -l) file(s)"
done
