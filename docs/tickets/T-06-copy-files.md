# T-06 檔案模式 copy(磁碟來源)

## 目標
移植 `.ts-ref/src/copy.ts` 的 `collectCopyFiles` / `collectCopyTextFiles` 與 payload 組裝到 `crates/core/src/copy.rs`:給一組路徑與 `Settings`,產出 payload 字串與 `CopyResult`(已複製數、跳過的非 UTF-8 數、placeholder 數等,欄位照 TS)。

## 範圍
- 遞迴走訪(`fsutil::list_files_recursive`)、過濾(`filter`)、大小與數量上限、非 UTF-8 跳過並計數、讀不到放 placeholder 且不算已複製、不佔數量上限。
- 路徑轉換用 `paths::to_clipboard_path_from_roots`;單一 root 時輸出 `// clipcode-root:`。
- 統計用 `stats::payload_stats`(從整份 payload 算)。
- 輸入路徑要先正規化(去掉 `.` / `..`,等同 TS 的 `path.join` / `path.resolve`):`fsutil::list_files_recursive` 以 `PathBuf::join` 組子路徑、不會正規化,`./src` 會得到 `./src/a.ts`。
- `fsutil::read_text_file` 回傳 `io::Result<Option<String>>`:`Err` = 讀不到(放 placeholder),`Ok(None)` = 非 UTF-8(跳過並計數)。
- 參考測試:`.ts-ref/test/copyRestore.test.ts` 的 copy 部分。

## 依賴
T-01、T-02、T-03、T-04。

## 只可修改
`crates/core/src/copy.rs`。

## 驗收
`cargo test -p snip-core --lib copy` 全綠,且包含「copy → `restore::plan_restore` 到另一個空目錄 → 檔案樹相同」的往返測試(T-05 尚未合併時先標 `#[ignore]` 並回報)。
