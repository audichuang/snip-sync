# T-04 filter / settings / fsutil

## 目標
移植三個小模組:
- `.ts-ref/src/settings.ts` → `crates/core/src/settings.rs`:`FilterRule`、`Settings`(對應 `ClipCodeSettings`,serde,欄位名與 TS 相同以便讀寫 JSON 設定)、`Settings::default()`、`normalize`。
- `.ts-ref/src/filterMatcher.ts` → `crates/core/src/filter.rs`:`file_matches_filters`、`matches_path`、`directory_excluded`、`overlaps_directory`。
- `.ts-ref/src/fileSystem.ts` → `crates/core/src/fsutil.rs`:`decode_utf8_or_skip`、`TargetEncoding`(absent / utf8 / other / unverifiable)、`target_encoding`、`must_not_overwrite`、`list_files_recursive`(目錄 symlink 只在本身是輸入時才跟進)、`read_text_file`、`write_text_file`、`delete_file`。全部同步 API,不用 async。

## 最相關的陷阱
嚴格 UTF-8(`String::from_utf8`,保留 BOM;不要 `from_utf8_lossy`)、UTF-16 含 BOM 也要被判為非 UTF-8、大於 8 MiB 或讀不到視為 unverifiable、pattern 比對的 regex 字元類別。

## 參考測試
`.ts-ref/test/settings.test.ts`、`.ts-ref/test/filterMatcher.test.ts`,以及 `copyRestore.test.ts` 裡與 fileSystem 相關的案例,移植成各模組的單元測試。

## 只可修改
`crates/core/src/settings.rs`、`crates/core/src/filter.rs`、`crates/core/src/fsutil.rs`。

## 驗收
`cargo test -p snip-core --lib settings filter fsutil` 全綠。
