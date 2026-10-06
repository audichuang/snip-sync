# snip-sync

Behaviour is defined in `docs/spec.md` (what), `docs/plan.md` (how) and `docs/porting-notes.md` (Rust traps, accepted divergences).

## Parity with the IDE plugins

- File mode stays byte-compatible with ClipCode / ClipCodeVSCode. The reference is the TS at ClipCodeVSCode `0aa24c8`, extracted into the gitignored `.ts-ref/` (`docs/plan.md` section 2); the sibling checkout may be stale.
- When the TS and a doc disagree, follow the TS and the contract fixture, then fix the doc.
- `fixtures/clipboard-contract.json` is owned by ClipCodeVSCode, and its SHA is pinned in `crates/core/tests/contract.rs`. Never edit or regenerate it here: copy it over and update the SHA in all three repos.
- A deliberate divergence from the TS goes into "已知且接受的差異" in `docs/porting-notes.md`, or a later port "fixes" it back.

## One engine, two front ends

- The CLI and the desktop app are front ends over the same `snip-core` `transfer` functions and `snip_remote` stores (`docs/spec.md` §5.2 lists them). A copy, paste or pairing fix goes there, never into one surface: while the CLI ran its own engine, every safety fix reached only the GUI.
- `copy::collect_copy_files` and `gitsrc::collect_payload` survive only as byte-parity test oracles. Product code must not call them, and a bug fix does not go there.

## Branches and releases

- `main` holds released code only. Every change gets a `feature/<name>` or `fix/<name>` branch cut from `develop`, with its PR targeting `develop`, and is squash-merged.
- `develop` → `main` is a merge commit, never a squash: a squash leaves `main` with a commit `develop` lacks, and the next release PR conflicts.
- Release from `main` with `just release X.Y.Z` after the `develop` → `main` PR is green, and only when something is worth shipping. A pre-release skips `main`: on `develop`, `just bump X.Y.Z-beta.N`, commit, push, `just release X.Y.Z-beta.N`.

## Before you call a change done

- Before pushing a change that can alter a gate's result (code, scripts, CI config, Cargo files, fixtures), run `just preflight`. A failure CI finds that preflight would have caught is a process bug. On Linux it runs CI's Linux jobs. On macOS it runs CI's macOS checks, then the Linux jobs, native acceptance included, in an Apple `container` VM (`scripts/linux_container.sh`). Windows has no container path: run `just preflight-host`. Run it from a full clone (`git clone --no-local`), not a `git worktree`: the container mounts only the checkout, so native acceptance cannot reach a worktree's git dir, nor the main checkout's gitignored `.ts-ref/`; copy that into the clone, or the cross-tool tests fail. Set `SNIP_REQUIRE_ALL_TESTS=1` as CI does: preflight does not, so a test that skips on your machine passes preflight and fails CI.
- Each checkout's container target/ volume takes 12-25 GB. A passing preflight drops its own; a failed one keeps it for the rerun, and a deleted clone's stays until some checkout's next preflight. Deleting a clone or worktree is not cleanup: run `just clean` from a checkout that remains (it also clears that checkout's target/ and its worktrees'). Once only cargo's target/ was cleaned and 74 GB of volumes stayed.
- A push that cannot change any gate's result skips preflight: only `.md` files, or a `.gitignore` entry. CI still runs every job. A script under `docs/` is not Markdown; run the script itself.
- Remote workspaces have their own end-to-end gate, apart from the GUI gates: `scripts/remote_e2e.sh` has a real `snip remote` master start real `snip serve --stdio` workers. Preflight and CI run it on this machine (`just remote-e2e`), on every OS. A change to `crates/remote`, to the CLI's `serve` or `remote` commands, or to anything they call is not done until `just remote-e2e-ssh <host>` also passed against a second machine, where the worker is started over ssh.
- `native-acceptance` needs a Python with Pillow in `SNIP_NATIVE_PYTHON` and fails if the checkout changes after its build: commit first, then leave the tree alone.
- Where a test goes: pure logic → a unit test in `crates/core` or the native crate; UI state and interaction → `#[gpui::test]` in `crates/desktop-native/src/main.rs` `tests::in_process` (no display, all OSes); real input, clipboard or pixels → `crates/native-e2e/tests/smoke.rs` / `lifecycle.rs` (Xvfb; on macOS inside the preflight container); CLI behaviour and output → `crates/cli/tests/cli.rs`; byte round trips and TS compatibility → `crates/cli/tests/e2e.rs`; cross-machine file/commit semantics → a collaboration manifest step; remote connection behaviour → a check in `scripts/remote_e2e.sh`. macOS and Windows have no real-input GUI test.
- A `#[gpui::test]` about focus or keys after a UI flow drives that flow with the user's gesture (`simulate_click` on `debug_bounds`, `simulate_keystrokes`), not the model method it ends in: a clicked control takes the focus. A test that called `open_remote_path` passed while the first Cmd+V after opening from the menu did nothing.
- A control a test drives gets a `probe(...)` id; drivers read it from `[APP:CTRL_BOUNDS]`.
- Every file the desktop app persists resolves its folder through `recent::config_dir()`, which returns nothing under `cfg(test)` and in an e2e run without `SNIP_CONFIG_DIR`. A store that took another path let `cargo test` overwrite the user's real `recent-workspaces.json`.
- Real-app waits go through `snip_native_e2e::scaled(...)`, or `bench_native_memory.e2e_scaled(...)` in the Python harness. On a slow machine set `SNIP_E2E_TIMEOUT_SCALE` (CI uses 2) instead of raising a deadline or rerunning; when the gates share one machine, as in the macOS container, lower their parallelism with `SNIP_ACCEPTANCE_ARGS`.

