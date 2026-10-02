# CLI 與 App 共用同一條複製／貼上引擎

狀態:**實作中**。第 3 節 D1–D7 由使用者於 2026-10-02 全部照建議拍板。

目標一句話:`snip` CLI 與原生 App 的複製、貼上、遠端配對,底層都走 `snip-core`／`snip-remote` 的**同一組函式**。兩邊的差別只剩 UI(CLI 是旗標＋文字輸出,App 是預覽＋勾選),修一個 bug 兩邊同時好。這也是讓 `docs/spec.md` §5.2「CLI 與 App 呼叫同一組核心函式,行為完全相同」重新成立。

---

## 1. 現況與證據

### 1.1 兩套引擎

| 動作 | CLI 走的(舊引擎,TS 移植) | App 走的(`transfer`) |
|---|---|---|
| 複製檔案／資料夾 | `copy::collect_copy_files`(`cli/src/main.rs:251`) | GUI 的 `expand_folder_items` → `transfer::plan_export_with`(`desktop-native/src/main.rs:4995–5047`) |
| 複製 working／staged／commit | `gitsrc::collect_payload`(`main.rs:273`) | `SourceKind::{Working,Unstaged,Staged,Commit}` → `plan_export_with` |
| 複製 range `a..b` | `gitsrc::collect_payload`(`GitSource::Range`) | 無(transfer 沒有 Range) |
| 複製 commits | `commits::select_* + copy_commits`(`main.rs:391–413`),無總量上限 | `transfer::plan_commit_export_exact_with(..., 64 MiB)`(`main.rs:5148`) |
| 貼上檔案 | `restore::plan_restore` + `execute_restore_plan`(`main.rs:619`) | `transfer::plan_import_with` + `TransferImportPlan::apply`(`paste.rs:1299–1369`, `main.rs:5603`) |
| 貼上 commits | dry-run:`commits::plan_commit_replay`;apply:`CommitReplayPreview::capture().apply()`(`main.rs:698–770`) | `CommitReplayPreview::capture_with` + `execute_commit` 的兩道關(`paste.rs:1783–1795`) |
| 路徑重新定位 | `suggest_restore_base` / `apply_restore_base`(`--adjust-paths`) | `detect_clipboard_prefixes` + 每個 prefix 一個選擇(`paste.rs:993`) |
| 配對清單 | 每次指令 load→改→save(`cli/src/remote.rs:134–167`) | 啟動時 load 一次,之後整份 save,錯誤丟掉(`desktop-native/src/remote.rs:211–221, 418–424, 486–490`) |

真正已共用的:剪貼簿讀寫、payload 格式(`format::write_payload_*`)、blob reader(`blob.rs`)、commit 內容讀取、remote worker(`snip_remote`,它不碰 copy/paste)。

Hindsight 記錄:2026-09-30 架構深化時「合併兩條 export pipeline」被明確列為**延後**項目。本計畫就是把它接回來。

### 1.2 CLI 因此留下的問題(code review 2026-10-02,xhigh)

標 ✅ 的已用 `target/debug/snip` 實際重現;標 📖 的是讀程式碼推得。

