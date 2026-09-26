# Tauri 記憶體基線量測：pilot（Linux，2026-09-25）

這是一次 **pilot 量測檢查點**：不是 P0 驗收，也不是與原生版的效能比較。舊報告 `/tmp/snip-tauri-baseline-20260925` 已被拒絕，其中的數字一律不引用。

pilot 的環境條件：量測期間這台機器上還有其他 agent 同時在跑（CPU 有競爭）；Xvfb 使用軟體繪圖；D-Bus 是隔離的私有 bus，上面沒有 portal、keyring、a11y，也沒有 tray host。

## 結果（第 2 輪，每個 profile 各跑 10 次）

數字單位為 MiB，格式是 10 次結果的 median / p95 / worst。表中的「峰值」都是 50 ms 取樣中的最大值，而且只在標明的 phase 內取；**沒有任何一欄代表冷啟動峰值**。

| Profile | 成功 | 穩態 PSS 中位數 | 穩態 RSS 中位數（總和） | pre-ready 取樣峰值 RSS | steady 取樣峰值 RSS | app 主程序 VmHWM |
| --- | :-: | --- | --- | --- | --- | --- |
| idle | 10/10 | 257.45 / 259.21 / 259.39 | 476.15 / 477.92 / 478.12 | 483.40 / 484.91 / 485.05 | 483.23 / 484.69 / 484.83 | 182.07 / 182.28 / 182.34 |
| 1repo | 10/10 | 391.28 / 397.44 / 398.67 | 610.34 / 616.39 / 617.56 | 640.24 / 643.24 / 644.18 | 638.48 / 642.28 / 643.49 | 183.71 / 184.16 / 184.29 |
| 15repo | `UNSUPPORTED` | – | – | – | – | – |

- 量測範圍是 app 的程序樹：`snip-sync`、`WebKitNetworkProcess`、`WebKitWebProcess`，穩態時一律是 3 個程序。RSS 會把共享頁重複計入，所以跨程序相加時以 PSS 為準。另有一欄 `attachToEndSampledPeakRssMib`（涵蓋 pre-ready 加 steady）只寫在 JSON 裡。
- 計畫第 9 節的初始目標是 idle ≤ 100 MiB、1 repo ≤ 160 MiB，Tauri 在這個環境下遠高於這兩個值。
- 15repo：Tauri 只有一個 active repo（`App.tsx` 的 `const [repo, setRepo] = useState("")`，commands 也只收單一 `repo: String`），因此沒有可比的 15 repo 數字。**不宣稱原生版省下 30%**。
- 將來與原生版比較時，兩邊必須跑同樣的量測負載，否則必須揭露差異。舊 Tauri 的 1repo 會 render 第一頁 300 行 history；supervisor 指出目前原生版只顯示 25 行，這一點我沒有在原生程式碼中驗證。在負載對齊之前，任何差距都不能當作節省的證據。

## 來源與重建

- 最初的 20 次量測是在兩項修正**之前**產生的：一是「一律寫入 15repo `UNSUPPORTED`」，二是「cleanup 有問題就算 run 失敗」。所以原始 `tauri_baseline_report.json` 裡沒有 15repo 項目，舊格式裡的 `sampledPeakRssMib_attachToEnd` 也沒有更名。
- 這些原始檔案都保持原樣，**沒有改寫**。之後用 `python3 -B scripts/bench_tauri_memory.py --regenerate <out-dir>`，從每一次 run 的 `raw_samples.jsonl` 重新計算，另外寫出 `tauri_baseline_report.regenerated.{json,md}`。15repo 項目是在重建時才補上的，`provenance` 欄位有註明「沒有量測過任何東西」。
- 重建後的數字與原報告完全相同：pre-ready 峰值原本就是用 phase ≠ steady 的樣本算的，等同於 pre-ready。沒有數值錯誤，因此沒有重跑。
- 20 次的 `cleanupProblems` 都是 `[]`，所以在新規則下也都成立。
- evidence 留在 commit `a4b201f1843080ca3e9be9231960d5093e83dfec` 的 `docs/evidence/tauri-baseline-20260925-r2/`（此檢出不複製該目錄），內含原始報告、重建報告，以及 run-01 的截圖。原始取樣留在 `/tmp/snip-tauri-baseline-20260925-r2-{idle,1repo}/<profile>/run-NN/`。

## 指令

