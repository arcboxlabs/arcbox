# ArcBox Development Makefile

PROFILE ?= debug
ENTITLEMENTS := bundle/arcbox.entitlements
AGENT_TARGET := aarch64-unknown-linux-musl

ifeq ($(PROFILE),release)
  CARGO_FLAGS := --release
  TARGET_DIR := target/release
else
  CARGO_FLAGS :=
  TARGET_DIR := target/debug
endif

.PHONY: build build-release build-cli build-daemon build-agent \
        test check fmt clean \
        setup-boot-assets sign run-daemon

## ── Build ──────────────────────────────────────────────

build:
	cargo build $(CARGO_FLAGS)

build-release:
	$(MAKE) build PROFILE=release

build-cli:
	cargo build -p arcbox-cli $(CARGO_FLAGS)

build-daemon:
	cargo build -p arcbox-daemon $(CARGO_FLAGS)

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

## ── Dev Workflow ───────────────────────────────────────

setup-boot-assets:
	./scripts/setup-dev-boot-assets.sh

sign:
	codesign --force --options runtime \
		--entitlements $(ENTITLEMENTS) \
		-s - $(TARGET_DIR)/arcbox-daemon

run-daemon: build-cli build-daemon setup-boot-assets sign
	./scripts/rebuild-run-daemon.sh

## ── Cleanup ───────────────────────────────────────────

clean:
	cargo clean
