# T-13 CI 與 release

## 目標
比照 `../aghub/.github/workflows/ci.yml` 與 `release.yml`,依 plan.md 第 4、5 節裁剪。

## 範圍
- `ci.yml`:actionlint、format(rustfmt + prettier)、三平台 clippy `-D warnings`、前端 lint + test、三平台 `cargo test --workspace --no-fail-fast`;Linux 安裝 webkit2gtk / appindicator 與 xvfb。
- `release.yml`:tag 觸發 → verify-ci → git-cliff changelog → `tauri-action` 四個 target(mac arm / intel、linux、windows NSIS)→ CLI 四個 target + smoke test → 上傳。macOS ad-hoc 簽章 + `codesign --verify`。**不含** Homebrew 與 updater。
- `cliff.toml`、`justfile` 的 `bump` / `release`。

## 只可修改
`.github/workflows/*`、`cliff.toml`、`justfile`。

## 驗收
`actionlint` 通過;在 fork 上推一個測試 tag 能產出全部 artifact(由使用者觸發)。
