# T-15 git 來源的 payload 組裝

## 目標
`gitsrc::collect` 只回傳原始檔案清單;補上 TS `collectGitPayloadFiles`(`.ts-ref/src/extension.ts` 約 541–655 行)與
`buildGraphCopyPayload`(`.ts-ref/src/graphCopy.ts`)那一層:從 git 來源產出與 IDE 套件相同的 payload 與統計。

## 範圍
- `gitsrc::collect_payload(git, source, workspace_roots, settings) -> copy::CopyResult`:
  - 依路徑去重,Windows 上以 TS `normalizeFsPath` 的規則(小寫)比對。
  - 套用 `settings` 的過濾規則(`filter::file_matches_filters`),路徑用 `paths::to_clipboard_path_from_roots`。
  - 檔案數上限(只有真正可複製的候選被丟掉時才設 `file_limit_reached`)、大小上限(放 `skipped_reason`)。
  - `UNREADABLE_FILE_MARKER` 仍放進 payload,但計入 `skipped_unreadable_count`、不算已複製、不佔數量上限。
  - 以 `format::build_git_payload` 組 payload;`usesRegularSpacing` / fallback 的判斷照 TS。單一 root 時帶 `sourceRoot`。
- **修正:** staged 模式讀到非 UTF-8 的 index blob 時,TS 是 `readRefContent(...) ?? UNREADABLE_FILE_MARKER`,
  也就是放 marker;目前 Rust 直接丟掉。改成與 TS 相同。
- 參考測試:`.ts-ref/test/gitCopy.test.ts`、`stagedGitCopy.test.ts`、`graphCopy.test.ts`,移植能在真實 repo 上重現的案例。

## 依賴
T-06(`CopyResult`)、T-07(`collect`)。

## 只可修改
`crates/core/src/gitsrc.rs`。

## 驗收
`cargo test -p snip-core --lib gitsrc` 全綠。
