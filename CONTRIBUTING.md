# Contributing to Remanence

Thank you for your interest. Remanence is young and pre-production, and the most useful help right now comes from
people who use it and tell us what they find: a bug, an unclear page, a specification that can be read two ways, or a
drive, library or filesystem that behaves differently from the ones we test on. We read every report, and we are
grateful for each one.

## How to help

Open an issue. These are the kinds we find most useful:

- **A bug report.** The command you ran, what happened and what you expected, the version or commit, and the smallest
  reproduction you can make, on synthetic data if possible. Paste commands and output as text rather than screenshots.
- **A test result from your own hardware.** The drive or library model, its firmware, the cartridge type, and the
  output of `rem doctor` and of the commands you ran. A result that shows something working is as useful to us as a
  failure, because it tells us which setups are known to work. We test on a small number of drives, so results from
  other hardware fill a real gap.
- **A documentation error.** The page and the sentence.
- **A specification defect.** The section and, where you have them, the bytes.
- **A question or a proposal.**

If you have found the cause of a problem, please describe it in the issue. A short diff is welcome when it is the
clearest way to show what you mean; we read it as part of your diagnosis.

We answer every issue as well as we can, but we cannot promise how soon.

## Why we do not merge pull requests

Every change to Remanence is written inside the project and goes through the same review and testing, so we do not
merge code from outside the project, however good it is.

When an issue shows us a problem, we reproduce it and write the fix ourselves, and the change links back to the
issue. Please do not take this as a judgment of your work. A careful report with a diagnosis is the most valuable
thing you can send us.

The code is still open source. It is published under the Apache License 2.0, the specifications under CC-BY-4.0 and
the test vectors under CC0-1.0 (see [LICENSING.md](LICENSING.md)), and you may fork, change and redistribute them under
those licenses. An independent implementation checked against the published test vectors is especially welcome,
and so is a report of where the vectors are wrong.

## How the project works

- **The specifications are the contract.** REM-OBJECT and REM-ENCRYPT are defined by the review documents in
  `specs/publication/`, not by this implementation's behavior. REM-PARITY is temporarily split: the publication
  candidate defines generation 1 while current code follows the generation-2 replacement in `specs/in-progress/`. If
  code and its governing text disagree, that is a bug in one of them, and we would like to hear about it either way.
- **Determinism is load-bearing.** Object writing is byte-deterministic by design, and the test-vector archive pins
  exact bytes. A change that would alter published bytes is a format revision and needs a specification change first.
- **Stored bytes are hostile.** Readers treat everything read from media as untrusted input and fail closed with a
  classified error rather than guess.

## Building and testing

To build and test as CI does:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Hardware-touching tests are ignored by default and opt in through environment variables documented in their test
modules. `make proof-inventory` checks the formal-verification estate (`verif/`).

## Security issues

Please do not open public issues for suspected vulnerabilities. See [SECURITY.md](SECURITY.md).

## Questions

Open a GitHub issue, or write to specs@archivetech.org for anything about the format specifications themselves.
