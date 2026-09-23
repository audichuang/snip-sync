# T-05 restore:還原計畫與執行

## 目標
移植 `.ts-ref/src/restore.ts` 與 `.ts-ref/src/restoreBase.ts` 到 `crates/core/src/restore.rs`。

## 範圍
- `RestoreEntry`、`CreateOperation`、`DeleteOperation`、`SkippedOperation`(含原因)、`RestorePlan`、`RestoreExecutionResult`。
- `plan_restore(roots, entries) -> RestorePlan`、`execute_restore_plan(plan, selection) -> RestoreExecutionResult`(`selection` 讓使用者逐檔勾選)、`has_path_dependencies`。
- `RestoreBase`、`apply_restore_base`、`suggest_restore_base`。
- 規則:placeholder 只看內容第一行判斷且永遠不寫入;目標非 UTF-8 / unverifiable 不覆寫;所有寫入 UTF-8;**執行前重新檢查** containment 與編碼。
- 型別加 `#[derive(Serialize, TS)]`(前端會用到),`ts-rs` 輸出路徑先不設定。
- `contract.rs` 的 `restore_cases`:照 `.ts-ref/test/contract.test.ts` 的 restore 測試,在暫存目錄建出案例需要的佈局,比對 `creates` / `deletes` / `skips`。
- 參考測試:`.ts-ref/test/copyRestore.test.ts`、`restoreBase.test.ts`。

## 依賴
T-01(`parse_clipboard`)、T-03(`resolve_write_target` / `resolve_delete_target`)、T-04(`fsutil`)。

## 只可修改
`crates/core/src/restore.rs`、`crates/core/tests/contract.rs` 的 `restore_cases` 函式。

## 驗收
`cargo test -p snip-core --test contract restore_cases` 與 `cargo test -p snip-core --lib restore` 全綠。
