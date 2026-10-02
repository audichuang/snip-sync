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

- Before pushing a change that can alter a gate's result (code, scripts, CI config, Cargo files, fixtures), run `just preflight`. A failure CI finds that preflight would have caught is a process bug. On Linux it runs CI's Linux jobs. On macOS it runs CI's macOS checks, then the Linux jobs, native acceptance included, in an Apple `container` VM (`scripts/linux_container.sh`; each run drops the target/ volumes of checkouts that no longer exist, `--clean` drops its volumes, images and kernel cache). Windows has no container path: run `just preflight-host`.
- A push that cannot change any gate's result skips preflight: only `.md` files, or a `.gitignore` entry. CI still runs every job. A script under `docs/` is not Markdown; run the script itself.
- Remote-node connectivity is its own end-to-end gate, apart from the GUI gates: `scripts/remote_e2e.sh` runs a real `snip worker` process and a real `snip remote` master over TLS. Preflight and CI run it on 127.0.0.1 (`just remote-e2e`), on every OS. A change to `crates/remote`, to the CLI's `worker` or `remote` commands, or to anything they call is not done until `just remote-e2e-ssh <host>` also passed against a second machine over Tailscale.
- `native-acceptance` needs a Python with Pillow in `SNIP_NATIVE_PYTHON` and fails if the checkout changes after its build: commit first, then leave the tree alone.
- Where a test goes: pure logic → a unit test in `crates/core` or the native crate; UI state and interaction → `#[gpui::test]` in `crates/desktop-native/src/main.rs` `tests::in_process` (no display, all OSes); real input, clipboard or pixels → `crates/native-e2e/tests/smoke.rs` / `lifecycle.rs` (Xvfb; on macOS inside the preflight container); CLI behaviour and output → `crates/cli/tests/cli.rs`; byte round trips and TS compatibility → `crates/cli/tests/e2e.rs`; cross-machine file/commit semantics → a collaboration manifest step; remote-node connection behaviour → a check in `scripts/remote_e2e.sh`. macOS and Windows have no real-input GUI test.
- A control a test drives gets a `probe(...)` id; drivers read it from `[APP:CTRL_BOUNDS]`.
- Real-app waits go through `snip_native_e2e::scaled(...)`, or `bench_native_memory.e2e_scaled(...)` in the Python harness. On a slow machine set `SNIP_E2E_TIMEOUT_SCALE` (CI uses 2) instead of raising a deadline or rerunning; when the gates share one machine, as in the macOS container, lower their parallelism with `SNIP_ACCEPTANCE_ARGS`.

## Cross-platform

- Path/fs code has broken on both macOS and Windows because git reports its resolved toplevel, which did not match the root the user gave. Test with the root spelled through a symlink (as macOS `/var` → `/private/var` is).
- A test that skips when something is missing (display, node, `.ts-ref`) must `assert!(std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(), …)` first. CI sets it, so a skip cannot pass as green.
- CI's macOS and Windows VMs are several times slower than a dev machine. Deadlines on helper processes (spawning `ps`, reaping a child) must survive that: a 500 ms `ps` check in `gitrun` failed clean git calls on CI and leaked their budget slot.
- A test that waits on another thread, channel or process needs a timeout that fails with a message. An unbounded `recv()` hung CI's macOS job for 45 minutes.

## GitHub Actions

The repo is private: an action that reads the GitHub API (PR files, merged PRs) needs the permission in the job's `permissions:`, e.g. `pull-requests: read`. This has failed CI twice.
