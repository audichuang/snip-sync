default: build

build:
	cargo build --workspace

test:
	cargo test --workspace --no-fail-fast

lint:
	cargo clippy --workspace --all-targets -- -D warnings

fmt:
	cargo fmt --all

# Mirrors CI's Rust checks (see .github/workflows/ci.yml for the rest).
preflight:
	cargo fmt --all --check
	RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets --locked -- -D warnings
	RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked
	RUSTFLAGS="-D warnings" cargo test --workspace --locked --no-fail-fast

# Run the desktop app in dev mode.
desktop:
	cd crates/desktop && bun install --frozen-lockfile && bun run start

# Build the desktop installers for this platform.
desktop-bundle:
	cd crates/desktop && bun install --frozen-lockfile && bun run tauri build

# Real-app E2E (Linux): needs webkit2gtk-driver, `cargo install tauri-driver`, xvfb.
desktop-e2e:
	cd crates/desktop && bun install --frozen-lockfile && bun run tauri build --debug --no-bundle && xvfb-run -a node e2e/scenarios.mjs

# Bump the version in every manifest (perl -pi is portable across GNU/BSD).
bump version:
	perl -pi -e 's/^version = .*/version = "{{version}}"/' Cargo.toml
	perl -pi -e 's/"version": "[^"]*"/"version": "{{version}}"/' crates/desktop/package.json crates/desktop/src-tauri/tauri.conf.json

# Cut a release (green CI on HEAD -> tag -> watch release.yml -> verify assets); --yes skips the prompt.
release version *flags:
	#!/usr/bin/env bash
	set -euo pipefail
	REPO="audichuang/snip-sync"
	REMOTE="origin"
	VERSION="{{version}}"
	VERSION="${VERSION#v}"
	TAG="v$VERSION"
	err() { printf '\033[31m✗ %s\033[0m\n' "$*" >&2; exit 1; }
	ok() { printf '\033[32m✓ %s\033[0m\n' "$*"; }
	info() { printf '\033[36m• %s\033[0m\n' "$*"; }
	echo "$VERSION" | grep -Eq '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$' \
		|| err "version must be X.Y.Z with no leading zeros (got: $VERSION)"
	git remote get-url "$REMOTE" | grep -q "$REPO" || err "remote '$REMOTE' is not $REPO"
	[ "$(git rev-parse --abbrev-ref HEAD)" = "main" ] || err "not on main"
	{ git diff --quiet && git diff --cached --quiet; } || err "working tree is dirty"
	if git rev-parse -q --verify "refs/tags/$TAG" >/dev/null || git ls-remote --tags "$REMOTE" "$TAG" | grep -q "$TAG"; then
		err "tag $TAG already exists"
	fi
	# The build syncs the version from the tag, but a committed bump keeps local builds honest.
	grep -q "^version = \"$VERSION\"$" Cargo.toml || err "Cargo.toml is not at $VERSION; run 'just bump $VERSION' and commit"
	# Must be newer than the latest stable tag (pre-release tags sort above their stable).
	LATEST="$(git tag --list 'v*' --sort=-v:refname | sed '/-/d' | head -1)"
	if [ -n "$LATEST" ]; then
		HIGHEST="$(printf '%s\n%s\n' "${LATEST#v}" "$VERSION" | sort -V | tail -1)"
		{ [ "$VERSION" != "${LATEST#v}" ] && [ "$HIGHEST" = "$VERSION" ]; } || err "$TAG is not newer than $LATEST"
	fi
	ok "version $TAG validated (latest was ${LATEST:-none})"

	HEAD_SHA="$(git rev-parse HEAD)"
	git push "$REMOTE" main
	info "waiting for ci.yml (push to main) on $HEAD_SHA..."
	CI_OK=0
	# 40 min budget: the cold 3-OS matrix routinely takes 25+ min.
	for _ in $(seq 1 120); do
		RUN="$(gh run list --repo "$REPO" --workflow ci.yml --limit 30 \
			--json headSha,headBranch,event,status,conclusion \
			--jq "[.[] | select(.headSha==\"$HEAD_SHA\" and .headBranch==\"main\" and .event==\"push\")] | first" 2>/dev/null || echo "")"
		if [ -n "$RUN" ] && [ "$RUN" != "null" ] && [ "$(echo "$RUN" | jq -r .status)" = "completed" ]; then
			[ "$(echo "$RUN" | jq -r .conclusion)" = "success" ] || err "ci.yml for HEAD did not succeed"
			CI_OK=1
			break
		fi
		sleep 20
	done
	[ "$CI_OK" = "1" ] || err "timed out waiting for ci.yml"
	ok "ci.yml is green for HEAD"

	git log --oneline "${LATEST:+$LATEST..}HEAD"
	if ! printf '%s\n' {{flags}} | grep -qx -- --yes; then
		[ -t 0 ] || err "non-interactive shell; re-run with --yes"
		printf "Tag and release %s? [y/N] " "$TAG"
		read -r ANS
		case "$ANS" in y | Y | yes | YES) ;; *) err "aborted" ;; esac
	fi

	# Runs created before the tag push have smaller ids, so a stale run is never picked.
	PRIOR_RUN_ID="$(gh run list --repo "$REPO" --workflow release.yml --limit 1 --json databaseId --jq '.[0].databaseId // 0' 2>/dev/null || echo 0)"
	git tag "$TAG"
	git push "$REMOTE" "$TAG"
	ok "pushed $TAG; release.yml triggered"
	RUN_ID=""
	for _ in $(seq 1 20); do
		RUN_ID="$(gh run list --repo "$REPO" --workflow release.yml --limit 15 --json databaseId,headSha,event \
			--jq "[.[] | select(.headSha==\"$HEAD_SHA\" and .event==\"push\" and .databaseId > $PRIOR_RUN_ID)] | sort_by(.databaseId) | last | .databaseId" 2>/dev/null || echo "")"
		[ -n "$RUN_ID" ] && [ "$RUN_ID" != "null" ] && break
		sleep 6
	done
	{ [ -n "$RUN_ID" ] && [ "$RUN_ID" != "null" ]; } || err "could not find the release run for $TAG"
	info "release run $RUN_ID; watching..."
	# `gh run watch` can exit non-zero on an API hiccup; only the run's own conclusion counts.
	gh run watch "$RUN_ID" --repo "$REPO" >/dev/null 2>&1 || true
	until [ "$(gh run view "$RUN_ID" --repo "$REPO" --json status --jq .status)" = "completed" ]; do sleep 15; done
	[ "$(gh run view "$RUN_ID" --repo "$REPO" --json conclusion --jq .conclusion)" = "success" ] \
		|| err "release run failed: gh run view $RUN_ID --repo $REPO --log-failed"
	ok "release.yml succeeded"

	ASSETS="$(gh release view "$TAG" --repo "$REPO" --json assets --jq '.assets[].name')"
	echo "$ASSETS" | sed 's/^/    /'
	for must in snip-sync_mac_arm.dmg snip-sync_mac_intel.dmg snip-sync-linux.AppImage snip-sync-windows-setup.exe \
		snip-aarch64-apple-darwin.tar.gz snip-x86_64-apple-darwin.tar.gz snip-x86_64-unknown-linux-gnu.tar.gz snip-x86_64-pc-windows-msvc.zip; do
		echo "$ASSETS" | grep -qx -- "$must" || err "missing asset $must"
	done
	ok "Release $TAG complete."
