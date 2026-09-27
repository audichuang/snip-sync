# Native 協作驗收 fixture

狀態：資料集與 UI driver 已實作；凍結 binary `b11e6724…` 的第四輪 Linux GUI 功能驗收 18/18 通過、退出碼 0，36 個 App 都正常 drained/exit 0。原始 manifest 未變，前三輪失敗證據保留。完整 release acceptance 仍需 graph painted-edge 證據、其他平台、最終版本 binary 與獨立記憶體 gate；driver 單元測試本身不等於 native UI acceptance。

`scripts/collaboration_fixture.py` 建立兩台模擬機器的真實 Git 工作區與版本化 truth manifest。它不呼叫 snip-sync、不貼上、不驅動 UI。UI driver 讀 manifest，要求真剪貼簿、真 OS input、截圖與套用證據。沒有這些實際執行證據時，generate、verify 或 driver 單元測試成功不得寫成協作驗收通過。

這是功能驗收資料，不是 standard performance benchmark，也不依賴 `workload_generator.py`。

## 佈局

輸出目錄只含相對路徑：

- `manifest.json`
- `machine-a/<parent>/<basename>/`：15 個獨立 worktree
- `machine-b/<parent>/<basename>/`：15 個獨立 worktree
- `origins/pair-NN.git`：15 個本地 bare origin，不在 A/B 裡面。`--no-origins` 可省略

每一對都有固定 repo id。A 與 B 的 basename 不同，所以不能用資料夾名當目的地。`billing` 同時出現在 `machine-a/west` 與 `machine-a/east`；`edge` 同時出現在 `machine-b/north` 與 `machine-b/south`。對應關係在 manifest 的 `pairs`，例如 `a-west-billing` → `b-north-ledger`。

明確指定到另一個存在的 root（`file-explicit-valid-root-pair01-to-pair15b` 把 `a-west-billing` 指到 `b-south-edge`）是允許的正向步驟。錯誤 mapping 只涵蓋三種：同一目的路徑碰撞、只給 basename 而無法唯一決定、目的 repo id 不存在。

`remote.origin.url` 是相對於該 worktree 的 `../../../origins/pair-NN.git`。搬移輸出目錄後，manifest 與這條 URL 都不必改。fixture 不會 fetch、pull 或 push。

## Git 真實狀態

每個 repo 約 10–30 個 commit。作者是 Ada Lovelace、Grace Hopper、Ken Thompson、Mary Keller；committer 固定為 Fixture Committer，而且 committer 時間比 author 時間晚 60 秒。時間寫死，不用牆鐘。

共享歷史含 root、diverged `topic`、兩親 merge、rename `notes/base.txt` → `notes/guide.txt`、delete `src/drop_me.txt`、lightweight tag 與 annotated tag。`pair-01` 的 A 另有四親 octopus。`pair-05` 的 A 另有 orphan root。本地 `local-a` / `local-b` 與 `origin/*` 分叉；`unmerged-a` / `unmerged-b` 沒有併進 HEAD。`pair-08` 的 B 目前 HEAD 就是 `unmerged-b`。

除了 `a-east-infra`，各 repo 都有：

- `src/samepath.txt`：HEAD、index（version A）、worktree（version B）三者不同
- `transfer/staged.txt`：index 與 worktree 不同
- `transfer/working.txt`：只有 worktree 改過
- staged 新檔 `local/hold.txt`
- untracked `scratch/untracked.txt` 與 `scratch/identity.txt`
- ignored `scratch/ignored.txt`（規則在 `.git/info/exclude`，snapshot 仍記錄 bytes）

`a-west-mobile` 另有 staged rename。`a-west-search` 另有 worktree delete。`a-east-infra` 是進行中的 merge conflict（index stage 1/2/3 與 `MERGE_HEAD`）。正向步驟的目的地都不是這個 conflict repo。

## Manifest

`schemaVersion` 是 1。`datasetHash` 是 canonical JSON 的 SHA-256：`datasetHash` 先設成空字串，`sort_keys`，分隔符為 `(',', ':')`，UTF-8。路徑都是相對於 fixture root 的 POSIX 路徑。

每個 repo 記錄 HEAD branch/oid、refs（含 peeled tag）、每個 commit 的 parents、author、committer、完整 message、tree、index stage entries，以及 worktree、untracked 與 ignored 檔的 SHA-256 / size / base64。ignored 來自 `git ls-files -o --ignored --exclude-standard`，不是另一套掃描器。base64 來自分檔讀取，不把路徑當 shell 字串。