| # | 問題 | 位置 | |
|---|---|---|---|
| F1 | 大小寫別名 `[NEW] A.txt` + `[DELETED] a.txt` 在不分大小寫的 FS 上寫了又刪,檔案消失,exit 0 | `cli/src/main.rs:619` | ✅ |
| F2 | 貼 commit 時 `--skip-existing`/`--overwrite`/`--adjust-paths` 被默默忽略,蓋掉未 commit 的修改 | `main.rs:698` | ✅ |
| F3 | 跟隨指向 root 外的 symlink,把 `~/.ssh` 之類的內容放進 payload | `core/src/copy.rs:194` | ✅ |
| F4 | 碰到 FIFO 永久卡住 | `copy.rs:192` | ✅ |
| F5 | 不修剪 `.git`／巢狀 repo,`.git/config` 被複製且吃掉 30 檔額度 | `copy.rs:158` | ✅ |
| F6 | 兩套引擎本身(spec §5.2 不成立) | `main.rs:251` | ✅ |
| F7 | 結果為空仍覆寫剪貼簿、exit 0;違反既有決策 T-11(「No files selected.」且不覆寫剪貼簿) | `main.rs:252` | ✅ |
| F8 | 匯出安全規則(`folder_file_rel`、`expand_folder_items`)放在 GUI binary,CLI 無法共用 | `desktop-native/src/main.rs:59–93` | 📖 |
| F9 | GUI 存配對時整份覆寫,蓋掉 CLI 剛加的配對;GUI 吞掉存檔錯誤 | `desktop-native/src/remote.rs:419` | 📖 |
| F10 | 兩套路徑重新定位,同一份剪貼簿兩邊落點不同 | `desktop-native/src/paste.rs:993` | 📖 |
| F11 | 上限不一致:GUI 複製 64 MiB、GUI 貼上 32 MiB、CLI 無上限 | `main.rs:5389`, `paste.rs:38`, `gitrun.rs:107` | 📖 |
| F12 | `--working` 等模式先把所有變更完整讀進記憶體才套 30 檔／500 KB | `core/src/gitsrc.rs:850` | 📖 |
| F13 | GUI 的 freshness 重驗、`TargetCollision` 與 spec §3.2「一律覆蓋」、TS 都不同,卻沒登記在 porting-notes「已知且接受的差異」 | `core/src/transfer.rs:683` | 📖 |
| F14 | `is_relative_entry_path` 複製了 `restore::is_relative`;`FsProbe` 放在 CLI | `cli/src/main.rs:543, 563` | 📖 |
| F15 | `collect_payload_with_selection` 文件寫「Desktop selection」,實際 desktop 從不呼叫 | `gitsrc.rs:1100` | 📖 |

---

## 2. 目標與非目標

**目標**
- CLI 的 `copy`/`paste` 全部改呼叫 `transfer` 的入口;GUI 專屬的匯出過濾規則下沉到 core。
- 配對清單只有一個存取型別(`snip_remote::WorkerStore`),兩邊共用。
- 上限常數由 core 定義一次,兩邊引用。
- 每一個刻意與 TS 不同的行為都登記在 `docs/porting-notes.md`「已知且接受的差異」。

**非目標**
- 不改剪貼簿線上格式,不動 `fixtures/clipboard-contract.json`(它歸 ClipCodeVSCode 管)。
- 不改 GUI 的互動與畫面(除非第 3 節 D4 決定讓 GUI 也採用 restore-base 建議)。
- 不重寫 `restore::plan_restore`:它仍是 `plan_import_with` 每筆 entry 的規劃器,也是 contract 測試的對象。
- 舊函式(`collect_copy_files`、`collect_payload`)不急著刪,先降級為測試 oracle;全部遷完、穩定一個版本後再評估移除。

---

## 3. 需要使用者先決定的事項

AGENTS.md 與既有慣例規定:與 TS 不同的行為**只有使用者能決定保留**,而且要登記。下面每一項都會改變 CLI 的可見行為或與 TS／spec 的一致性,所以列出來請你拍板。每項附建議。

### D1 — CLI 貼上要不要採用 transfer 的兩道防護?

`transfer` 貼上比 TS 多兩道:(a) `TargetCollision`:兩筆 entry 指向同一個實體檔(含大小寫、symlink 別名)就整批拒絕;(b) freshness:預覽後目標或 repo HEAD/index 有變就拒絕(`StaleDestination`)。兩者都違反 spec §3.2「一律覆蓋,不偵測目標是否被改過」,也與 TS 不同;GUI 已經這樣做,但沒登記。

