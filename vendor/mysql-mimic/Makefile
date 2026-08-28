.PHONY: all build test lint fmt fmt-check clean ci

all: build

build:
	cargo build

test:
	cargo test --all-features

lint:
	cargo clippy --all-targets --all-features -- -D warnings

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all --check

clean:
	cargo clean

ci: fmt-check lint test
