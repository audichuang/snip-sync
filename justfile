default: build

native_python := env("SNIP_NATIVE_PYTHON", "python3")

build:
	cargo build --workspace

test:
	cargo test --workspace --no-fail-fast

lint:
	cargo clippy --workspace --all-targets -- -D warnings

fmt:
	cargo fmt --all

# Frees build output: this checkout's target/, its worktrees' target/, and on macOS the
# preflight container's target/ volumes of this checkout (kept by a failed preflight) and
# of every deleted clone or worktree (12-25 GB each, otherwise kept until the next preflight).
clean:
	#!/usr/bin/env bash
	set -euo pipefail
	cargo clean
	git worktree list --porcelain | sed -n 's/^worktree //p' | tail -n +2 | while IFS= read -r w; do
		if [ -d "$w/target" ]; then rm -rf "$w/target" && echo "removed $w/target"; fi
	done
	if command -v container >/dev/null 2>&1 && container system status >/dev/null 2>&1; then
		scripts/linux_container.sh --prune-all
	fi

# On Linux this is preflight-linux. Elsewhere this OS's checks run on the host, then the
# Linux jobs run in a container (scripts/linux_container.sh), native acceptance included.
# Everything CI runs that a machine can run (see .github/workflows/ci.yml for the rest).
preflight:
	#!/usr/bin/env bash
	set -euo pipefail
	if [ "$(uname -s)" = Linux ]; then
		exec {{just_executable()}} native_python="{{native_python}}" preflight-linux
	fi
	{{just_executable()}} preflight-host
	scripts/linux_container.sh just preflight-linux
	# A pass drops this checkout's target/ volume (12-25 GB): each task uses a fresh
	# clone, so it would only serve a rerun. A failure keeps it for an incremental rerun.
	scripts/linux_container.sh --prune-all

# The Python harness tests are light, so they overlap the Rust checks; their output
# is held back and printed after, so a failure is not buried in cargo's.
# CI's Linux jobs, run directly (Linux) or through scripts/linux_container.sh.
preflight-linux: preflight-workflows
	#!/usr/bin/env bash
	set -uo pipefail
	log="$(mktemp)"
	trap 'rm -f "$log"' EXIT
	{{just_executable()}} native_python="{{native_python}}" preflight-harness >"$log" 2>&1 &
	harness=$!
	rc=0
	{{just_executable()}} preflight-rust || rc=$?
	wait "$harness" || { rc=$?; cat "$log"; }
	[ "$rc" -eq 0 ] || exit "$rc"
	{{just_executable()}} remote-e2e
	# Extra acceptance flags, e.g. less parallelism where the machine is small.
	{{just_executable()}} native_python="{{native_python}}" native-acceptance ${SNIP_ACCEPTANCE_ARGS:-}

# Same as CI's Lint Workflows job; needs actionlint and shellcheck on PATH.
preflight-workflows:
	@# actionlint silently skips the run: scripts without shellcheck; CI has it preinstalled.
	@command -v shellcheck >/dev/null || { echo "shellcheck not on PATH: actionlint would skip the run: scripts CI lints (pip install shellcheck-py)" >&2; exit 1; }
	actionlint
	@# The scripts CI's Remote E2E and container jobs lint on their own.
	shellcheck scripts/remote_e2e.sh scripts/linux_container.sh scripts/linux-container/entrypoint.sh scripts/check_test_refresh.sh

preflight-rust:
	cargo fmt --all --check
	scripts/check_test_refresh.sh
	RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets --locked -- -D warnings
	RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked
	# Same as CI's Linux Test job: one run, clipboard tests on a private display.
	RUSTFLAGS="-D warnings" xvfb-run -a cargo nextest run --workspace --exclude snip-native-e2e --locked --no-fail-fast
	RUSTFLAGS="-D warnings" cargo test --doc --workspace --exclude snip-native-e2e --locked

# CI's Lint and Test jobs on macOS and Windows: no Xvfb, the host's own clipboard.
preflight-host:
	cargo fmt --all --check
	scripts/check_test_refresh.sh
	RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets --locked -- -D warnings
	RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked
	RUSTFLAGS="-D warnings" cargo nextest run --workspace --exclude snip-native-e2e --locked --no-fail-fast
	RUSTFLAGS="-D warnings" cargo test --doc --workspace --exclude snip-native-e2e --locked
	{{just_executable()}} remote-e2e

