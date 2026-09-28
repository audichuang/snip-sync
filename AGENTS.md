# snip-sync

Behaviour is defined in `docs/spec.md` (what), `docs/plan.md` (how) and `docs/porting-notes.md` (Rust traps, accepted divergences).

## Parity with the IDE plugins

- File mode must stay byte-compatible with ClipCode / ClipCodeVSCode. The reference is the TS source at ClipCodeVSCode `0aa24c8`, extracted into the gitignored `.ts-ref/` (command in `docs/plan.md` section 2). Read the TS there, not the sibling checkout, which may be stale.
- When the TS source and a doc disagree, follow the TS and the contract fixture, then fix the doc.
- `fixtures/clipboard-contract.json` is a byte-exact copy owned by ClipCodeVSCode, and its SHA is pinned in `crates/core/tests/contract.rs`. Never edit or regenerate it here. To update it, copy it from ClipCodeVSCode and update the SHA in all three repos.
- A deliberate divergence from the TS goes into the "已知且接受的差異" list in `docs/porting-notes.md`. Without that entry, a later port "fixes" it back.

## Branches and releases

- `main` only holds released code; `develop` is the integration branch; every change gets its own `feature/<name>` (or `fix/<name>`) branch cut from `develop`, and its PR targets `develop`.
- To release, open a PR `develop` → `main`, merge it once green, then run `just release X.Y.Z` on `main` (it pushes the tag; `release.yml` builds, publishes and bumps the Homebrew tap). Release only when there is something worth shipping, not per merge.
- A pre-release to try a build skips `main`: on `develop`, `just bump X.Y.Z-beta.N`, commit, push, then `just release X.Y.Z-beta.N`. `release.yml` accepts the green push-to-develop CI run for a tag with a `-`, marks it pre-release and skips Homebrew.

## Before you call a change done

- Run `just preflight` before every push. It runs everything CI runs that Linux can run: actionlint, Rust fmt/clippy/doc/test, the Python harness tests, and the native smoke/lifecycle/acceptance gates (IME, 18 collaboration cases, resource runs). A Linux failure found by CI instead of locally is a process bug. CI adds audit, clean checkout, packaging and Windows/macOS; see `.github/workflows/ci.yml`.
- `native-acceptance` needs a Python with Pillow in `SNIP_NATIVE_PYTHON`, and it fails if the checkout changes after its build. Commit first, then leave the tree alone until it finishes.
- A new control that a test drives gets a `probe(...)` id, which the drivers read from `[APP:CTRL_BOUNDS]`.
- Real-app waits in `smoke.rs`/`lifecycle.rs` go through `scaled(...)`; on a loaded machine set `SNIP_E2E_TIMEOUT_SCALE` (CI uses 2) instead of raising a deadline. Under a memory-capped sandbox, a release build OOM-killed in `rustc` needs `CARGO_BUILD_JOBS`, not a retry.

## Cross-platform

- This machine is Linux, but CI runs Windows and macOS too. For path/fs code, reproduce the other platforms' conditions in a Linux test, e.g. a root spelled through a symlink to mimic macOS `/var` → `/private/var`. Both Windows and macOS broke on exactly this: git reports its resolved toplevel, and the user-given root did not match it.
- A test that skips when something is missing (display, node, `.ts-ref`) must `assert!(std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(), …)` before skipping. CI sets that variable, so a skipped test cannot pass as green.

## GitHub Actions

The repo is private. Any action that reads the GitHub API (PR files, merged PRs) needs that permission granted explicitly in the job's `permissions:`, for example `pull-requests: read`. This has already failed CI twice. aghub's workflows are public and never hit it.