`steps` 彼此獨立，都從這份初始快照起算，不是一條會累積的腳本。

Replay 會保留的是 author name、email、author time、完整 message，以及把該 commit 對 first parent 的 UTF-8 檔案 diff 套到目的地後的 tree。不保留 source commit OID、source parent 清單、committer name/email/time。新 commit 只有一個 parent：目的地當時的 HEAD，其後才是上一筆 replay。

檔案步驟只改 worktree，而且目的相對路徑就是來源 repo 相對路徑。不同 repo 名稱只靠 repo id mapping，不做逐檔 rename。已存在且內容會變的目的檔才標成已授權覆寫。HEAD、refs、index 維持不變。選取內容沒有首尾空白行，也沒有結尾換行，因此 file-mode 剪貼簿那條「去掉首尾空行」的契約不會改到這些 bytes。這個產生器沒有重做 clipboard codec。

`file-a-to-b-pair01` 的 `src/extra.txt` 是來源 worktree 刪除：index 仍有 blob，`git diff` 列 `D`，目的 repo 的同一路徑仍在。`file-b-to-a-pair09` 的 `notes/guide.txt` 是來源 index 刪除：`git diff --cached` 列 `D`，HEAD 仍有 blob，目的 repo 的同一路徑仍在。verify 會重讀這兩筆 `D` 與 oid；來源檔被還原或 provenance 被改掉都會失敗。

Commit 步驟用私人 scratch：`git diff-tree -r -z -M` 對 first parent（root 則對 empty tree），略過含 NUL 的 blob，再 `git add` 那些路徑並 `git commit --only`。`local/hold.txt` 會留在 index，且不會出現在新 commit tree。這是 spec 4.3 / `commits.rs` 的假設，不是檔案系統交易。`pair-02` 的 `assets/tiny.bin` 含 NUL，標成 not copied，不寫入也不刪除。

`commit-root-pair05` 含 orphan root。`commit-merge-first-parent-pair04` 只帶 merge 對 first parent 的 diff。`commit-octopus-first-parent-pair01` 的範圍只有 octopus 那一筆 first-parent commit，不會把另外三個親的歷史展開。`commit-rename-delete-pair08` 套到尚未含該 rename 的 `unmerged-b`，所以結果 tree 不會憑空出現只存在於後續 main 的 `src/feature.txt`。

verify 會用來源 `git cat-file commit` 的 author name、email、author time 與完整 message（保留 commit 物件裡的換行，不 trim）核對 `expectedCommits`，並把 fresh `replay_commits` 的 commits、index、files（path/bytes/size）、absent、final tree entries、applied paths 整份拿來比。新 commit OID 與 committer 時間不在 oracle 裡，所以不比。

負向步驟的 `writes` 是 false。除了下面兩個過期步驟，`expected` 是 `full-baseline-snapshot`：

| id | reason |
| --- | --- |
| `neg-cross-repo-commits` | 兩個 repo 的 commit 混選 |
| `neg-noncontiguous-tips` | 同一 repo 的 `topic` 與 `local-a` 不是一段 first-parent |
| `neg-mapping-collision` | 兩個來源的同一相對路徑撞上同一個目的檔 |
| `neg-mapping-ambiguous-basename` | 只給 `billing`，west 與 east 都有 |
| `neg-mapping-missing-destination` | 目的 repo id 不存在 |
| `neg-stale-source` | 來源 preview/selection 之後外部改了來源，Copy/export 做 freshness；不是目的 Apply 回查另一台電腦 |
| `neg-stale-target` | 目的 paste preview 之後改了目的 HEAD/index/選取內容，Apply 必須拒絕 |
| `neg-overwrite-unauthorized` | 目的檔已存在且未授權覆寫；整步不寫 |
| `neg-cancel` | 預覽後取消 |

產生時的 baseline 只是生成當下的 truth。Driver 的 stale-source race 在 Copy 已捕捉 export plan、尚未 final revalidation 時才修改來源；hold 解除前拍下修改後快照。`neg-stale-source` 與 `neg-stale-target` 的不寫入證明，是故意改完之後、執行 Copy/export 或 Apply 之前立刻拍的 30-repo snapshot；動作後再拍一次，必須等於那張動作前快照。setup 造成的差異要另外留著，不能拿生成 baseline 當成動作前狀態。來源端若是釘住的 commit 內容，保留釘住的 bytes。既有 export contract 沒有保證每次 working tree 修改都會在 Copy 前被拒絕；那要由 driver 拿契約證據，fixture 不假設有跨機器 watcher。

