# T-10 CLI(`snip`)

## 目標
`crates/cli/src/main.rs`(可拆成 `crates/cli/src/*.rs`):實作 spec 第 5.2 節的全部指令,只呼叫 `snip-core`,不自己寫邏輯。

## 範圍
```
snip copy <路徑…> | --working | --staged | --commit <sha> | --range <a>..<b>
snip copy --commits -n <N> | --commits <a>..<b>
snip paste --dry-run
snip paste --apply [--overwrite | --skip-existing]
```
- `--repo <dir>`(預設目前目錄)、`--settings <json>`(預設 `Settings::default()`)。
- `--stdout` / `--stdin`:不經剪貼簿,方便測試與管線使用。
- 複製後印出與 IDE 通知相同的統計;貼上時自動判斷模式。
- `--dry-run` 列出每個檔案的動作與原因;commit 模式列出 commit 清單。
- 結束碼:成功 0、部分失敗 1、使用錯誤 2。

## 只可修改
`crates/cli/**`。

## 驗收
`cargo test -p snip-cli` 全綠(以 `assert_cmd` 風格的整合測試,透過 `--stdout` / `--stdin` 跑一次檔案模式與 commit 模式往返;若需要新增 dev-dependency,在回報中說明)。
