# T-11 Tauri 殼

## 目標
建立 `crates/desktop`:比照 `../aghub/crates/desktop` 的結構(bun + Vite + React 19 + HeroUI v3 + Tailwind v4,`src-tauri` 為 Tauri 2),**拿掉** aghub 專屬的部分(api sidecar、remote、deep-link、inference、updater)。

## 範圍
- workspace 加入 `crates/desktop/src-tauri`(Cargo 套件名 `snip-sync`)。
- plugin:`single-instance`、`autostart`、`store`、`log`、`dialog`、`opener`、`positioner`;`tray-icon` feature。
- 系統匣選單:從剪貼簿貼上、複製上一次的選取、開啟主視窗、結束;關閉視窗縮回系統匣。
- Tauri command 薄殼:copy(檔案 / git 來源)、list_commits、copy_commits、read_clipboard_plan、apply_restore、replay_commits、diff(`similar`)。只呼叫 `snip-core`。
- `bun run generate:dto`:用 ts-rs 把 core 型別輸出到 `crates/desktop/src/generated/`。
- 前端只放一個能呼叫每個 command 的除錯頁;正式畫面在 T-12。

## 只可修改
`crates/desktop/**`、根目錄 `Cargo.toml` 的 `members`、`justfile`(加 `desktop`、`desktop-bundle`)。

## 驗收
`cargo build -p snip-sync`、`cd crates/desktop && bun install --frozen-lockfile && bun run typecheck && bun run build` 通過。