## Cross-platform

- Path/fs code has broken on both macOS and Windows because git reports its resolved toplevel, which did not match the root the user gave. Test with the root spelled through a symlink (as macOS `/var` → `/private/var` is).
- A test that skips when something is missing (display, node, `.ts-ref`) must `assert!(std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(), …)` first. CI sets it, so a skip cannot pass as green.
- CI's macOS and Windows VMs are several times slower than a dev machine. Deadlines on helper processes (spawning `ps`, reaping a child) must survive that: a 500 ms `ps` check in `gitrun` failed clean git calls on CI and leaked their budget slot.
- A test that waits on another thread, channel or process, or loops until something converges, needs a bound that fails with a message. An unbounded `recv()` hung CI's macOS job for 45 minutes, and an unbounded load-more loop hung the Windows job for 36.
- Whatever only `cfg(unix)` tests use (a helper, an import) gets `cfg(unix)` too. Windows CI's `-D warnings` rejects it as unused, and no macOS or Linux gate sees that. Check it with `cargo clippy -p snip-core -p snip-remote -p snip-cli --all-targets --target x86_64-pc-windows-msvc -- -D warnings` (the desktop crate does not cross-build). One PR broke Windows CI twice this way.
- `read_dir` returns names sorted on APFS but in no fixed order on Linux, and repo discovery walks it unsorted. A test must not depend on which entry is reached first, or on which repos fit a scan budget.
- Count only what a test waits for, never what fires during a fixed sleep: three heartbeat intervals of sleep produced one heartbeat on CI's macOS VM. Poll for the expected count, with a bound.
- A test that reads process-global gitrun counters runs alone through `run_isolated` (desktop-native): other tests' git calls move the same counters.
- The tree's byte budget counts each node's `size_of`, which is more than twice as large on Windows: a remote listing that admits all 700 names on macOS admits about 266 there. A test about something other than the budget builds its state directly instead of relying on how many names fit.

## GitHub Actions

The repo is private: an action that reads the GitHub API (PR files, merged PRs) needs the permission in the job's `permissions:`, e.g. `pull-requests: read`. This has failed CI twice.
