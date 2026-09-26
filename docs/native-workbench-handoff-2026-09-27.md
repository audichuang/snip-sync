# Native workbench 接手待辦清單 — 2026-09-27

目前由使用者接手執行；Codex 停止新增派工。本輪四個 AGY 工作均已結束，沒有本輪仍在執行的 AGY 任務。以下將已驗證結果、已找到的缺口、以及仍需驗收的既定產品範圍分開；「待驗收」不表示該功能完全沒有實作。

> 🧠 **From Hindsight memory (Initiatives and enhancements)** — 既定交付範圍仍包含 D0–D5、15 repo、多機雙向剪貼簿同步、記憶體與真實 UI 驗收；UI 或局部測試通過不等於可以發版。以下當前狀態以 2026-09-27 的工作樹、程式碼與測試紀錄為準。

## 1. 接手基準與已完成事項

- 主工作目錄：`/home/audichuang/research/snip-sync`
- 分支：`feature/lightweight-git-workbench-plan`
- HEAD：`9ecc8d7bc432b8a12f8a67730829996330b657c2`
- 原有三個 commit：`cdcb525`、`734fd93`、`9ecc8d7` 保留。
- 本輪沒有新增 commit、push、PR 或 release。
- 接手清單建立前共有 13 個 tracked modified files、6 個 untracked files；本文件另外新增一個 untracked file。新增的核心／native 工作仍未提交。
- 請保留 dirty 工作樹；不要 reset、clean，或用其他 worktree 的整份檔案覆蓋主目錄。

| 範圍 | 現況與證據 | 尚不能宣稱 |
| --- | --- | --- |
| UI 與 Linux X11 中文 IME | 已有三個 commit。本輪 lifecycle 整合後重跑 IME：exit 0、graceful exit true | Wayland／macOS／Windows IME 通過；本輪新截圖已人工逐張看過 |
| 有界、可取消的 file／commit export core | 已整合主工作樹；9 個檔案與接受快照逐一 SHA256 相同 | 已完成全部 native UI 接線／D2–D5 |
| 主工作樹 core 測試 | 獨立驗證 338 runtime tests passed、1 runtime ignored；1 doctest passed、1 illustrative doctest ignored。另明確執行被忽略的標準 workload discovery test，1 passed | 正常測試命令零忽略；15 repo 完整 UI 已通過 |
| Native lifecycle／Copy 取消 | 已實作並整合；獨立驗證 67 unit + 3 real-OS lifecycle tests 通過 | 所有背景工作都已納管；詳見第一優先缺口 |
| Native smoke | AGY 報告本輪 5/5 通過；本輪 Codex 獨立重跑的是上述 67+3 與 IME | 把 worker 自測標成 Codex 獨立驗證 |
| Read-only paste-preview core 取消 | 隔離 worktree 已修正並獨立驗證 68 個相關測試；3-file patch 已備好，主工作樹 apply-check 通過 | 已合入主工作樹；native 貼上預覽已非同步可取消 |

## 2. 第一批：先補目前已知缺口

### [ ] 1. 保存接手證據與工作樹

先確認 root／branch／HEAD，備份 tracked diff 與 untracked 新檔。單純 `git diff` 不包含新檔。將下節列出的 `/tmp` patch、receipt、測試 log 複製到穩定位置；它們目前尚未成為 Git 裡的交付物。

本輪新增的六個產品／測試檔案：

- `crates/core/tests/commit_export.rs`
- `crates/core/tests/commit_export_exact.rs`
- `crates/core/tests/git_open.rs`
- `crates/core/tests/transfer_cancel.rs`
- `crates/desktop-native/src/lifecycle.rs`
- `crates/desktop-native/tests/lifecycle.rs`

### [ ] 2. 修正目錄樹背景工作沒有納入 lifecycle 等待

**已確認的第一優先問題。** 主工作樹 `crates/desktop-native/src/main.rs` 的 `submit_tree_io` 約第 1631 行，仍以 `cx.spawn` + `self.tree_task = Some(task)` 啟動工作，內部再啟動 background executor 執行 `execute_tree_io`。它沒有登錄到目前 lifecycle 的 owned jobs。

結果是：當沒有 Git child，lifecycle 可能在目錄讀取仍進行時認為已 drained。丟棄外層 future／task handle，也不能證明背景 I/O 已結束。因此現有 3 個 lifecycle tests 通過，仍不足以接受整個 lifecycle。

要完成的行為：