```bash
python3 -B -W error::ResourceWarning -m unittest discover -s scripts/tests   # 44 tests OK
scripts/measure_tauri.sh --workspace /tmp/snip-workload-standard-20260925 \
  --out-dir /tmp/snip-tauri-baseline-20260925-r2-idle  --profile idle  --runs 10 --steady-seconds 30   # exit 0（修正前）
scripts/measure_tauri.sh --workspace /tmp/snip-workload-standard-20260925 \
  --out-dir /tmp/snip-tauri-baseline-20260925-r2-1repo --profile 1repo --runs 10 --steady-seconds 30   # exit 0（修正前）
python3 -B scripts/bench_tauri_memory.py --regenerate /tmp/snip-tauri-baseline-20260925-r2-idle    # exit 0
python3 -B scripts/bench_tauri_memory.py --regenerate /tmp/snip-tauri-baseline-20260925-r2-1repo   # exit 0
scripts/measure_tauri.sh --workspace /tmp/snip-workload-standard-20260925 \
  --out-dir /tmp/snip-tauri-cleanup-check-20260925 --profile 1repo --runs 1 --steady-seconds 5       # exit 0（修正後）
```

修正後的短跑是用來驗證 teardown，**不是**新的基線數據；它的 steady 只有 5 秒。結果是 `COMPLETED`、`cleanupProblems: []`、binary sha256 與前兩輪相同；跑完後 `pgrep` 找不到任何殘留程序。這次短跑也是第一次產出逐程序樣本：最後一筆樣本中，WebKitWebProcess 的 PSS 約 295 MiB，app 主程序約 89 MiB，NetworkProcess 約 16 MiB。報告在 commit `a4b201f1843080ca3e9be9231960d5093e83dfec` 的 `docs/evidence/tauri-cleanup-check-20260925/`（此檢出不複製該目錄）。

## Metadata：設定值與實測值分開記錄

| 項目 | 值 | 來源 |
| --- | --- | --- |
| Binary | `target/release/snip-sync`，sha256 `b6eccfd0e11e38d86ba313a565e250b5e496483784d13a79f395869ca780a41a` | 三次 `measure_tauri.sh` 各自重建後都算出同一個 hash |
| Build | `bun run --cwd crates/desktop tauri build --no-bundle`，cargo `release` | `measure_tauri.sh` 原樣記錄的指令 |
| Source | HEAD `d42c739`；desktop/core 最後一次變更是 `9be8684`。dirty 的只有 `scripts/`、`docs/` | `git status --porcelain` |
| 視窗設定 | `tauri.conf.json`：main 視窗 1000×720，min 480×400，`visible:false`（blob `e160d728`） | `tauriConfigSource`：從原始檔讀取，屬於設定值，**不是觀測值**。binary 在 build 時就嵌入了設定，而本輪的 binary 正是從這棵 tree build 出來的 |
| 視窗觀測 | 1000×720，DPR 1，`visibilityState=visible`，window rect 1000×720 | `uiEnvironment`：steady 結束後從 live app 讀取 |
| 繪圖 | Xvfb `-screen 0 1280x800x24`；WebKit 程序實際載入了 Mesa `libgallium-25.2.8`、`libEGL_mesa`、`libGLX_mesa` | `/proc/<pid>/maps`；driver.log 顯示 `DRI3 error`，推定為軟體繪圖。WebGL 回報的 "Apple GPU" 是 WebKit 的遮罩值，未採用 |
| OS/硬體 | Ubuntu 24.04.5，kernel 7.0.0-31-generic，i7-12700；WebKitGTK 2.52.6 | `/etc/os-release`、`/proc/cpuinfo`、`pkg-config` |
| Dataset | `/tmp/snip-workload-standard-20260925`：rev `2026-09-25.1`，seed 42，15 repo，共 300000 commits | `workload_manifest.json` |

## 就緒前的斷言（任一項失敗，該次就算失敗）

- **idle**：URL 是 `tauri://`，`repo-path` 已經 render，`data-applied-path == ""`，畫面上沒有任何 `[data-commit]`。
- **1repo**：套用 `repo-01-core` 後，等 commits tab render 出 300 行。從畫面上依序往下，挑第一個含可驗證 A/M 文字檔、且不是 merge 的 commit（每次都選到 `69d6a8f6d868:README.md`），然後檢查：
  - `source-content.textContent` 必須**逐字等於** `git show <sha>:<path>`；拿到 null、空字串或內容不符都算失敗。這個 blob 只有 76 bytes、3 行，所以不代表驗證過大檔預覽。
  - diff pane（含 shadow DOM 的文字）必須包含 `git show --unified=0` 的每一行 `+`/`-`（此例共 2 行）。這是「包含」檢查，不是逐字比對。
  - 內容與 diff 各截一張圖，截圖都已人工確認。

## Phase 的定義

