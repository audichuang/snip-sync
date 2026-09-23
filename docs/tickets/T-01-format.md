# T-01 format:payload 產生與解析

## 目標
移植 `.ts-ref/src/clipboardFormat.ts` 到 `crates/core/src/format.rs`,與 IDE 套件逐位元組相容。

## 範圍
- 型別:`ChangeType`(NEW / MODIFIED / DELETED / MOVED)、`PayloadFile`、`ParsedEntry`、`BuildPayloadOptions`。
- 函式:`ascii_trim`、`extract_source_root`、`format_header`、`build_payload`、`build_git_payload`、`parse_clipboard`、`extract_leading_labels`、`strip_leading_labels`。
- `// clipcode-root:`、`// clipcode-end`、`//clipcode-esc: ` 的規則(porting-notes 第 2 節)。
- 參考測試:`.ts-ref/test/clipboardFormat.test.ts`,把其中 fixture 沒涵蓋的案例移植成 `format.rs` 內的單元測試。

## 最相關的陷阱
regex 的 `.` 與 `\s`(一律寫明確的 ASCII 字元類別)、`ascii_trim` 必須含 `\x0B`、切行只用 `\n` 並去掉一個 `\r`、`$FILE_PATH` 是字面替換、header 比對只做 ASCII 大小寫。

## 只可修改
`crates/core/src/format.rs`、`crates/core/tests/contract.rs` 中的 `build_cases` 與 `parse_cases` 兩個函式(移除 `#[ignore]` 並實作)。

## 驗收
`cargo test -p snip-core --test contract build_cases parse_cases` 與 `cargo test -p snip-core --lib format` 全綠。
