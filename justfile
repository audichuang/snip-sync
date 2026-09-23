default: build

build:
	cargo build --workspace

test:
	cargo test --workspace --no-fail-fast

lint:
	cargo clippy --workspace --all-targets -- -D warnings

fmt:
	cargo fmt --all

preflight:
	cargo fmt --all --check
	cargo clippy --workspace --all-targets -- -D warnings
	cargo test --workspace --no-fail-fast