# Remote workspaces end to end, apart from the GUI gates: a real `snip remote` master
# starting real `snip serve --stdio` workers on this machine (scripts/remote_e2e.sh).
remote-e2e:
	#!/usr/bin/env bash
	set -euo pipefail
	cargo build -p snip-cli --locked
	snip=target/debug/snip
	[ -x "$snip" ] || snip=$snip.exe
	scripts/remote_e2e.sh --snip "$snip"

# The same checks with the worker on another machine, started over ssh as a master
# does: the worker is built there from `git archive HEAD`. Extra args go to the
# script (--remote-snip PATH).
remote-e2e-ssh host *args:
	#!/usr/bin/env bash
	set -euo pipefail
	cargo build --release -p snip-cli --locked
	snip=target/release/snip
	[ -x "$snip" ] || snip=$snip.exe
	scripts/remote_e2e.sh --snip "$snip" --worker-ssh {{host}} {{args}}

# Python stdlib memory harness contracts and workload generator tests.
preflight-harness:
	SNIP_REQUIRE_ALL_TESTS=1 "{{ native_python }}" -B -m unittest discover -s scripts/tests

# Run the native GPUI desktop prototype.
native *args:
	cargo run -p snip-desktop-native -- {{args}}

# Native real-app smoke test (Linux X11) against a debug build, for iterating on
# one driver: needs xvfb, xdotool, x11-apps, imagemagick, xkbcommon, fonts,
# software graphics. `just native-acceptance` runs it on the release build.
native-smoke out="target/native-e2e-artifacts":
	cargo build -p snip-desktop-native --locked
	mkdir -p "{{out}}"
	rm -f "{{out}}/graph.png" "{{out}}/file_tree.png" "{{out}}/paste_preview.png" "{{out}}/light_theme.png"
	bash -o pipefail -c 'SNIP_REQUIRE_ALL_TESTS=1 SNIP_NATIVE_BIN="$(realpath target/debug/snip-desktop-native)" SNIP_E2E_OUT="$(realpath "$1")" ./scripts/headless-x11.sh cargo test -p snip-native-e2e --test smoke --locked -- --nocapture 2>&1 | tee "$1/smoke.log"' _ "{{out}}"
	test -s "{{out}}/smoke.log"
	python3 -c "import sys, pathlib; out = pathlib.Path(sys.argv[1]); [sys.exit(f'Missing or invalid {name}') for name in ('graph.png', 'file_tree.png', 'paste_preview.png', 'light_theme.png') if not (p := out / name).is_file() or p.stat().st_size == 0 or p.read_bytes()[:8] != b'\x89PNG\r\n\x1a\n']" "{{out}}"

# Real X11 close/reopen/quit drain and copy/paste cancel checks (tests/lifecycle.rs).
native-lifecycle out="target/native-e2e-artifacts":
	cargo build -p snip-desktop-native --locked
	mkdir -p "{{out}}"
	bash -o pipefail -c 'SNIP_REQUIRE_ALL_TESTS=1 SNIP_NATIVE_BIN="$(realpath target/debug/snip-desktop-native)" SNIP_E2E_OUT="$(realpath "$1")" ./scripts/headless-x11.sh cargo test -p snip-native-e2e --test lifecycle --locked -- --nocapture 2>&1 | tee "$1/lifecycle.log"' _ "{{out}}"
	test -s "{{out}}/lifecycle.log"

# Current release build once, then private IME9+startup, collaboration18,
# functional short resource gate (20 warmup +100 measured switches), and the
# smoke/lifecycle drivers on that same binary, all five gates at once. Collaboration
# runs 3 steps at a time and each driver is split over 2 Xvfb processes; 6 at once
# starved apps of CPU here (startup and log-line timeouts), so raise these with care.
# Optional args include --output FRESH_DIR and --build-receipt EXISTING_RECEIPT.
native-acceptance *args:
    "{{ native_python }}" -B scripts/run_native_acceptance.py --gate all --jobs 5 --collaboration-jobs 3 --driver-shards 2 {{ args }}

native-acceptance-build *args:
    "{{ native_python }}" -B scripts/run_native_acceptance.py --gate build {{ args }}

native-ime *args:
    "{{ native_python }}" -B scripts/run_native_acceptance.py --gate ime {{ args }}

native-collaboration *args:
    "{{ native_python }}" -B scripts/run_native_acceptance.py --gate collaboration {{ args }}

native-resources-short *args:
    "{{ native_python }}" -B scripts/run_native_acceptance.py --gate resource-short {{ args }}

# Full/release resource acceptance: standard workload, original long gate.
# Currently FAILS for missing real hide/tray coverage; never a substitute for D4.
native-resources-long *args:
    "{{ native_python }}" -B scripts/run_native_acceptance.py --gate resource-long {{ args }}

