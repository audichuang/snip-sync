# T-03 paths:路徑解析與 containment

## 目標
移植 `.ts-ref/src/pathResolver.ts` 到 `crates/core/src/paths.rs`。

## 範圍
- `source_root_name`、`to_clipboard_path`、`to_clipboard_path_from_roots`、`resolve_write_target`、`resolve_delete_target`、`escapes_all_roots`,以及 `RestoreTargetResolution`(成功帶絕對路徑;失敗帶原因,至少區分 missing / ambiguous / refused 與 TS 的其他 reason 字串)。
- 安全規則見 porting-notes 第 3 節:控制字元與 `<>:"|?*` 拒絕;未對到 root 的絕對路徑寫入時放在主 root 下、刪除時拒絕;containment 用 realpath(最深的已存在祖先),只限制 symlink 跳轉次數。
- `contract.rs` 的 `path_cases`:照 `.ts-ref/test/contract.test.ts` 的 `path:` 測試,用 `pathLayout` 在 `tempfile` 暫存目錄建出目錄、檔案、symlink,再逐列比對 `write` / `delete` 的結果。暫存目錄本身要先 canonicalize(macOS 的 `/var` → `/private/var`)。Windows 無法建 symlink 時只跳過 `needsSymlink` 的列並印出來。
- 參考測試:`.ts-ref/test/pathResolver.test.ts`,fixture 沒涵蓋的移植成單元測試。

## 最相關的陷阱
`dunce::canonicalize`(不要用 `std::fs::canonicalize`)、`std::path` 在 Windows 的磁碟機代號與分隔符號、字串比對不能取代路徑元件比對。

## 只可修改
`crates/core/src/paths.rs`、`crates/core/tests/contract.rs` 的 `path_cases` 函式。

## 驗收
`cargo test -p snip-core --test contract path_cases` 與 `cargo test -p snip-core --lib paths` 全綠。
