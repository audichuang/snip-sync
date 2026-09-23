# T-07 gitsrc:git plumbing

## 目標
`crates/core/src/gitsrc.rs`:以系統 git CLI 取得檔案模式所需的 git 內容。指令以 porting-notes 第 5 節為準。

## 範圍
- `Git` runner:`Command::new("git").args(..)`,不經 shell;Windows 加 `CREATE_NO_WINDOW`;`git --version` 檢查,找不到 git 時回傳明確錯誤。
- `--raw -z --no-abbrev -M` 輸出的**位元組層級** parser(狀態、舊 / 新 mode、舊 / 新 OID、路徑,rename 帶兩個路徑)。
- 長駐 `git cat-file --batch` 的讀取器:依 header size 讀精確位元組,missing 物件要能分辨。參考 `.ts-ref/src/catFile.ts`。
- 來源:working tree(含未追蹤檔)、staged、單一 commit(merge 取每個 parent 的聯集,保留 parent 與舊 OID 的對應;root commit 對空樹)、區間 `a..b`(端點比較)。
- 產出 `Vec<format::PayloadFile>`,規則照 porting-notes 第 4 節:刪除帶刪除前內容;沒有任何 parent 有這個檔案時才輸出刪除 marker;非 UTF-8 跳過並計數;shallow 邊界 commit 回報錯誤,不當 root commit。
- 參考:`.ts-ref/src/gitCopy.ts`、`graphCopy.ts`、`gitHistory.ts`、`gitContent.ts` 與對應測試。TS 版經由 VS Code git API 取資料,這層改為直接呼叫 git,**輸出的 PayloadFile 必須與 TS 版相同**。
- 測試用 `tempfile` 建真實 repo(一般 commit、刪除、rename、octopus merge、非 UTF-8 檔、檔名含空白與非 ASCII),設定 `user.name` / `user.email` 與 `core.autocrlf=false`。

## 只可修改
`crates/core/src/gitsrc.rs`。

## 驗收
`cargo test -p snip-core --lib gitsrc` 全綠。
