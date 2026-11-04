#!/usr/bin/env python3
"""Capture selected drives' host limits without issuing a SCSI or tape command.

Use the same SG_SET_RESERVED_SIZE / SG_GET_RESERVED_SIZE pair as the Rust
transport. The READ(6) maximum covers every current plan's single identity read.
Sysfs values explain constraints; only the actual sg grant bounds the probe.
"""
import array
import fcntl
import json
from pathlib import Path
import re
import sys

READ6_MAX = 0xFFFFFF


def reserved_grant(node, wanted):
    """Request and query the buffer on one fd; neither ioctl dispatches a CDB."""
    with open(node, "rb", buffering=0) as device:
        size = array.array("i", [wanted])
        fcntl.ioctl(device, 0x2275, size, True)  # SG_SET_RESERVED_SIZE
        size[0] = 0
        fcntl.ioctl(device, 0x2272, size, True)  # SG_GET_RESERVED_SIZE
        if size[0] < 0:
            raise ValueError(f"negative reserved-buffer grant: {size[0]}")
        return size[0]


def capture(discovery, library, sysfs=Path("/sys"), reserve=reserved_grant):
    """Join selected-library drive endpoints to sysfs; missing data stays explicit."""
    warnings = []

    def value(path):
        try:
            return path.read_text().strip()
        except OSError as error:
            return f"unavailable: {error.strerror}"

    parameters = {name: value(sysfs / "module/sg/parameters" / name)
                  for name in ("allow_dio", "def_reserved_size", "scatter_elem_sz")}
    drives = []
    for endpoint in discovery.get("observed_endpoints", []):
        if endpoint.get("library_serial") != library or not endpoint.get("drive_serial"):
            continue
        node = endpoint.get("path")
        drive = {"serial": endpoint["drive_serial"], "sg_node": node,
                 "requested_bytes": READ6_MAX, "granted_bytes": None}
        drives.append(drive)
        if not node or not re.fullmatch(r"/dev/sg[0-9]+", node):
            warnings.append(f"drive {drive['serial']}: no sg node; cannot measure host limit")
            continue
        device = (sysfs / "class/scsi_generic" / Path(node).name / "device").resolve()
        host = next((part for part in reversed(device.parts) if re.fullmatch(r"host[0-9]+", part)), None)
        drive["scsi_host"] = host
        drive["sg_tablesize"] = value(sysfs / "class/scsi_host" / host / "sg_tablesize") if host else "unavailable: host not found"
        drive["max_sectors"] = value(sysfs / "class/scsi_host" / host / "max_sectors") if host else "unavailable: host not found"
        try:
            drive["granted_bytes"] = reserve(node, READ6_MAX)
        except (OSError, ValueError) as error:
            warnings.append(f"drive {drive['serial']} ({node}): reserved-buffer query failed: {error}")
            continue
        if drive["granted_bytes"] < 16 * 1024 * 1024:
            warnings.append(
                f"drive {drive['serial']} ({node}) grants {drive['granted_bytes']} bytes "
                f"for a {READ6_MAX}-byte request, below 16 MiB. Identity READs are capped "
                "at the grant (and READ(6)'s 16 MiB minus 1); larger physical records "
                "are refused, never treated as blank. Raise sg scatter_elem_sz or use "
                "a host with a larger per-command limit; adapter limits may still apply.")
    if not drives:
        warnings.append(f"no discovered drive endpoints for {library}; host limit is unknown")
    return {"library_serial": library, "sg_module_parameters": parameters,
            "largest_single_read_bytes": READ6_MAX, "drives": drives, "warnings": warnings}


if __name__ == "__main__":
    print(json.dumps(capture(json.loads(Path(sys.argv[1]).read_text()), sys.argv[2]), indent=2))
