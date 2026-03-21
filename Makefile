# ArcBox Development Makefile

PROFILE ?= debug
ENTITLEMENTS := bundle/arcbox.entitlements
AGENT_TARGET := aarch64-unknown-linux-musl

# Signing identity: auto-detect "Developer ID Application: ArcBox, Inc."
# from keychain. Override with: make sign SIGN_IDENTITY="..."
# Use SIGN_IDENTITY=- for ad-hoc signing (won't work with Virtualization.framework
# on recent macOS).
SIGN_IDENTITY ?= $(shell security find-identity -v -p codesigning 2>/dev/null \
	| grep -o '"Developer ID Application: ArcBox, Inc\.[^"]*"' \
	| head -1 | tr -d '"')

BINARIES := arcbox-daemon arcbox-helper abctl

ifeq ($(PROFILE),release)
  CARGO_FLAGS := --release
  TARGET_DIR := target/release
else
  CARGO_FLAGS :=
  TARGET_DIR := target/debug
endif

.PHONY: build build-release build-cli build-daemon build-helper build-agent \
        test check fmt clean \
        setup-boot-assets sign sign-daemon sign-all verify run-daemon

## ── Build ──────────────────────────────────────────────

build:
	cargo build $(CARGO_FLAGS)

build-release:
	$(MAKE) build PROFILE=release

build-cli:
	cargo build -p arcbox-cli $(CARGO_FLAGS)

build-daemon:
	cargo build -p arcbox-daemon $(CARGO_FLAGS)

build-helper:
	cargo build -p arcbox-helper $(CARGO_FLAGS)

build-agent:
	cargo build -p arcbox-agent --target $(AGENT_TARGET) --release

## ── Quality ────────────────────────────────────────────

check:
	cargo clippy --workspace --all-targets -- -D warnings
	cargo fmt --check

fmt:
	cargo fmt

test:
	cargo test --workspace

## ── Code Signing ─────────────────────────────────────

sign-daemon: build-daemon
	@if [ -z "$(SIGN_IDENTITY)" ]; then \
		echo "ERROR: No Developer ID signing identity found." >&2; \
		echo "  Install the ArcBox Developer ID certificate or set SIGN_IDENTITY:" >&2; \
		echo "  make sign-daemon SIGN_IDENTITY=\"Developer ID Application: ...\"" >&2; \
		exit 1; \
	fi
	codesign --force --options runtime \
		--identifier com.arcboxlabs.desktop.daemon \
		--entitlements $(ENTITLEMENTS) \
		--sign "$(SIGN_IDENTITY)" \
		$(TARGET_DIR)/arcbox-daemon
	@codesign -v --deep --strict $(TARGET_DIR)/arcbox-daemon && echo "✓ arcbox-daemon signed"

sign-all: build
	@if [ -z "$(SIGN_IDENTITY)" ]; then \
		echo "ERROR: No Developer ID signing identity found." >&2; \
		exit 1; \
	fi
	codesign --force --options runtime \
		--identifier com.arcboxlabs.desktop.daemon \
		--entitlements $(ENTITLEMENTS) \
		--sign "$(SIGN_IDENTITY)" \
		$(TARGET_DIR)/arcbox-daemon
	codesign --force --options runtime \
		--identifier com.arcboxlabs.desktop.helper \
		--sign "$(SIGN_IDENTITY)" \
		$(TARGET_DIR)/arcbox-helper
	codesign --force --options runtime \
		--identifier com.arcboxlabs.desktop.cli \
		--sign "$(SIGN_IDENTITY)" \
		$(TARGET_DIR)/abctl
	@for bin in $(BINARIES); do \
		codesign -v --deep --strict $(TARGET_DIR)/$$bin && echo "✓ $$bin signed"; \
	done

# Legacy ad-hoc sign (kept for CI smoke tests where no Developer ID exists).
sign:
	codesign --force --options runtime \
		--entitlements $(ENTITLEMENTS) \
		-s - $(TARGET_DIR)/arcbox-daemon

verify:
	@for bin in $(BINARIES); do \
		if [ -f $(TARGET_DIR)/$$bin ]; then \
			echo "--- $$bin ---"; \
			codesign -d -v --entitlements :- $(TARGET_DIR)/$$bin 2>&1 | head -5; \
			echo; \
		fi; \
	done

## ── Dev Workflow ───────────────────────────────────────

setup-boot-assets:
	./scripts/setup-dev-boot-assets.sh

run-daemon: sign-daemon
	SIGN=0 ./scripts/rebuild-run-daemon.sh

## ── Cleanup ───────────────────────────────────────────

clean:
	cargo clean