- **建議:採用**,並把兩者登記為已接受的差異、改寫 spec §3.2。理由:F1 是實際資料遺失,而 CLI 的 dry-run → apply 中間本來就可能隔很久。CLI 單一指令 `--apply` 時 plan 與 apply 緊接著,freshness 幾乎不會誤擋。
- 代價:contract fixture 有三個案例描述的是 TS 行為,CLI 改走 `plan_import_with` 後會**不一樣**(見 D2)。

### D2 — contract fixture 三個 TS 行為怎麼處理?

`contract.rs::restore_cases`(`crates/core/tests/contract.rs:414`)直接測 `plan_restore`,所以**測試本身不會紅**。但 fixture 裡有三個案例在 CLI 改走 `plan_import_with` 後,CLI 的實際行為會偏離:

1. 「the same path twice is planned twice, in order」→ transfer 會變成 `TargetCollision` 錯誤。
2. 「a sibling root label targets that root」(`backup/` 落到 backup root,需要多 root)→ CLI 目前只給一個 root,transfer 需要有人建 `ImportMapping`。
3. 「an absolute path matching no root is kept literally under the primary root」(`D:\work\lib\b.ts` → `D/work/lib/b.ts`)→ transfer 會變 `UNRESOLVED_PATH` 跳過。

- **建議**:1 依 D1 接受為差異;2 CLI 目前本就只有單 root,不受影響,登記「CLI 單 root」即可;3 接受為差異(跳過比寫進奇怪的 `D/` 目錄安全)。三者都寫進 porting-notes,fixture 與 `contract.rs` 不動。
- 若你要保留 TS 行為:替代方案是 `plan_import_with` 加一個「TS 相容模式」旗標,但那等於在 transfer 裡再長出第二套語意,不建議。

### D3 — `--range a..b` 怎麼辦?

transfer 沒有 Range 來源;拿 `Commit` 硬湊會讓刪除檔讀到 B 的 parent 而不是 A,內容錯。

- **建議:在 transfer 加 `SourceKind::Range { base, tip }`**(刪除檔讀 `base:<path>`),讓 range 也只有一套實作,GUI 之後要做 range 複製可直接用。
- 替代:`--range` 暫留 gitsrc,在 porting-notes 標為「唯一未遷移的模式」。

### D4 — 路徑重新定位統一成哪一種?

CLI 用 restore-base(一個全體套用的 Strip/Add 建議,讀 `clipcode-root`);GUI 用每個 prefix 一個選擇,而且有 `clipcode-root` 時完全不做。`ImportMapping` 兩種都表達得出來(Strip → `map_prefix`;Add → `map_entry`)。

- **建議**:core 新增 `ImportMapping::from_restore_base(&RestoreBaseSuggestion, primary)` 這一個轉換;CLI `--adjust-paths` 經它建 mapping。GUI **第一步不改**,只在第 6 階段後另開一張單決定要不要把 restore-base 建議當成預設選項(那是 GUI 行為變更,要另外驗收)。
- 這樣 F10 的「兩套實作」先收斂成「一個 mapping 型別、兩個 UI 產生方式」;落點差異在 GUI 那張單解決。

### D5 — 貼 commits 的旗標語意

- **建議**:`--skip-existing` 對 commit payload → exit 2「commit 模式不支援 --skip-existing」;目標有未 commit 修改會被覆寫時,沒給 `--overwrite` → exit 2(對齊 GUI 的 `commit_overwrite_required`);`--adjust-paths` 對 commit payload → exit 2。
- 這和 spec §4.3「直接覆蓋」不同(GUI 已經如此,同樣未登記),一併改 spec 並登記。

### D6 — 上限統一成多少?

- **建議**:core 定義 `transfer::CLIPBOARD_PAYLOAD_MAX`。複製上限(兩邊)≤ 貼上預覽上限,避免「GUI 複製成功、另一台 GUI 貼不上」。具體:複製上限改為 32 MiB,與貼上預覽預算一致;或把 GUI 貼上預算放大到 64 MiB(要重跑記憶體量測,`docs/memory-measurement-protocol.md`)。我傾向前者,記憶體目標優先。
- CLI 複製超過上限 → exit 1 並說明,不截斷(既有決策:超量是 strict error,不默默截斷)。

