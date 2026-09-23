# T-12 前端畫面

## 目標
spec 第 5.1 節的主視窗與預覽畫面。

## 範圍
- 選 repo / 資料夾;檔案模式選來源後複製;commit 模式以 `@tomplum/react-git-log`(HTML Grid)顯示時間軸,點起點 + Shift 點終點選取連續範圍。
- 貼上預覽:`@rc-component/tree` 逐檔勾選,跳過項禁勾並顯示原因;覆寫檔以 `@pierre/diffs` 顯示 diff;commit 模式列 commit 清單並可展開檔案與 diff、標出未複製檔案。
- 結果畫面;i18next 繁中 / 英文。
- HeroUI v3 請先讀 `../aghub/crates/desktop/AGENTS.md` 的 HeroUI 注意事項,不要憑記憶寫元件。
- 純邏輯放 `src/lib/` 並以 `node --test` 測試(比照 aghub)。

## 只可修改
`crates/desktop/src/**`、`crates/desktop/package.json`、`bun.lock`。

## 驗收
`bun run typecheck`、`bun run lint:check`、`bun run test`、`bun run build` 通過;附上主要畫面的截圖(`tauri dev` 或 Vite dev server 搭配 mock command)。