取消與其他負向步驟仍以生成 baseline 為不寫入結果。比較器對一般快照要含 ignored 檔。Commit apply 的 ref 集合必須與 baseline 相同：只有目的地目前的 HEAD branch 改指新 HEAD，不能多也不能少。

## CLI

在 repo root：

```text
python3 scripts/collaboration_fixture.py generate --output DIR [--no-origins] [--log PATH]
python3 scripts/collaboration_fixture.py verify --fixture DIR [--log PATH]
python3 scripts/collaboration_fixture.py snapshot --fixture DIR --output SNAP.json
python3 scripts/collaboration_fixture.py compare-step --fixture DIR --step STEP --snapshot SNAP.json --phase initial|applied
```

結束碼：0 成功，1 驗證或比對失敗，2 目標路徑不安全，3 沒有 Git。Git 缺失、timeout、fast-import 失敗都不是成功。

`generate` 拒絕空路徑、非空目錄、目標本身是 symlink、任一祖先是 symlink，也不會跟著 symlink 寫進去。失敗時只刪除這次新建的輸出目錄；呼叫前就存在的目錄與檔案留著。Git 使用空的 `GIT_CONFIG_GLOBAL`、`GIT_CONFIG_NOSYSTEM=1`、關閉簽章與網路協定，並把 hooks 指到空目錄。每筆 commit 都帶固定 author/committer 時間。命令列是 argv，不經 shell。每筆 Git 有 30 秒上限。

`verify` 重算 dataset hash，再用 Git 讀 HEAD、refs、commit、index 與檔案 bytes。正向 commit 步驟會在 scratch 重跑 oracle，tree / index / worktree 必須與 manifest 一致。只改 hash 或只改 manifest 裡的 base64 都會失敗。

`snapshot` 寫出當下 30 個 repo 的同一套欄位，給 UI runner 在操作前後各拍一次。`compare-step --phase initial` 要求快照等於 baseline。負向步驟的 `applied` 也必須等於 baseline。正向 file 步驟比較 worktree delta，HEAD 與 index 不變。正向 commit 步驟比較新 commit 的 tree、author、author time、message 與單親鏈，不比較新 commit OID 或 committer 時間。

沒有 Git 時，unittest 在未設定 `SNIP_REQUIRE_ALL_TESTS` 時 skip；設定該變數時必須失敗。CLI 不論該變數是否存在都回非 0。

## 尚未完成

- 記憶體與 fd/thread/child gate 不在這支 fixture，也不由下面的 UI runner 宣稱。
- 不修改 pinned TS、`fixtures/clipboard-contract.json`、標準效能資料集或產品程式。

## Runner 用法與證據

這四件事嚴格分開：

| 閘 | 是什麼 | 成功代表什麼 |
| --- | --- | --- |
| 功能 fixture | `scripts/collaboration_fixture.py` 的 generate / verify / snapshot / compare-step | 30 repo Git 資料與 manifest 一致。不是 UI acceptance。 |
| 歷史 UI pilot | `check_native_collaboration.py --phase pilot` 配上已凍結的 native binary | 4 個 pilot 步驟可用真實視窗、真實 Copy、OS clipboard bytes、真實 Paste 執行。pilot 子集結束碼必為非 0（未達 18 步全集）。 |
| Linux 功能閘 | `check_native_collaboration.py --phase all`（`scope: linux-functional`） | Manifest 規定的全部 18 個獨立步驟在 Linux 上全部通過，且具備完整實體輸入、剪貼簿讀回、Git oracle/快照比對、刪除 provenance 與 process cleanup 零存活證據。此時 Linux 執行結束碼為 0；但報告中 `acceptanceComplete` 仍為 false。 |
| 跨平台發布閘 | 完整產品發布晉級驗收（Release Promotion Gate） | Linux + macOS + Windows 三平台功能全過、繪製 git graph 邊線 probe 驗證（`paintedEdgesProven`）、正式 release binary、以及獨立記憶體/句柄長照 soak gate 皆完成。只有全部達成時 `acceptanceComplete` 才為 true。 |

在 repo root，顯式給 binary、SHA、fixture、dataset hash、空的輸出目錄，以及有限的單步逾時。`--help` 只印說明，不會啟動 GUI，也不是測試通過。Helper 從 `PYTHONPATH` 的 `bench_native_memory.py` 載入（正式整合時放在 `scripts/` 旁邊）。不要把暫時目錄寫進預設值。