# Package the native desktop app (the release asset layout; see docs/native-cross-platform-ci-and-packaging.md).
package-native target out bin version:
	./scripts/package_native.sh "{{target}}" "{{out}}" "{{bin}}" "{{version}}"

# Verify build/package artifacts in target directory.
verify-artifacts dir version="" target="":
	#!/usr/bin/env bash
	set -euo pipefail
	CMD=(python3 scripts/verify_artifacts.py --dir "{{dir}}")
	[ -n "{{version}}" ] && CMD+=(--version "{{version}}")
	[ -n "{{target}}" ] && CMD+=(--target "{{target}}")
	"${CMD[@]}"

# Run strict native CLI smoke check on native binary.
native-cli-smoke bin version:
	python3 scripts/smoke_native.py --bin "{{bin}}" --expected-version "{{version}}"

# Bump the workspace version (perl -pi is portable across GNU/BSD).
bump version:
	perl -pi -e 's/^version = .*/version = "{{version}}"/' Cargo.toml
	# Keep Cargo.lock in step, or every --locked build fails.
	cargo update -w

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
	echo "$VERSION" | grep -Eq '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z.]+)?$' \
		|| err "version must be X.Y.Z or X.Y.Z-pre (e.g. 0.4.0-beta.1) with no leading zeros (got: $VERSION)"
	# A pre-release ships from develop without the develop -> main round trip;
	# release.yml accepts its green push-to-develop CI run. Stable stays on main.
	case "$VERSION" in *-*) BRANCH=develop ;; *) BRANCH=main ;; esac
	git remote get-url "$REMOTE" | grep -q "$REPO" || err "remote '$REMOTE' is not $REPO"
	[ "$(git rev-parse --abbrev-ref HEAD)" = "$BRANCH" ] || err "$TAG releases from $BRANCH; not on $BRANCH"
	{ git diff --quiet && git diff --cached --quiet; } || err "working tree is dirty"
	if git rev-parse -q --verify "refs/tags/$TAG" >/dev/null || git ls-remote --tags "$REMOTE" "$TAG" | grep -q "$TAG"; then
		err "tag $TAG already exists"
	fi
	# The desktop assets are the CI-accepted packages of this SHA and carry its
	# Cargo.toml version (release.yml never rebuilds them), so the bump must be committed.
	grep -q "^version = \"$VERSION\"$" Cargo.toml || err "Cargo.toml is not at $VERSION; run 'just bump $VERSION' and commit"
	# Must be newer than the latest stable tag (pre-release tags sort above their stable).
	LATEST="$(git tag --list 'v*' --sort=-v:refname | sed '/-/d' | head -1)"
	if [ -n "$LATEST" ]; then
		HIGHEST="$(printf '%s\n%s\n' "${LATEST#v}" "$VERSION" | sort -V | tail -1)"
		{ [ "$VERSION" != "${LATEST#v}" ] && [ "$HIGHEST" = "$VERSION" ]; } || err "$TAG is not newer than $LATEST"
	fi
	ok "version $TAG validated (latest was ${LATEST:-none})"

	HEAD_SHA="$(git rev-parse HEAD)"
	git push "$REMOTE" "$BRANCH"
	info "waiting for ci.yml (push to $BRANCH) on $HEAD_SHA..."
	CI_OK=0
	# 150 min budget: native-acceptance alone may take up to its 120 min timeout.
	for _ in $(seq 1 450); do
		RUN="$(gh run list --repo "$REPO" --workflow ci.yml --limit 30 \
			--json headSha,headBranch,event,status,conclusion \
			--jq "[.[] | select(.headSha==\"$HEAD_SHA\" and .headBranch==\"$BRANCH\" and .event==\"push\")] | first" 2>/dev/null || echo "")"
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
	for must in snip-sync_mac_arm.dmg snip-sync_mac_arm.app.tar.gz snip-sync_mac_intel.dmg snip-sync_mac_intel.app.tar.gz \
		snip-sync-windows-setup.exe snip-sync-windows-x64.zip snip-sync-linux-x86_64.tar.gz snip-sync-desktop-SHA256SUMS.txt \
		snip-aarch64-apple-darwin.tar.gz snip-x86_64-apple-darwin.tar.gz snip-x86_64-unknown-linux-gnu.tar.gz snip-x86_64-pc-windows-msvc.zip; do
		echo "$ASSETS" | grep -qx -- "$must" || err "missing asset $must"
	done
	ok "Release $TAG complete."
