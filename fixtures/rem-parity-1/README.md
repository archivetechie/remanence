# REM-PARITY 1.0 conformance vectors

`vectors.json` seeds the arithmetic values for the REM-PARITY 1.0 vector set.
The publication builder expands it into a standalone machine-readable index
covering every Section 17 positive image, single-fault negative, recovery
refusal, and damage-matrix cell. Each indexed vector carries its own artifact
list and checksum; the archive also contains `verify.py`, so checking the
distribution does not require a Remanence source checkout.

Positive image bytes are emitted through the production Rust codecs and resume
planner used by the crate tests. The publication archive also carries the RAO
byte streams and negative-vector manifests needed to exercise the
payload-format side of the combined specification packet.