```text
PYTHONPATH=/path/to/helper-dir \
python3 scripts/check_native_collaboration.py \
  --binary /path/to/snip-desktop-native-debug \
  --binary-sha SHA256 \
  --fixture /path/to/verified-fixture \
  --dataset-hash DATASET_HASH \
  --helper-receipt /path/to/receipt.json \
  --output /path/to/empty-output \
  --timeout 60 \
  --phase all
```

`--phase pilot` 只跑 4 步 pilot（`file-a-to-b-pair01`、`file-b-to-a-pair09`、`commit-a-to-b-pair02`、`commit-b-to-a-unmerged-pair03`）；`--phase all` 跑完整 18 步。Linux 功能閘結束碼 0 要求全部 18 步都實際通過且具備完整證據；pilot 子集、blocked、timeout、clipboard 不一致、cleanup 失敗或缺任一證據都是非 0。

### 控制項與互動規範

1. **Repo 選擇與消歧義**：Repo 列表將同名 repo 顯示成 workspace 相對路徑（如 `west/billing` 與 `east/billing`）；由 header 的 `btn-repo-selector` 展開帶序號的 `pick-repo:N:label`。點選後必須由 `[APP:REPO_SELECTING:` 解析出的 canonical root 與 manifest 目的 root 嚴格比對。
2. **刪除操作（Deletion Provenance）**：Manifest 刪除規格為 `op: "delete"` 且 `source.kind` 為 `"working"` 或 `"index"`（非發明的 `sourceKind` 欄位；原始 fixture 產生的 schema 即是如此，絕無假造的刪除崩潰修復）。Driver 依 `source.kind` 解析來源：`"working"` 對應 `unstaged`（`change-row:unstaged:<path>` / `change-chk:unstaged:<path>`），`"index"` 對應 `staged`（`change-row:staged:<path>` / `change-chk:staged:<path>`）。嚴格要求 `status == "D"`、`inWorktree is False`、`diffAgainst` 與 blob `oid`；歧義或無效欄位直接失敗，絕不假通過。
3. **歷史檔案固定 OID（Fixed-OID Selection）**：依循 current `smoke.rs` 規範歷史瀏覽流程：
   - 點選 commit 列（`commit-row:<short>`），驗證當前 active root 與預期 canonical root 嚴格相符。
   - 讀取 Git oracle（`git rev-parse {commit}:{path}`）驗證 blob OID 與 manifest 預期完全一致。
   - 點選 `btn-browse-tree` 進入歷史樹瀏覽，等待 `[APP:REV_TREE: <short>]` 與 `[APP:E2E_TREE: rev=<short> dir=/` 根目錄準備完成。
   - 逐層點選目錄列（`rev-row:<parent>`）展開父目錄，等待 `[APP:TREE_EXPANDED:`（禁止壓制 `MissingControl`）。
   - 點選檔案列（`rev-row:<path>`），等待 `[APP:PREVIEW_LOADED:` 與 `[APP:E2E_PREVIEW: source=commit_file` 完成載入。
   - 勾選歷史核取方塊（`rev-chk:<full-commit-oid>:<relative-path>`），等待 `[APP:BASKET: n=` 更新。
   - 在進行後續工作區/index 檔案選取前，點選 `btn-leave-tree` 退出歷史樹模式（`[APP:REV_TREE: off]`），再明確點選 `rail-changes` 並要求 fresh `TAB_SWITCHED: GitChanges visible=true`。產品進入歷史樹直接切至 Project，退出後也維持 Project，因此不能只信任 helper 的舊 tab 事件。
4. **目的地映射（Paste Destination Mapping）**：
   - `NativeSession.lines` 契約為 `list[tuple[float, str]]`，文字檢索一律透過 `session.texts(start)`，游標一律取 `len(session.lines)`。
   - `[APP:PASTE_MAP_CANDIDATE:` 於產品中係在 `[APP:PASTE_PREVIEW:` **之前**輸出。Driver 必須在點選 Paste 前即儲存游標，並將該起點傳入映射解析，禁止以 preview 後的游標截斷候選名單。
   - 嚴格由候選名單之 canonical root 與 manifest 目的 repo root 匹配，完全移除單一按鈕 fallback。
   - 點選對應的 `paste-map-pick:<prefix>:<idx>` 後，以正則運算式保留空格與 Unicode，嚴格驗證輸出的 `[APP:PASTE_MAPPED: prefix=... dest=... items=...]` 之 mandatory prefix 與 canonical dest 與預期一致。
   - 覆寫開關支援 `paste-items` 視口探針滾動，禁止偽造座標。
