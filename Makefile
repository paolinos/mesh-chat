.PHONY: build run test fmt clippy

build:
	cargo build

run:
	cargo run -- $(ARGS)

test:
	cargo test --all-targets

fmt:
	cargo fmt --all

clippy:
	cargo clippy --all-targets -- -D warnings