- `processAgeAtSamplerStartSec`：sampler 取第一筆樣本時 app 已經活了多久，本輪是 0.054–0.108 秒。
- `pre-ready`：從 sampler attach 到收到就緒標記，涵蓋 WebView 載入加上 UI 操作。`preReadySampledPeakRssMib` **只取這個 phase 的樣本**。
- `steady`：就緒後 30 秒內不做任何輸入。`steadySampledPeakRssMib` 只取這個 phase 的樣本。
- `attachToEndSampledPeakRssMib`：pre-ready 與 steady 兩段合併取最大值。
- 冷啟動峰值：**未量測**。app 生命最初約 0.06 秒沒有取樣，各次之間也沒有 drop page cache，所以 10 次都屬於暖啟動。

## 程序歸屬與清理

- app PID 必須是本次啟動的 `xvfb-run` → `dbus-run-session` → `tauri-driver` → `WebKitWebDriver` → app 這條鏈的子孫，而且 exe realpath 要完全等於受測 binary，符合者只能有一個。程式不做全域 `/proc` 掃描，也沒有 basename fallback。
- 身分以 `(pid, starttime)` 判斷，每一次取樣都重新驗證；PID 被重用或變成 zombie 都算失敗。
- `Driver.owned` 會在 session 建立、就緒、結束，以及 teardown 開始時，分別記錄 driver 樹下每一個 `(pid, starttime)`。teardown 的順序是：
  1. 刪除 WebDriver session。
  2. 對 driver process group 送 SIGTERM，再送 SIGKILL。
  3. 對所有記錄過的身分最多等 5 秒，確認它們消失。
  4. 還活著的補送 SIGKILL，**並記為 problem**；補送後仍在的也記錄。
- 只會對記錄過的身分送 signal，不在記錄內的程序（包括別的 session）一律不碰。
- **`run_profile` 在 teardown 之後才決定狀態**。只要有 error，或 `cleanupProblems` 不是空的，就記為 `FAILED`，但已收集的 measurement、截圖路徑與診斷資料都保留。`summarize` 只統計 `COMPLETED` 的 run，另附 `failedRuns`；只要有任何 run 失敗，整個指令就 exit 1。bench 收到 SIGTERM 時也會先跑完 teardown 再結束。
- controller 另外列出（約 39–40 MiB PSS），不算進 app。app 視窗的 X pixmap 存在 Xvfb 裡，這部分歸在 controller。

## 設定隔離

- 每一次 run 都使用全新的 `XDG_{CONFIG,DATA,CACHE,STATE}_HOME`，結束後刪除。
- 使用私有 D-Bus，而且**關閉 service activation**。原因：用預設設定時，app 會觸發 `xdg-desktop-portal`，portal 接著嘗試啟動 `org.freedesktop.secrets` 而每次卡住 25 秒 timeout，還會順帶拉起 gvfs、a11y 等桌面服務。代價是這個環境和真實桌面不同。
- `single_instance` 也登記在這個私有 bus 上，不會和使用者自己開著的 snip-sync 衝突。
- repo 路徑本來就不會持久化；idle 斷言仍會明確檢查 `data-applied-path == ""`。

## 限制

- 只量了 Linux、Xvfb、軟體繪圖，而且機器有 CPU 競爭。macOS 與 Windows 的指標都沒有量，也不可拿這裡的數字互相相除比較。
- 只有暖啟動，沒有冷啟動量測；沒有 100 次切換 soak；沒有量 UI 延遲與 idle CPU。
- GPU、DMA-BUF，以及 X server 端的表面記憶體，都不在 `/proc` 的 RSS/PSS 之內。
- 1repo 的穩態在不同次之間有 540–618 MiB 的 RSS 差異（例如 run-07），原因沒有分析。那 20 次跑的時候還沒有逐程序取樣，現在已有 `perProcess` 欄位，下一輪可以用它歸因；不會只為了解釋這個差異而重跑 20 次。
- diff 斷言是「包含」檢查；oracle 只驗證一個 commit 裡的一個小檔。
- 1repo 情境只涵蓋「一個 repo、第一頁 300 行 history、單檔 content/diff」，不代表計畫中 15 repo 的活躍工作情境。

## 仍待完成的 P0 量測工作

1. 原生版在同一份 15 repo 資料集、同一台機器、release build 下量測。量測負載要與 Tauri 對齊（history 行數、預覽內容一致），否則必須明確揭露差異。
2. 原生版的 15 repo 概覽與活躍 graph/diff 情境要量測。Tauri 對應項目維持 `UNSUPPORTED`，比較時要說明。
3. 冷啟動量測（drop cache 或開機後第一次）與暖啟動分開；每個 profile 各 10 次，報 median / p95 / worst。
4. 100 次 repo／預覽切換 soak、UI 延遲 p95、idle CPU。
5. 用 `perProcess` 做逐程序歸因，並在沒有 CPU 競爭的固定環境重跑。
6. macOS physical footprint、Windows private working set/bytes。
7. 以上完成、預算門檻通過之後，才能評估「30% 下降」是否成立。