- 所有 tree I/O 在真正結束前都計入 lifecycle；沿用既有 ownership 機制，確認計數涵蓋實際 background work。
- `dispatch_tree`／`submit_tree_io` 在 draining／closed 時拒絕新工作；queue 清理與取消一致。
- Close／Open／Quit 取消 read-only 工作並等待清理；舊 worker 結果不得寫入已关闭或重新開啟的 workspace，即使重新開的是同一路徑。
- 檢查 worker id、workspace generation、cancel token 在 close/reopen 的交界，避免只檢查相同 base path。
- 查明 `tree_task`、`discovery_task`、`add_repo_task` 舊欄位是否仍有作用，只移除確定不再使用的 handle。

最小回歸驗收：刻意阻塞「沒有 Git child 的真 tree background operation」，再要求 Close／Quit；阻塞解除前不得宣告 drained，解除後才可完成。相同 PID reopen 後舊結果不能更新畫面，queue／retained capacity 釋放。沿用真實 OS 事件與 app 報出的控制項位置。

### [ ] 3. 補 Copy 取消的真實 UI 驗收

目前 3 個 lifecycle scenarios 是：close/reopen 丟棄舊 preview、Quit 等待被卡住的 Git child、Close 取消進行中的 file Copy。仍缺：

- 真實點擊新 `btn-copy-cancel`，驗證取消後 clipboard sentinel 不變、busy 結束、工作與子程序確實結束。
- Commit Copy 進行中取消，驗證不發布部分 payload，不留下 Git child。
- Close／Quit 對 file 與 commit copy 的共用取消行為，確認沒有只修到其中一條。
- 已確認 Apply 正在寫入時，Close／Open／Quit 被拒絕並顯示原因；等待寫入完成與呈現結果，不中途取消已確認的寫入。

### [ ] 4. 合入已驗證的貼上預覽 core patch

使用現成 patch，不重寫已驗證的實作：

- Patch：`/tmp/snip-preview-cancellation-20260927.patch`
- Patch SHA256：`50c18f9a7fa147176279bfde790174d876aaa58e9885b328b5fa95bf5082fb3b`
- Receipt：`/tmp/snip-preview-cancellation-accepted-20260927.json`
- 來源 worktree：`/home/audichuang/.codex/worktrees/snip-linear-commit-export/snip-sync`
- 來源 branch／base：`feature/linear-commit-export`／`c05658eb109843dc9cd4a57dae8827ca5b56c4c6`
- 僅修改 `crates/core/src/commits.rs`、`crates/core/src/transfer.rs`、`crates/core/tests/transfer_cancel.rs`。

Patch 已包含 review 發現的最後取消檢查：payload clone 後、revalidation comparison 後，都不能在取消已發生時回傳可用 plan。已有 pre-cancel／in-flight／final-boundary 回歸測試。

接手時重新驗證 patch hash、`git apply --check`，再套用；套用後核對 receipt 的三個檔案 hash，跑相關與完整 core tests。未來若主工作樹先修改這三檔，應做語意整合，不硬覆蓋。

### [ ] 5. 將 native 貼上預覽接上非同步取消流程

目前 `main.rs` 的 `trigger_paste_preview`、`choose_paste_keep`、`choose_paste_prefix`，以及 `paste.rs` 的 `build_from_clipboard_text`、`set_prefix_destination`、`set_keep_relative` 還需共同追蹤與接線。不要只改第一次打開貼上面板的入口。

- 使用 core 的 `plan_import_with`、`CommitReplayPreview::capture_with`／`revalidate_with`，沿用 `RunOptions`／cancel token。
- 初次預覽及 mapping／destination 變更引起的重算都放入 owned background jobs；UI 保持可操作。
- 新貼上開始時立刻作廢舊 apply plan；loading 或失敗時不可 Apply 舊計画。
- Escape／Cancel／新貼上／workspace close 或切換取消舊 read-only 工作；晚到的結果不得重新打開已取消的 preview。
- 取消不寫入；stale source／target 回報清楚，不能偷偷重新規劃後直接寫入。
- `apply_paste_restore`／`PastePreviewPlan::execute` 等已確認寫入保持不可中途取消，並納入 mutating lifecycle jobs。

驗收至少涵蓋 file preview、commit preview、mapping 重算、取消、快速連續貼上、close/reopen、Apply 執行中要求退出；檢查目的 files／HEAD／index／refs 与 clipboard。

## 3. 第二批：完整功能與雙機驗收

### [ ] 6. 補 fixed-OID 歷史檔案加入共用 selection basket

目前欠缺歷史檔案加入 basket 的完整 UI 操作與驗收。Basket 保存 root／source／revision／path，revision 固定 OID，不預存所有內容。操作不 checkout；同檔 staged／working／歷史來源不得混淆。

