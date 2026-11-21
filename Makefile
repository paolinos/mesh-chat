.PHONY: build run test fmt clippy dist release push-release \
	build-linux-x64 build-linux-arm64 build-macos-x64 build-macos-arm64 \
	build-windows-x64 build-windows-arm64

# Optimized build for the current host (target/release/mesh-chat).
build:
	cargo build --release --locked

run:
	cargo run --locked -- $(ARGS)

test:
	cargo test --all-targets --locked

fmt:
	cargo fmt --all

clippy:
	cargo clippy --all-targets --locked -- -D warnings

# Single cross-target builds (delegated to the release script so toolchain
# setup, ZIP naming and host checks stay in one place).
build-linux-x64:
	./scripts/release.sh --no-push --only linux-x64

build-linux-arm64:
	./scripts/release.sh --no-push --only linux-arm64

build-macos-x64:
	./scripts/release.sh --no-push --only macos-x64

build-macos-arm64:
	./scripts/release.sh --no-push --only macos-arm64

build-windows-x64:
	./scripts/release.sh --no-push --only windows-x64

build-windows-arm64:
	./scripts/release.sh --no-push --only windows-arm64

# Build everything this host can produce and write dist/*.zip.
dist:
	./scripts/release.sh --no-push

# Build on this host, then publish if all six archives are present.
release:
	./scripts/release.sh

# Publish already-built dist/*.zip without rebuilding.
push-release:
	./scripts/release.sh --push-only
