# Linux native resource-leak gate

檢查名稱：`native-resource-leaks`

這個 gate 看暖機之後、等效 GitChanges 畫面靜止時，app 程序樹有沒有留住 RSS／PSS、fd、thread 或 inotify watch。它不證明永遠沒有洩漏，不通過 D4，也不宣告產品 release。`NativeSession.stop` 只證明 harness 強制回收了自己擁有的程序，不是產品的 graceful quit。

切換使用凍結 driver 的 `click_repo`（目前籃子為空才點）。100 次實測是同一條 round-robin 切成每批 10 次，所以 15 個 repo 都會出現。`run_soak` 每次呼叫都把索引重頭算，不用來計這幾批。

## 命令

短閘與長閘都必須釘 binary、fixture、source。缺任何一顆就在起動前拒絕。

```bash
python3 -B scripts/check_native_leaks.py \
  --profile short \
  --bin "$BIN" \
  --expected-binary-sha256 "$BIN_SHA" \
  --workspace "$FIXTURE" \
  --expected-fixture-sha256 "$FIXTURE_SHA" \
  --expected-source-sha "$SOURCE_SHA" \
  --out-dir "$OUT"
```

`$OUT` 必須是空目錄。契約檔是 `$OUT/native_resource_leaks.json`（`schemaVersion` 1）。`$OUT/raw_samples.jsonl` 是唯一的量測來源：每一行都是同一個 `runId` 的 JSON，`seq` 從 0 連續遞增，`tMono` 不倒退。暖機與實測 action、互動、資源樣本、剪貼簿與長閘窗都在這條流裡。判決用這條流重算 RSS／PSS／fd／thread／inotify／Git 子程序／完整性／身分／順序／等效狀態。報告裡的複本若跟這條流不一致，是 `raw-divergent`。缺欄、重複、壞掉、截斷、錯的 run、或執行檔變成 ` (deleted)` 都不能接受。

長閘把 `--profile` 換成 `long`。在 hide 與 tray 仍沒有契約時，命令會直接 `missing-coverage` 並且不啟動 600 秒浸泡。workspace 必須是 generator `PRESETS["standard"]` 的 15 repo：每個 repo `filesPerRepo=10000`、`commitsPerRepo=20000`、`refsPerRepo=100`，manifest 的 parameters、summary 與每個 repo 的 `commitCount`／`trackedPathsCount`／`refCount` 都要相符，而且 `git rev-list --count` 要等於 manifest。只有 `preset: "standard"` 或總量大於一萬不算。

建議的 just／CI 名稱是 `native-resource-leaks`。本變更不改 justfile、CI 或 release。

退出碼：0 是 short 的 `SUBGATE_ACCEPTED`，或 long 在覆蓋與視窗都齊時的 `LEAK_GATE_ACCEPTED`。1 是 `NOT_ACCEPTED`。2 是參數或非空輸出目錄。`LEAK_GATE_ACCEPTED` 仍把 `d4` 留在 `NOT_EVALUATED`、`releaseComplete` 與 `productAcceptance` 留在 false。收據不能把 `sourceBuildAuthorized` 設成 true。

## 門檻

暖機不進趨勢。每一批的終點是該批最後一筆樣本；前面一筆完整樣本不能把後面的 busy 或不完整終點濾掉。

- RSS 與 PSS：後三分之一中位數減前三分之一 `<= max(32 MiB, 基線 10%)`，而且斜率 `<= 0.25 MiB／次`。
- fd `<= 2`，thread `<= 4`。
- inotify watch 成長 `<= 0`，而且只在同一組靜止的 repo／view／開關狀態上比較。不同的合法 watch 集合不是洩漏。讀不到 fdinfo 是 null，不是 0。
- 工作區分頁序列：同一個 repo 用「+」加輸入路徑開進新分頁、等它載入、Ctrl+W 關掉，反覆 N 次（short 與 long 都是暖機 5 次、實測 30 次，每 5 次一個終點，`--warmup-tab-cycles`／`--measured-tab-cycles` 只能往上調）。每批結束回到同一個 canonical repo／GitChanges 再 settle。這條序列另外判，不混進 repo 切換的趨勢：RSS／PSS、fd、thread、inotify watch 用上面同樣的門檻，斜率改成每開關一次。每次開關都要回到一個分頁，關掉的分頁自己的收尾要排空。
- GPU 不估計，也不加進 RSS／PSS。`vramBytes` 維持 null。內部 task 讀不到就維持 null，不可以填 0。

