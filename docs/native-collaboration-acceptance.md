# Native 協作驗收 fixture

狀態：資料集可以產生並用 Git 驗證。這不是 native UI acceptance。

`scripts/collaboration_fixture.py` 建立兩台模擬機器的真實 Git 工作區與版本化 truth manifest。它不呼叫 snip-sync、不貼上、不驅動 UI。未來的 UI driver 可以讀 manifest；driver、真剪貼簿、真 OS input、截圖與套用證據都還沒有。沒有那些控制項與證據時，generate 或 verify 成功不得寫成協作驗收通過。

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

產生時的 baseline 只是生成當下的 truth。`neg-stale-source` 與 `neg-stale-target` 的不寫入證明，是故意改完之後、執行 Copy/export 或 Apply 之前立刻拍的 30-repo snapshot；動作後再拍一次，必須等於那張動作前快照。setup 造成的差異要另外留著，不能拿生成 baseline 當成動作前狀態。來源端若是釘住的 commit 內容，保留釘住的 bytes。既有 export contract 沒有保證每次 working tree 修改都會在 Copy 前被拒絕；那要由 driver 拿契約證據，fixture 不假設有跨機器 watcher。

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

- 沒有 UI driver，也沒有把 manifest 步驟套進 native 視窗。
- 沒有真剪貼簿、真輸入、截圖、目的快照或 binary 執行證據。
- 記憶體與 fd/thread/child gate 不在這支 fixture。
- 不修改 pinned TS、`fixtures/clipboard-contract.json`、標準效能資料集或產品程式。
