# REM-PARITY 1.0 conformance vectors

`vectors.json` is the machine-readable claim map for the REM-PARITY 1.0
vectors. Arithmetic values are independently reproducible from Sections 5--7
of the publication specification. Scanner, overlay, recovery, resume, and
single-damage cases name the exact Rust test entrypoints that exercise the
corresponding on-tape structures; `cargo test -p remanence-parity` runs the
complete set and fails if a subject cannot be exercised.

The publication archive also carries the RAO byte streams and negative-vector
manifests needed to exercise the payload-format side of the combined
specification packet.
