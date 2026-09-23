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

# Run the desktop app in dev mode.
desktop:
	cd crates/desktop && bun install --frozen-lockfile && bun run start

# Build the desktop installers for this platform.
desktop-bundle:
	cd crates/desktop && bun install --frozen-lockfile && bun run tauri build