驗收 non-HEAD／unmerged branch 檔案 exact bytes、多 repo、同路徑不同來源的明確處理、切面板保留選取。Commit replay 仍只接受同 repo、連續 first-parent 範圍；歷史 file 選取與可 replay commit 範圍分開。

### [ ] 7. 按既有 D3 規格逐項查缺與驗收

先用現有實作、tests、probes；有證據即記錄通過，缺功能才補：

- Workspace：15 repo、nested repo、submodule、linked worktree、detached／unborn、symlink／Unicode／空白／同 basename 身份；錯誤不冒充 clean。
- 來源：staged A／working B、untracked／conflict、delete／rename；預覽與 clipboard bytes 一致。
- Graph：真 parent、root／merge／octopus／shallow、分頁 lane 穩定、refs、SHA/message/author 搜尋、HEAD、鍵盤導航、range、collapse／expand、compare。
- Reader：working 與任意 commit、刪除前內容、大檔／binary／超長行有界退化、inline／side-by-side diff、搜尋、跳行、文字選取複製。
- Transfer：明確 mapping、碰撞／越界拒絕、overwrite 預設關閉、skip 原因、partial failure 呈現。
- Freshness：source／target HEAD、ref、index、內容與「不存在」狀態；preview 後新建／刪除／替換檔案使 plan 失效；原本被跳過的來源發生變化也要按既定語意處理。
- 不改 immutable `fixtures/clipboard-contract.json`；File mode 以 pinned TS reference／fixture 的 byte parity 為準。

### [ ] 8. 跑兩個模擬電腦 × 各 15 repo 的完整真實 UI 驗收

沿用 `scripts/collaboration_fixture.py`、`tests/test_collaboration_fixture.py` 與 `docs/native-collaboration-acceptance.md`，不要重造 fixture。已有 fixture／部分 pilot 證據不代表完整驗收通過。

- A、B 各 15 個獨立 repo；真 UI 操作、隔離 display／DBus／clipboard，透過明確 clipboard bridge 傳遞。
- 依 manifest 跑完整 18 cases（9 positive、9 negative）；每個 step 從要求的乾淨快照開始，保留 machine／repo mapping 與 log。
- A→B、B→A 的 file 與 commit 路徑都執行；用獨立 Git／filesystem oracle 比對 exact bytes、tree、author name/email/time、完整 message。
- 驗證未選檔、ignored files、HEAD／index／refs 的允許或不允許變動；不把 commit SHA 相同當 replay 成功條件。
- Negative cases 包含非法 commit 集合、錯誤／模糊 mapping、collision、missing destination、stale source／target、未授權覆寫、Cancel。
- Stale 測試的 no-write 基準在刻意造成 stale 的變更之後建立，避免把測試本身的變更錯算成 app 寫入。
- Fixture generator／core tests 通過不能代替 native real-UI cases。

## 4. 第三批：記憶體、資源與桌面品質

### [ ] 9. 驗證並補齊有界資料與背景工作

- Retained-data 初始總預算 64 MiB：preview 32、graph 16、tree 8、highlight 8。
- 核對實際 retained capacity／bytes，包含 strings、vectors、maps、caches；只虛擬化可見 rows 不代表資料有界。
- 只有 active 詳細視圖保留重資料；大量切 repo／preview 後回到預算。
- 全域最多 2 個 Git child，per-worktree 重工作序列化，queue 有界；真正清理完成才釋放 permit。
- Cancellation／timeout／overflow／Git failure 分開；不截斷後回報成功。
- Watcher 共用、debounce、bounded refresh；hidden／tray 停止非必要輪詢；close 回收 watcher／task／child／cache。

### [ ] 10. 跑正式 release-profile 記憶體與效能驗收

沿用 `scripts/bench_native_memory.py`、`scripts/memory_harness.py`、`scripts/bench_tauri_memory.py`、`scripts/workload_generator.py`；依 `docs/memory-measurement-protocol.md`、`docs/native-perf-harness.md` 及原規劃第 9 節。

標準 workload：15 repo，每 repo 10,000 paths／20,000 commits／100 refs；功能 fixture 與正式效能 fixture 用途不同。

| 指標 | 初始門檻，尚待正式驗證 |
| --- | --- |
| Idle | ≤100 MiB |
| 1 repo | ≤160 MiB |
| 15 repo overview | ≤256 MiB，且比 1 repo 增量 ≤96 MiB |
| 15 repo active | ≤384 MiB |
| 預設 copy／paste peak | ≤512 MiB |
| 可比 active baseline 改善 | Native 比 Tauri 至少下降 30% |
| Warm selection p95 | <100 ms |
| 一般檔案首次預覽 | <300 ms |
| Graph scroll p95 frame time | <20 ms |

