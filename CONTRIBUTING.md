# Contributing to Remanence

Thank you for your interest. Remanence is young and pre-production, so the
most valuable contributions right now are careful ones: bug reports with
enough detail to reproduce, questions that reveal unclear documentation,
and independent implementations of the published formats.

## Ground rules

- **The specifications are the contract.** The on-disk and on-tape formats
  are defined by the documents in `specs/publication/`, not by this
  implementation's behavior. If the code and a specification disagree,
  that is a bug in one of them — please report it either way. Changes that
  would alter published bytes are effectively format revisions and need a
  specification change first.
- **Determinism is load-bearing.** Object writing is byte-deterministic by
  design; the test-vector archive pins exact bytes. Any change to the
  write path must leave every pinned vector byte-identical (or come with
  the case for a format revision).
- **Stored bytes are hostile.** Readers treat everything from media as
  untrusted input. New parsing code should fail closed with a classified
  error, never guess, and ideally come with negative tests or fuzz
  coverage.

## Practicalities

Build and test as CI does:

```sh
cargo fmt --all --check
cargo clippy --workspace --exclude remanence-chaos --all-targets -- -D warnings
cargo test --workspace --exclude remanence-chaos
```

Hardware-touching tests are ignored by default and opt in via environment
variables documented in their test modules. `make proof-inventory` checks
the formal-verification estate (`verif/`) if you touch proved code paths.

Pull requests should be small and single-purpose, with tests. Commit
history is linear; no merge commits.

## Security issues

Do not open public issues for suspected vulnerabilities — see
[SECURITY.md](SECURITY.md).

## Questions

Open a GitHub issue, or write to specs@archivetech.org for anything about
the format specifications themselves.
