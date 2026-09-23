# T-08 commit 模式

## 目標
`crates/core/src/commits.rs`:實作 spec 第 4 節的 commit 模式(複製與貼上)。

## 範圍
- **選取與檢查:** `select_range(repo, base, tip)`、`select_last(repo, n)`;沿 first parent 驗證連續,不連續時回傳錯誤並指出斷點。
- **複製:** 每個 commit 取 message、作者名稱 / email、作者時間(含時區)、對 first parent 的異動(新增 / 修改 / 刪除 / rename 帶舊路徑)與異動後完整內容。非 UTF-8 / 二進位:列入清單、標記未複製與原因、不帶內容。序列化成第一行 marker `// snip-sync commits v1` + JSON(serde)。
- **偵測:** `is_commit_payload(text)`。
- **預覽:** `plan_commit_replay(repo, payload)` 回傳每個 commit 的檔案動作(沿用 `paths` 的路徑安全規則),型別加 `#[derive(Serialize, TS)]`。
- **貼上:** `replay(repo, payload)`:依序寫入 / 刪除 → `git add -A -- <paths>` → `git commit --no-verify --allow-empty --author=… --date=… -F - -- <paths>`。未複製的檔案不寫不刪。失敗即停,回報已建立的 commit 與失敗原因。
- 測試:在暫存 repo 建 3 個連續 commit(含 rename、刪除、merge、二進位檔)→ 複製 → 在另一個 repo 的不同分支重播 → 比對 message、作者、作者時間與檔案內容;不連續選取要被拒絕;本機有 staged 的其他檔案時不會被帶進 commit。

## 依賴
T-03、T-04、T-07(git runner、raw parser、cat-file)。

## 只可修改
`crates/core/src/commits.rs`。

## 驗收
`cargo test -p snip-core --lib commits` 全綠。
