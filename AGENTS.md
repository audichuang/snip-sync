# snip-sync

Behaviour is defined in `docs/spec.md` (what), `docs/plan.md` (how) and `docs/porting-notes.md` (Rust traps, accepted divergences).

## Parity with the IDE plugins

- File mode must stay byte-compatible with ClipCode / ClipCodeVSCode. The reference is the TS source at ClipCodeVSCode `0aa24c8`, extracted into the gitignored `.ts-ref/` (command in `docs/plan.md` section 2). Read the TS there, not the sibling checkout, which may be stale.
- When the TS source and a doc disagree, follow the TS and the contract fixture, then fix the doc.
- `fixtures/clipboard-contract.json` is a byte-exact copy owned by ClipCodeVSCode, and its SHA is pinned in `crates/core/tests/contract.rs`. Never edit or regenerate it here. To update it, copy it from ClipCodeVSCode and update the SHA in all three repos.
- A deliberate divergence from the TS goes into the "已知且接受的差異" list in `docs/porting-notes.md`. Without that entry, a later port "fixes" it back.

## Before you call a change done

- Rust: `just preflight`. CI is stricter (`-D warnings` incl. rustdoc, `--locked`, audit, DTO drift, clean checkout); see `.github/workflows/ci.yml`.
- Frontend (`crates/desktop`): `bun run typecheck && bun run lint:check && bun run format:check && bun run test`.
- UI or command changes: `just desktop-e2e` (real app, every scenario in `crates/desktop/e2e/scenarios.mjs`). New UI controls the scenarios use get a `data-testid`.
- After changing any `#[derive(TS)]` type, run `bun run generate:dto && bun run format` in `crates/desktop` and commit `src/generated/`.

## Cross-platform

- This machine is Linux, but CI runs Windows and macOS too. For path/fs code, reproduce the other platforms' conditions in a Linux test, e.g. a root spelled through a symlink to mimic macOS `/var` → `/private/var`. Both Windows and macOS broke on exactly this: git reports its resolved toplevel, and the user-given root did not match it.
- A test that skips when something is missing (display, node, `.ts-ref`) must `assert!(std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(), …)` before skipping. CI sets that variable, so a skipped test cannot pass as green.

## GitHub Actions

The repo is private. Any action that reads the GitHub API (PR files, merged PRs) needs that permission granted explicitly in the job's `permissions:`, for example `pull-requests: read`. This has already failed CI twice. aghub's workflows are public and never hit it.