- 同硬體、尺寸、release profile 與可比場景；Tauri 場景能力不足時明示差異，不能捏造可比 15 repo 數據。
- Ready 後穩定等待與有效採樣依 protocol，10 次冷／暖量測，報 median／p95／worst，另跑 100 次切換 soak。
- Cold Git latency 與 warm UI latency 分開；idle CPU 依原規劃。
- Linux PSS／RSS、macOS physical footprint、Windows private working set／bytes 分開報，不混用。
- 附 source／binary／dataset hash、OS、process tree、readiness 與 raw samples。先前遭拒的 debug／release 混用 baseline 不能復用為通過證據。

### [ ] 11. 完成會抓出失敗的 resource-leak gate

既有候選工作在 `/home/audichuang/.codex/worktrees/snip-resource-leak-gate/snip-sync`；先審查再整合，不把歷史 worker 完成訊息當已通過。

- 真 app warmup 後執行 switch／preview／Copy／Cancel／Close／同 PID Reopen。
- 量 process-tree RSS/PSS、threads、fd、inotify watch 數、owned jobs、Git children；watch count 與 fd count 是不同指標。
- 用 PID + starttime 防 PID 重用；最後一次操作後再等待穩定，不提早取最後樣本。
- Force kill 只可做失敗清理，不能算 graceful exit。未支援／缺資料是 null 或 failure，不填 0。
- 故意注入 memory／fd／thread／child leak、無 UI、採樣過短、binary／SHA／fixture 錯誤、缺報告、timeout，gate 必須 nonzero。

### [ ] 12. 補桌面操作、跨平台與 IME 驗收

- 1080×720 與 900×600，繁中／英文、長路徑／branch、splitter、收合還原、可见焦點與合理 Tab 順序。
- 檢查 Linux／Windows tray／hide 是否真正實作；驗證 hidden idle 與 reopen，不能只存在按鈕。
- 真正的 accessibility labels／keyboard navigation／scaling 驗收。
- Linux Wayland、macOS、Windows 分別實測 IME、clipboard、rendering、focus、lifecycle、package 安裝／啟動。
- Linux X11 IME 已通過；保留 GPUI patch 與 `vendor/gpui/SNIP_PATCH.md`，cross-compile 通過不代表其他平台 runtime 通過。
- 本輪 IME 截圖仍可人工確認；重跑腳本需使用新的輸出目錄，預設固定 `/tmp/snip-native-ime-fix-20260926` 會刪除舊證據。

## 5. 第四批：把驗收變成 gate，整理與交付

### [ ] 13. 修正 preflight／CI 漏跑 lifecycle tests

**目前已確認：** `just preflight` 依賴 `preflight-rust preflight-frontend desktop-e2e preflight-harness native-smoke`；Rust recipe 對 native 只跑 `--bin`，native-smoke 只跑 `--test smoke`。`.github/workflows/ci.yml` 也如此。因此新增的 `tests/lifecycle.rs` 目前不會被這些 required gates 自動跑到。

- 把 Linux lifecycle tests 納入 preflight 與 CI required gate，沿用 headless X11 的既有環境。
- 為已要求的 IME／collaboration／resource acceptance 配置明確可執行入口與適當平台 gate，保存 artifacts。
- 必需工具／display／reference 缺失不得默默通過；遵守 `SNIP_REQUIRE_ALL_TESTS`。
- Core ignored standard-workload test 目前硬綁 `/tmp/snip-workload-standard-20260925`：改成可重現的 required workload gate，或明確配置專用驗收入口，不能報「零略過」。
- 保留原本 Tauri 全部 E2E scenarios、clipboard contract、audit、DTO drift、clean checkout、Windows／macOS checks。

### [ ] 14. 更新文件與證據，再提交及交付