5. **負向案例防禦與證據驗證**：
   - **混選跨 Repo Commit（`neg-cross-repo-commits`）**：選取 repo 1 tip，切換至 repo 2 並選取其 tip，執行一次真實複製。以 Git `log -1 -z --format=%an%x00%ae%x00%aI%x00%B`、first-parent `diff-tree -r -z -M` 與 blob bytes 建立獨立 oracle，精確比對 commit 陣列順序與數量、完整 message（含結尾換行）、author name/email/date，以及每個檔案的 `path`、`oldPath`、`change`、`content`、`notCopied`。Wire 沒有 OID 欄位；不可用 SHA 子字串或任一筆作者／標題相符冒充完整比對。保存原始 clipboard 與 `commit-oracle.json`，並確認 baseline 零寫入。
   - **不連續 Commit（`neg-noncontiguous-tips`）**：選取 manifest 的 `baseOid` 與 `tipOid`，使用同一個已讀回的 clipboard sentinel 執行 Copy。只有 fresh `COPY_COMMITS_ERR: commits are not contiguous:` 與 sentinel 完全未變可作拒絕證據；`no_selection` 或一般錯誤都不算。
   - **路徑碰撞（`neg-mapping-collision`）**：fixture 的來源是 billing 與 docs，但最後選取的 docs 是主要來源，真實 wire 為 `billing/src/app.txt` 與 `src/app.txt`（主要 root 省略 basename），沒有 docs mapping 控制。先精確核對原始 clipboard 的兩條來源路徑，再操作 billing→canonical ledger，以及 src→keep-relative；兩者都必須由 fresh `PASTE_MAPPED` 確認同一 ledger root，後者還需 `keep=primary`，所以兩者的 canonical 目的檔同為 `ledger/src/app.txt`。第二次 replan 必須出現 `[APP:PASTE_PLAN_REFUSED: reason=target_collision]`，且所有 repo 零寫入。一般 `PASTE_ERR`、`paste_err_plan`、`mapping_required` 或 `items=0` 都不是碰撞證據；driver 不改寫 payload。

   - **歧義 Basename（`neg-mapping-ambiguous-basename`）**：跨 `a-west-billing` 與 `a-west-docs` 建立 multi-root 匯出，於工作區 A 的 `a-west-billing` 貼上。先展開來源父目錄，所有游標在點擊前取得。驗證 fresh `mapping=false` 與 `billing` 候選 canonical roots 包含 manifest 的 `a-west-billing`、`a-east-billing`，然後要求明確 `mapping_required`，取消並確認零寫入。沒有虛構的 `a-west-ledger` repo。
   - **缺少目的地（`neg-mapping-missing-destination`）**：依驗收決策，以 `prevention=canonical-destination-whitelist` 記錄 prevention，而非提交無效 ID 後的拒絕。保留原 manifest，明確標示 `originalMappedIdSubmitted=false`。要求 `billing` prefix 的完整候選 canonical roots 精確等於 B 工作區 15 個實際存在的 repo，沒有不存在的 ID/path、沒有自動 mapping；Return 必須產生 fresh `mapping_required`，rendered disabled Apply 必須確實點擊且未套用，clipboard bytes 未變，30-repo snapshot 零寫入，兩個 App 都正常 drained/exit 0。缺少或截斷候選、任何自動映射、缺鍵盤拒絕、寫入或 cleanup 缺證據都不能通過。現有 UI 只允許 canonical root 白名單或 keep-relative，因此不新增可提交不存在目的地的產品控制。核心 `test_missing_root_rejected_at_boundaries` 覆蓋未宣告 root 拒絕，但不列為 UI 執行證據。

   - **套用拒絕（`try_apply_refusal`）**：要求當前世代的 `mapping=false`，送出 Return 並取得 fresh `[APP:PASTE_ERR: mapping_required]`，再以真實視窗 bounds 點擊 rendered `btn-apply`，有界觀察最多 1 秒。任一鍵盤或點擊後 `PASTE_APPLYING`／`PASTE_DONE`、缺少控制、視窗範圍錯誤或程序退出都失敗；沉默不是拒絕證據。
   - **過期來源（`neg-stale-source`）**：在 session 建構前設定 `SNIP_NATIVE_E2E_EXPORT_HOLD_FILE`。選取完成後、Copy 前建立 hold 檔，等待 fresh `[APP:EXPORT_PLAN_READY: files=N]`（N > 0），此時才修改自己 fixture 的來源檔；修改後拍 pre-action snapshot，再移除 hold。必須得到 fresh `[APP:COPY_FAILED: stale_source]` 與 `[APP:COPY_IDLE]`、clipboard sentinel bytes 未變，且動作後所有 repo 等於修改後的 pre-action snapshot。hold 在任何失敗路徑也由 `finally` 移除。兩個 `NativeSession` constructor 共用鎖，僅暫時設定所需環境鍵並在 `finally` 還原那些鍵；沒有 env 參數相容層或清空整個 process environment。
   - 負向案例必須截取完整的 `source-selected` 與 `result` 截圖，禁止空白截圖。