根身分是 pid + starttime + 執行檔 realpath，而且必須等於這次選中的 binary realpath。product run 的報告一定要有 app pid、starttime、exe。每個 action、樣本、基線窗與結尾窗都對這組身分。`/proc/<pid>/exe` 出現 ` (deleted)` 是失敗。子程序的 task／children 讀不到時，Git 子程序是 null，不是 0，這筆資源樣本不完整。fd／thread／watch 讀完才再讀身分，根程序也要再讀一次。

long 還要：實測至少 500、暖機 20、至少 10 個檢查點、每次 settle 至少 3 秒、觀察至少 600 秒、30 秒基線窗與 30 秒結尾窗。結尾窗在所有 action 與互動之後；只看 `sampleOrder` 不夠。窗內每一筆都要有 fd／thread／watch／Git 子程序／身分／等效狀態。未知或讀不到不能算過。一筆幸運樣本不能代表 30 秒。

## 覆蓋

short 子閘要求 9 項互動的完整覆蓋：repo 切換與 history 的新鮮日誌（`REPO_SELECTING`、`REPO_LOADED` 檔案數等於 source-row oracle、`GRAPH_LOADED`），加上 tree（`TREE_FILE_SELECTED`、`TREE_EXPANDED` 或 `TREE_TOGGLED`）、copy（`copy_explicit_selection` 驗證 oracle）、paste（`PASTE_PREVIEW`）、cancel（`PASTE_CANCELLED`）、workspace 關閉再開啟（`btn-workspace-menu`、`btn-close-workspace`、`[APP:WORKSPACE: state=closed]`、drained 驗證、`btn-open-workspace`、`workspace-path-input`、`btn-workspace-open-confirm`、`[APP:WORKSPACE: state=open]`、`READY_REPOS` 相符、PID+starttime 同一程序、剪貼簿 sentinel 保留、Git 子程序排空）、工作區分頁開關（`ws-tab-new`、`[APP:WS_TAB_OPENED: … count=2]`、輸入路徑開啟、`Ctrl+W`、`[APP:WS_TAB_CLOSED: … count=1]`、該分頁的 close drained，次數等於暖機加實測）與 graceful quit（`ctrl+q` 觸發 `[APP:QUIT: deferred]`、退出碼 0、無存活 app 子程序）。任何嘗試的操作失敗或缺漏皆拒絕（`missing-coverage`）。

隔離顯示上的剪貼簿在取樣前放進固定 payload。收尾要嘛仍是同一份內容，要嘛明文標出 copy 留下的 payload。不可以把剪貼簿清成空的來過記憶體預算。workspace close 與 reopen 必須保留剪貼簿。

分頁開關序列接在 repo 切換的實測之後；tree、copy（右鍵「複製」一個變更列）、paste、cancel 與 workspace close/reopen 都在終點資源窗之前完成。清完回到同一個 canonical repo／GitChanges，settle 之後才寫終點樣本。長閘的 30 秒結尾窗也在這些操作之後，並且落在觀察的最後 30 秒。終點資源取樣完成後才送出 graceful quit。

long 仍要求 600 秒浸泡、500 次切換、30 秒基線與結尾窗，以及包含 hide 與 tray 的完整 release 覆蓋（11 項）。在 hide 與 tray 產品契約尚未就緒前，long 誠實回傳 `NOT_ACCEPTED`（exit code 1，`missing-coverage`），不跳過也不做整體產品或 D4 認證。

## 測試

```bash
SNIP_REQUIRE_ALL_TESTS=1 python3 -B -W error::ResourceWarning -m unittest scripts.tests.test_native_leaks scripts.tests.test_bench_native
SNIP_REQUIRE_ALL_TESTS=1 python3 -B -W error::ResourceWarning -m unittest discover -s scripts/tests
```

## Current entrypoints

Use `just native-resources-short` for the required Linux functional-short gate
or `just native-resources-long` for the unchanged standard-workload long gate.
The long gate currently fails for missing observed hide/tray coverage. Neither
a short pass nor an entrypoint build establishes full D4 or release acceptance.
Fresh outputs, interpreter selection, source/build receipts and reuse of a
frozen current binary are documented in [native-ci-integration.md](native-ci-integration.md#current-linux-acceptance-entrypoints-2026-09-27).