- 修正 `docs/native-workbench-progress.md`／`docs/native-workbench-supervision.md` 的本輪記錄：core 334 是舊數字；本輪主工作樹獨立結果是 338+另跑 1；lifecycle 還有 tree ownership 缺口。
- 分清 worker self-test 與 Codex／接手者的獨立驗證；測試數量隨後續修改更新，不沿用本清單舊計數。
- 每個 acceptance receipt 綁定最終 source／binary／dataset；dirty tree 附 patch/hash，提交後記 exact commit。
- 完整執行 `just preflight`，修完再重跑必要檢查。本輪尚未跑完整 `just preflight`；`preflight-rust` 不可代替。
- 審查後做分段 checkpoint commits；push 前必跑 preflight，feature PR 目標為 `develop`。
- CI matrix 與 required gates 全綠後才進入整合／release 流程。Release 是 `develop` → `main` PR，然後在 main 執行 `just release X.Y.Z`；不是每個中間 checkpoint 都發版。
- Native package promotion 使用 CI 已驗證的同一 binary／artifact；same source SHA 重建的另一顆 binary 不等於同一份驗收。簽章或封裝改變 executable identity 時按規格驗證最終 artifact。
- 保留既有 Tauri／CLI artifacts，核對 Linux x64、macOS arm64／x64、Windows x64 package 與 installer 行為。

## 6. 可直接使用的本輪證據

| 用途 | 路徑 |
| --- | --- |
| 已接受 export core 快照 | `/tmp/snip-shared-core-export-accepted-20260926/receipt.json` |
| 主工作樹 core 獨立驗收 receipt | `/tmp/snip-core-integration-independent-20260927.json` |
| 主工作樹 core 獨立 log | `/tmp/snip-core-integration-independent-20260927.log` |
| Standard workload discovery 明確執行 log | `/tmp/snip-standard-workload-independent-20260927.log` |
| Preview patch／receipt | `/tmp/snip-preview-cancellation-20260927.patch`、`/tmp/snip-preview-cancellation-accepted-20260927.json` |
| Preview 68 tests 獨立 log | `/tmp/snip-preview-independent-20260927.log` |
| Preview 修正前 regression failure | `/tmp/snip-preview-pre-fix-failure.log` |
| Preview review／fix 報告 | `/tmp/snip-agy-preview-review-20260927-report.md`、`/tmp/snip-agy-preview-fix-20260927-report.md` |
| Native lifecycle worker 報告 | `/tmp/snip-agy-native-lifecycle-integration-20260927-report.md` |
| Native 67+3 獨立 log | `/tmp/snip-native-lifecycle-independent-20260927.log` |
| X11 IME 結果與 screenshots | `/tmp/snip-ime-lifecycle-independent-20260927/result.json` 及同目錄 PNG |

本輪 IME 使用的 debug binary SHA256：`b7a77c5798c2d980de59c76ea408916c59de4b0aaed92cccbb2b2dc133fceff6`。後續 rebuild 必須重新計算，不能沿用。

可重跑的目前 Linux 檢查（在主工作目錄執行；各命令獨立）：

```bash
rtk proxy env LIBRARY_PATH=/home/audichuang/.local/lib SNIP_REQUIRE_ALL_TESTS=1 scripts/headless-x11.sh cargo test -p snip-core --locked
rtk proxy env LIBRARY_PATH=/home/audichuang/.local/lib SNIP_REQUIRE_ALL_TESTS=1 scripts/headless-x11.sh cargo test -p snip-desktop-native --locked
rtk proxy env LIBRARY_PATH=/home/audichuang/.local/lib just native-smoke
rtk proxy env LIBRARY_PATH=/home/audichuang/.local/lib just preflight
```

`LIBRARY_PATH` 是本機 xkbcommon-x11 連結環境，不要寫成其他平台的固定 repo 設定。IME 腳本本機用 `uv run --with pillow`，無需系統安裝 Pillow。

## 7. 既有候選 worktree，接手前先盤點

以下是可沿用的候選成果，不表示全部已接受；不要整批複製／cherry-pick 未審查的 dirty worktree：

- Preview core：`/home/audichuang/.codex/worktrees/snip-linear-commit-export/snip-sync`，本輪接受範圍已封成上述三檔 patch。
- Lifecycle 歷史來源：`/home/audichuang/.codex/worktrees/snip-native-lifecycle/snip-sync`；本輪以主工作樹整合版本為準。
- Memory harness：`/home/audichuang/.codex/worktrees/snip-memory-harness/snip-sync`。
- Resource leak gate：`/home/audichuang/.codex/worktrees/snip-resource-leak-gate/snip-sync`。
- Collaboration fixture：`/home/audichuang/.codex/worktrees/snip-collaboration-fixture/snip-sync`。
- Packaging：`/home/audichuang/.codex/worktrees/snip-native-packaging/snip-sync`。

建議回交批次：先完成 1–5，附 diff／相關 tests／lifecycle 與 Copy、Paste 真 UI 證據，再處理 6–8、9–12、13–14。第 13 項的既有 lifecycle gate 漏洞可在第一批就修；其餘 gate 隨對應驗收完成接入。