### D7 — `snip copy <paths>` 的路徑解析

- **建議**:維持「相對路徑以 cwd 解析」(shell 慣例);路徑不存在 → exit 1;路徑在 `--repo` 外 → exit 1(目前舊引擎給絕對路徑 label,transfer 表達不了)。結果為空 → 「No files selected.」exit 1,不碰剪貼簿(T-11)。

---

## 4. 分階段實作

規則沿用 2026-09-30 架構深化時的做法:每一階段一個 `feature/` 分支、從最新 `develop` 切、一個 PR、squash merge;同時最多兩個實作中的變更;一次只跑一個 `just preflight`,在完整 clone 跑(worktree 不行)。**行為不變**的階段必須做到相對 `develop` 零行為差;會改行為的階段只改第 3 節已拍板的部分。

### 階段 0 — 決策落地(只改文件)

- 依第 3 節的決定改 `docs/spec.md` §3.2、§4.3,並在 `docs/porting-notes.md`「已知且接受的差異」登記 GUI 現有的 freshness、`TargetCollision`、`commit_overwrite_required`、32 MiB(F13)。
- 只改 `.md`,不需 preflight。

### 階段 1 — 純搬移,不改行為(F8、F14)

- `folder_file_rel`、`expand_folder_items`、`FolderExpansion` 從 `desktop-native/src/main.rs:59–93` 搬到 `core::transfer`(或新的 `transfer::select` 子模組),簽名例如 `expand_folder_items(sel: ExportSelection, limit: usize, cancel: &CancelToken) -> Result<FolderExpansion, TransferError>`。
- `restore::is_relative` 改 `pub`,CLI 刪掉 `is_relative_entry_path`;`FsProbe` 搬到 `restore.rs` 的 `DirProbe` 旁(`restore::FsProbe`)。
- 測試:GUI `tests::in_process` 與 native-e2e 是 oracle,不得有任何變化;搬過去的函式在 `crates/core/tests/transfer_planning.rs` 補單元測試(FIFO 跳過、root 外 symlink 跳過、`.git`／巢狀 repo 修剪、budget 截斷、cancel)。
- 關卡:`just preflight`。

### 階段 2 — core 補齊 CLI 需要的入口

- `transfer::selection_from_paths(root, paths: &[PathBuf]) -> Result<(ExportSelection, Skipped), TransferError>`:把 cwd 相對路徑轉成 root 相對 item。**必須處理 `snip copy .`**:root 本身不是可匯出 item,而 `.git` 修剪會把 repo root 整個丟掉,所以 root 要當成「展開 root 的資料夾 item」特判。
- 依 D3:`SourceKind::Range { base, tip }`,刪除檔讀 base;在 `transfer_planning.rs` 用舊 `collect_payload(GitSource::Range)` 當 oracle 做位元組比對。
- 依 D4:`ImportMapping::from_restore_base`。
- 依 D6:`CLIPBOARD_PAYLOAD_MAX` 常數。
- `transfer::changed_items(git, mode)`(或同義):列出 working/staged/commit 的變更 item,**保留 gitsrc 的順序**(worktree、untracked、index),`--working` 全部用 `SourceKind::Working`,否則會撞 `StagedWorkingConflict`,而且順序變了 payload 位元組就變。
- 這一階段只加 API 與測試,CLI 還沒切過去,行為不變。關卡:`just preflight`。

### 階段 3 — CLI `--commits` 改走 transfer(最便宜,先上)

