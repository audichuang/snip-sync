# snip-sync

Behaviour is defined in `docs/spec.md` (what), `docs/plan.md` (how) and `docs/porting-notes.md` (Rust traps, accepted divergences).

## Parity with the IDE plugins

- File mode stays byte-compatible with ClipCode / ClipCodeVSCode. The reference is the TS at ClipCodeVSCode `0aa24c8`, extracted into the gitignored `.ts-ref/` (`docs/plan.md` section 2); the sibling checkout may be stale.
- When the TS and a doc disagree, follow the TS and the contract fixture, then fix the doc.
- `fixtures/clipboard-contract.json` is owned by ClipCodeVSCode, and its SHA is pinned in `crates/core/tests/contract.rs`. Never edit or regenerate it here: copy it over and update the SHA in all three repos.
- A deliberate divergence from the TS goes into "已知且接受的差異" in `docs/porting-notes.md`, or a later port "fixes" it back.

## Branches and releases

- `main` holds released code only. Every change gets a `feature/<name>` or `fix/<name>` branch cut from `develop`, with its PR targeting `develop`, and is squash-merged.
- `develop` → `main` is a merge commit, never a squash: a squash leaves `main` with a commit `develop` lacks, and the next release PR conflicts.
- Release from `main` with `just release X.Y.Z` after the `develop` → `main` PR is green, and only when something is worth shipping. A pre-release skips `main`: on `develop`, `just bump X.Y.Z-beta.N`, commit, push, `just release X.Y.Z-beta.N`.

## Before you call a change done

- Before pushing anything besides Markdown, run `just preflight`, which runs everything CI's Linux jobs run. A failure CI finds that preflight would have caught is a process bug. It needs Xvfb; on macOS or Windows run `cargo fmt --all --check`, clippy and rustdoc with `-D warnings`, and `cargo test --workspace --exclude snip-native-e2e --locked`, as CI's jobs for that OS do.
- A push of only `.md` files skips preflight: nothing in it reads Markdown, and CI still runs every job. A script under `docs/` is not Markdown; run the script itself.
- `native-acceptance` needs a Python with Pillow in `SNIP_NATIVE_PYTHON` and fails if the checkout changes after its build: commit first, then leave the tree alone.
- Where a test goes: pure logic → a unit test in `crates/core` or the native crate; UI state and interaction → `#[gpui::test]` in `crates/desktop-native/src/main.rs` `tests::in_process` (no display, all OSes); real input, clipboard or pixels → `crates/native-e2e/tests/smoke.rs` / `lifecycle.rs` (Xvfb, Linux only); cross-machine file/commit semantics → a collaboration manifest step. macOS and Windows have no real-input GUI test.
- A control a test drives gets a `probe(...)` id; drivers read it from `[APP:CTRL_BOUNDS]`.
- Real-app waits in native-e2e go through `snip_native_e2e::scaled(...)`. On a slow machine set `SNIP_E2E_TIMEOUT_SCALE` (CI uses 2) instead of raising a deadline.

## Cross-platform

- Path/fs code has broken on both macOS and Windows because git reports its resolved toplevel, which did not match the root the user gave. Test with the root spelled through a symlink (as macOS `/var` → `/private/var` is).
- A test that skips when something is missing (display, node, `.ts-ref`) must `assert!(std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(), …)` first. CI sets it, so a skip cannot pass as green.
- CI's macOS and Windows VMs are several times slower than a dev machine. Deadlines on helper processes (spawning `ps`, reaping a child) must survive that: a 500 ms `ps` check in `gitrun` failed clean git calls on CI and leaked their budget slot.
- A test that waits on another thread, channel or process needs a timeout that fails with a message. An unbounded `recv()` hung CI's macOS job for 45 minutes.

## GitHub Actions

The repo is private: an action that reads the GitHub API (PR files, merged PRs) needs the permission in the job's `permissions:`, e.g. `pull-requests: read`. This has failed CI twice.
