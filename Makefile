# Development helpers for Remanence.
#
# Linux stores file capabilities as xattrs on the generated binary.
# Rebuilding `target/debug/rem` can replace that inode and drop
# CAP_SYS_RAWIO, so hardware-facing dev workflows should use
# `make rem-dev` instead of `cargo run`.

CARGO ?= cargo
GETCAP ?= getcap
REM_BIN ?= target/debug/rem
SETCAP ?= setcap
SUDO ?= sudo

.PHONY: rem-dev rem-dev-setcap rem-dev-libraries proof-inventory \
	in-progress-parity-test-vectors verify-in-progress-parity-test-vectors \
	publication-test-vectors verify-publication-test-vectors \
	benchmark-terminal-index-stream benchmark-terminal-index-journal-replay

rem-dev: rem-dev-setcap

rem-dev-setcap:
	@if [ "$$(uname -s)" != "Linux" ]; then \
		echo "error: rem-dev-setcap requires Linux file capabilities"; \
		exit 1; \
	fi
	$(CARGO) build -p remanence-cli
	$(SUDO) $(SETCAP) cap_sys_rawio+ep $(REM_BIN)
	$(GETCAP) $(REM_BIN)

rem-dev-libraries: rem-dev-setcap
	$(REM_BIN) libraries

proof-inventory:
	./verif/check-inventory.sh

in-progress-parity-test-vectors:
	$(CARGO) run -p remanence-parity --example generate_terminal_index_vectors -- \
		fixtures/rem-parity-terminal-index-draft

verify-in-progress-parity-test-vectors:
	python3 tools/verify_terminal_index_vectors.py \
		fixtures/rem-parity-terminal-index-draft

publication-test-vectors:
	python3 tools/build_publication_test_vectors.py

verify-publication-test-vectors:
	python3 tools/verify_publication_test_vectors.py
	python3 tools/check_spec_versioning.py

check-spec-versioning:
	python3 tools/check_spec_versioning.py

# Observational performance report; never writes unless --report is supplied
# through TERMINAL_INDEX_BENCH_ARGS.
benchmark-terminal-index-stream:
	$(CARGO) run --release -p remanence-parity \
		--example benchmark_terminal_index_stream -- $(TERMINAL_INDEX_BENCH_ARGS)

# Checkpoint-journal replay companion to the synthetic record-source benchmark.
benchmark-terminal-index-journal-replay:
	$(CARGO) run --release -p remanence-state \
		--example terminal_index_replay_benchmark -- $(TERMINAL_INDEX_REPLAY_BENCH_ARGS)