- `transfer::plan_commit_export(git, range, last)`(`transfer.rs:2249`)已與 CLI 現在做的一樣;改呼叫它,並套 D6 的上限。
- 唯一差異:不連續範圍回 `TransferError::DiscontinuousCommits`,CLI 對應到原本的「not contiguous」訊息。
- 測試:`crates/cli/tests/cli.rs` 的 commit 測試(:155–169, :301–552)不得變;新增超過上限 exit 1。

### 階段 4 — CLI `copy <paths>` 改走 transfer(F3、F4、F5、F7)

- `copy_paths` → `selection_from_paths` → `expand_folder_items`(**展開上限傳無上限**,讓 30 檔只算「真的複製的檔案」,與舊引擎一致)→ `plan_export_with(..., Some(CLIPBOARD_PAYLOAD_MAX))`。
- 依 D7 處理不存在、root 外、空結果。
- 新增 `cli.rs` 回歸測試,一個發現一個:FIFO 不卡(帶逾時)、root 外 symlink 被跳過、`.git` 不出現、空結果 exit 1 且剪貼簿不變、typo 路徑 exit 1。
- **位元組相容**:`crates/cli/tests/e2e.rs` 的 ts-ref 交叉測試(:436, :464, :492)必須維持全綠,而且要另外以「經 symlink 拼寫的 root」跑一次——transfer 會 canonicalize `--repo`,root 的最後一段名稱若不同,`// clipcode-root:` 就變了。本機要有 node,否則測試會跳過(CI 有 `SNIP_REQUIRE_ALL_TESTS`,跳不掉)。
- stderr 計數(unreadable 等)可能不同:那只影響訊息,不影響 payload 位元組,不要把它當相容性失敗去追。

### 階段 5 — CLI `--working/--staged/--commit/--range` 改走 transfer(F12、F15)

- `copy_git` → `changed_items` → `plan_export_with`,讀取量由 `remaining_budget`/`read_cap` 限住(F12)。
- `--repo` 是 repo 子目錄時,label 規則會變(舊引擎:範圍外的變更給絕對 label;transfer:`UnsafePath`)。`cli.rs:274` 的子目錄測試要依新規則改,並在 porting-notes 登記。
- 修正 `collect_payload_with_selection` 的文件為「測試 oracle」(F15)。
- 測試:`transfer_planning.rs` #26–28、#36–38 與 :2415 原本用舊引擎當 oracle,保留;`cli.rs` 新增大檔案／大量未追蹤檔不爆記憶體的測試(以 payload 上限錯誤收尾,而不是 OOM)。

### 階段 6 — CLI `paste` 改走 transfer(F1、F2、F10 的 CLI 端)

- 檔案 payload:`plan_import_with(text, header, &[repo], mapping, opts)`,mapping 為 `with_primary(repo)`,`--adjust-paths` 時由 `from_restore_base` 產生;`--dry-run` 印 `TransferImportPlan` 的 create/overwrite/delete/skip(輸出格式不變);`--apply` 呼叫 `TransferImportPlan::apply(&RestoreSelection{overwrite_existing / skip_existing})`。
- `TargetCollision`、`StaleDestination` 對應到清楚的錯誤訊息與 exit 1(依 D1)。
- commit payload:dry-run 也改走 `CommitReplayPreview::capture_with`(預覽與 apply 同一份 plan),旗標依 D5。
- 測試(`cli.rs`):F1 大小寫別名在 macOS 與 Windows 拒絕、Linux 照常;F2 `--skip-existing` 貼 commit exit 2 且檔案不變;`--adjust-paths` 目前**完全沒有測試**,這一階段要補(Strip 與 Add 各一);既有 `file_mode_round_trip`(:57–119)輸出不得變。
- `contract.rs` 不動(依 D2)。

### 階段 7 — 共用配對存取(F9),可與階段 3–6 平行

