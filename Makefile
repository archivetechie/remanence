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

.PHONY: rem-dev rem-dev-setcap rem-dev-libraries proof-inventory

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