6. **生命週期與程序退出規範（Lifecycle & Process Termination）**：
   - 每步必須記錄兩個不同且有效的 App 身分（正整數 PID + starttime），在 App 存活期間與 Xvfb/dbus/xclip 控制器分開維護。空清單、缺欄位、null、0 或虛構 starttime 皆失敗。
   - sampler launcher exec 後、`WINDOW_READY` 時重新讀取 `comm` 與 `exe`；PID/starttime 必須未變，且 `/proc/PID/exe` 必須等於凍結 binary，報告記錄更新後的 App 身分。
   - 禁止以名稱殺行程（`killall` / `pkill`），禁止在正常測試退出時直接發送 SIGKILL。
   - 正常關閉使用 `stop_one`：檢查無過早退出，向聚焦之 App 視窗發送 `Ctrl+Q` 鍵盤事件，要求於 10 秒內正常回傳 exit code 0（逾時標記 `forced=True`，null exit code 絕不充作 0）。
   - 檢驗 log 中具備自按鍵後發出的 fresh 抽乾零任務證據（`phase=drained intent=quit jobs=0`）。
   - 在呼叫 `session.stop()` 收回測試專屬包裝行程**之前**，先觀察確認無任何 App 子行程殘留。
   - aggregate 與每個 App 都要求 `errors=[]`、`survivors=[]`、`appExit=0`、`forced=false`、`drained=true`；缺少證據或任何清理失敗均判失敗，不能以 aggregate 空清單掩蓋個別 App 錯誤。
   - 動作失敗或 Ctrl+Q 逾時時，在 teardown 前保存視窗、原焦點、截圖、程序 State/Threads/wchan 與 log 尾段；截圖會聚焦 App，報告明確記錄此動作。診斷失敗不得蓋掉原始錯誤，也不能把強制清理改判成功。
7. **嚴格驗證閘門（Gate Validation）**：
   - 驗證器綁定 manifest 規定的確切 18 個步驟 ID（`REQUIRED_STEP_IDS`）與各 ID 的正／負向 kind，拒絕偽造、替換或短少的步驟集合，也拒絕改 kind 來繞過負向要求。
   - 若步驟記錄中存在任何非空之 `failures` 清單，即使 `status == "passed"` 亦判為失敗。
   - 要求真實 64 字元十六進位 SHA-256 之 `binarySha256`、`datasetHash` 及剪貼簿/sentinel hash。
   - 正向 clipboard 的 source/readback hash 必須完全相等，只有長度正確不構成傳輸證據。
   - 負向步驟拒絕僅具 bare `prevention=True`：必須具備 64 字元 sentinel hash 且比對相符，或具備 64 字元 source hash 伴隨明確拒絕訊息或精確 payload oracle（`crossRepoPrevented` + `exactPayloadOracle`）。
   - 負向步驟要求 `compare.applied` 必須為 `equals-baseline` 或 `equals-pre-action-snapshot`。

每個步驟目錄留下：動作與 probe、來源 repo / kind / 完整 OID、clipboard 檔與 SHA、讀回 SHA、source-selected / preview / result / graph 截圖、binary 與 helper 與 fixture hash、before / pre-action / after snapshot、compare 結果、失敗原因、cleanup 的 pid 與 starttime。大 payload 只留檔案與 SHA，不印進 stdout。

報告路徑是 `<output>/report.json` 與 `<output>/report.md`。報告標示 `scope: linux-functional`，且在平台覆蓋、graph edge probe 或長照記憶體 gate 未完全滿足前，`acceptanceComplete` 保持 false。
