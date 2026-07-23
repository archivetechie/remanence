# Remanence licensing

Remanence uses path-specific licenses. A more specific path below takes
precedence over a broader one.

| Path | SPDX identifier | License text |
| --- | --- | --- |
| `crates/**` | `Apache-2.0` | [`LICENSE-APACHE-2.0`](LICENSE-APACHE-2.0) |
| `fieldtest/tools/remfield-io/**` | `Apache-2.0` | [`LICENSE-APACHE-2.0`](LICENSE-APACHE-2.0) |
| `specs/**` | `CC-BY-4.0` | [`LICENSE-CC-BY-4.0`](LICENSE-CC-BY-4.0) |
| `specs/publication/remanence-test-vectors.tar` | `CC0-1.0` | [`LICENSE-CC0-1.0`](LICENSE-CC0-1.0) |
| `fixtures/**` | `CC0-1.0` | [`LICENSE-CC0-1.0`](LICENSE-CC0-1.0) |

These assignments supersede any prior whole-repository AGPL license
declaration. The `fixtures/` tree and the published vector archive are CC0 so
independent implementers may freely vendor the conformance vectors. The
Zenodo record metadata is updated to match this split at release.
