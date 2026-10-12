# 貼上接縫放在 snip_remote，不是核心的 trait

CLI 與桌面 App 都只經 `snip_remote::PasteTarget` 規劃、重驗與寫入貼上；它是 enum（`Local`、`Remote`），不是像 `RepoView` 那樣放在核心的 trait。原因是遠端 Apply 失聯時結果未知（`PasteError::OutcomeUnknown`），核心的錯誤型別無法表達，而核心也不能依賴 `RemoteError`；遠端 adapter 需要 `Client` 與 `PasteMapping`，把 `PasteMapping` 搬進核心就得改 `proto.rs`。目標只有本機與 worker 兩種，enum 不需要 `dyn` 與 `Send + Sync`。寫入、鎖與檢查仍全在核心：本機目標只是轉呼叫 `plan_import_with`、`TransferImportPlan::apply` 與 `CommitReplayPreview`。

## Consequences

- worker 不經 `PasteTarget` 服務，而是直接呼叫同一個 `pastetarget::import_mapping`；`ImportMapping` 只在那裡建成。
- 兩種目標各自保留新鮮度檢查：本機套用保留的計畫並在寫入鎖內重驗，worker 重新規劃後比對 digest。
- 新增第三種目標（例如另一種傳輸）時才考慮改成 trait。