- 新增 `crates/remote/src/store.rs`:`WorkerStore { path }`,`load()`、`add(worker)`、`forget(fingerprint)`(用指紋不用索引,避免 GUI 的舊索引刪錯人)、`find(query)`(從 CLI 搬來,含 1-based 編號)。每次 add/forget 在 sidecar `remote-workers.json.lock` 的獨佔鎖內 load→改→save;temp 檔名每個 process 唯一(目前固定 `json.tmp`,兩個 process 同時存會撞)。鎖用 `std::fs::File::lock`,不加依賴。
- worker 端的 `remote-trusted-masters.json`(`worker.rs:124–126, 484–485`)同樣改用 store,不再吞錯。
- GUI:每次變更後重新 load,存檔失敗顯示錯誤;CLI:改呼叫 store。
- 測試:store 單元測試(兩個 process 交錯 add 不遺失);`cli/tests/worker.rs`;`scripts/remote_e2e.sh` 加一條「CLI pair 後,另一個 store 實例 forget 別人,CLI 的配對仍在」。
- 關卡:`just preflight`,而且因為動到 `crates/remote`,**必須另外跑 `just remote-e2e-ssh <host>`**(經 Tailscale 的第二台機器)。鎖相關程式碼大多有 `cfg`,push 前本機跑 windows-msvc clippy。

### 階段 8 — 收尾文件

- `docs/spec.md` §5.2:改成實際成立的敘述,列出 CLI 與 App 共用的入口(`plan_export_with`、`plan_import_with`、`CommitReplayPreview`、`WorkerStore`)。
- `docs/plan.md` §2 架構、§5 測試策略、§6.5「存放位置」補 store 與鎖。
- `docs/porting-notes.md` §3(還原安全規則)、§4(複製規則)更新。
- `AGENTS.md`「Where a test goes」補一行:CLI 行為與輸出 → `crates/cli/tests/cli.rs`;位元組往返與 TS 相容 → `crates/cli/tests/e2e.rs`。
- 只改 `.md`,不需 preflight。

---

## 5. 發現與階段對照

| 發現 | 階段 |
|---|---|
| F1 大小寫別名 | 6(D1) |
| F2 commit 旗標被忽略 | 6(D5) |
| F3 symlink 外洩 | 1 + 4 |
| F4 FIFO 卡住 | 1 + 4 |
| F5 `.git` 被複製 | 1 + 4 |
| F6 兩套引擎 | 3–6 |
| F7 空結果覆寫剪貼簿 | 4(D7) |
| F8 規則在 GUI binary | 1 |
| F9 配對被覆寫 | 7 |
| F10 兩套路徑重新定位 | 2 + 6(D4),GUI 端另開單 |
| F11 上限不一致 | 2 + 3/4(D6) |
| F12 無上限讀取 | 5 |
| F13 未登記差異 | 0 |
| F14 重複的 is_relative／FsProbe | 1 |
| F15 誤導的文件 | 5 |

---

## 6. 風險與注意事項

- **payload 位元組**是最容易默默壞掉的地方:順序(gitsrc 順序 vs GUI 的依路徑排序)、`fallback` 開關、`clipcode-root` basename(canonicalize)。每個 CLI 遷移階段都以「舊引擎輸出 == 新引擎輸出」的位元組比對測試把關,加上 ts-ref 交叉測試。
- **路徑經 symlink 拼寫**:AGENTS.md 要求 path/fs 程式碼以 symlink root 測試(macOS `/var` → `/private/var`)。階段 1、2、4、6 的新測試都要有這一版。
- **等待類測試**:FIFO 測試、跨 process 鎖測試都要有會失敗並給訊息的逾時,不能無界 `recv()`/`wait()`。
- **跳過的測試**:需要 node、display、`.ts-ref` 的測試先 `assert!(std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(), …)`。
- **native-acceptance** 在 build 後 checkout 不能變:先 commit 再跑,跑的時候不動 tree。
- 舊函式降級為 oracle 後,有人可能照舊文件去改它們而 CLI/GUI 都沒變——每個被降級的函式文件第一行寫明「測試 oracle,產品程式碼不呼叫」。
