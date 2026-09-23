# Tickets

由 [spec.md](../spec.md)、[plan.md](../plan.md)、[porting-notes.md](../porting-notes.md) 拆出。每張 ticket 對一個新進的 agent 都要能獨立完成。

## 依賴層級

| 層 | Ticket | 依賴 |
|---|---|---|
| L0 | [T-00](T-00-skeleton.md) workspace 骨架 | — |
| L1 | [T-01](T-01-format.md) format · [T-02](T-02-stats.md) stats · [T-03](T-03-paths.md) paths · [T-04](T-04-filter-settings-fsutil.md) filter / settings / fsutil | T-00 |
| L2 | [T-05](T-05-restore.md) restore · [T-06](T-06-copy-files.md) 檔案模式 copy · [T-07](T-07-gitsrc.md) gitsrc · [T-09](T-09-clip.md) clip | T-01~T-04 |
| L3 | [T-08](T-08-commit-mode.md) commit 模式 · [T-15](T-15-git-payload.md) git 來源 payload 組裝 | T-05~T-07、T-09 |
| L3b | [T-10](T-10-cli.md) CLI | T-08、T-15 |
| L4 | [T-11](T-11-desktop-shell.md) Tauri 殼 · [T-13](T-13-ci-release.md) CI / release · [T-14](T-14-e2e.md) E2E | T-10 |
| L5 | [T-12](T-12-desktop-ui.md) 前端畫面 | T-11 |

## 狀態(2026-09-23)

T-00 ~ T-15 全部完成並合併到 `feat/snip-core`,本機(Linux)gate 全綠:fmt、clippy `-D warnings`、`cargo test --workspace --locked`、
前端 typecheck / oxlint / prettier / node test / build、actionlint。

**尚未驗證:** CI 實際執行、Windows 與 macOS(含 `#[cfg(unix)]` 以外的路徑與 symlink 行為)、T-13 的 tag 發版、Phase 0 兩台實機。

**已知且接受的小問題(未修):**
- T-12:「調整路徑」提示的範例路徑取自計畫順序,不是剪貼簿原始順序;按「調整路徑」會重新讀一次剪貼簿(執行前仍會顯示新計畫)。
- T-14:本機跑 `clipboard_roundtrip` 會把系統剪貼簿換成測試內容(Linux 上還原無效)。
- T-10:Linux 背景剪貼簿子程序若寫入失敗,`snip copy` 仍回傳 0。

## 所有 ticket 共通規則

- **TS 參考原始碼**在 repo 根目錄的 `.ts-ref/`(gitignored)。不存在時執行:
  `mkdir -p .ts-ref && git -C ../IntellijPlugin/ClipCodeVSCode archive 0aa24c8 src test scripts AGENTS.md | tar -x -C .ts-ref`
  **不要**讀 `../IntellijPlugin/ClipCodeVSCode` 的工作目錄,它可能落後。
- **只修改 ticket 列出的檔案。** 需要動其他檔案(包括 `Cargo.toml`、`lib.rs`)時,停下來在回報中說明,不要自己改。
- 程式碼註解用英文。不新增 ticket 沒列的依賴。
- 行為以 TS 版與 contract fixture 為準,**不要靠讀規格推測**;兩者不一致時以 fixture 為準並回報。
- porting-notes 第 1 節的 Rust 陷阱每張 ticket 都適用,ticket 內只點出最相關的幾項。
- 完成條件:ticket 的「驗收」指令全部綠燈,且 `cargo clippy -p <crate> --all-targets -- -D warnings` 無警告。
