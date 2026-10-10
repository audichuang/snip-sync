# Linux 記憶體量測協議與基準作業規範 (Memory Measurement Protocol)

> 註:Tauri 版(`crates/desktop`)與其 driver(`bench_tauri_memory.py`、`measure_tauri.sh`、Tauri E2E)已從 repo 移除;本文提到它們建置、測試或量測的段落是當時的紀錄,回退請取 git 歷史。

日期：2026-09-26
版本：`2026-09-26.2`（取樣身分失敗即關閉；launch 僅在驗證過的執行前閘道加上觀察到的 exe 轉換；不自動產出發布比對）
依據：[`docs/native-git-workbench-plan.md`](native-git-workbench-plan.md) 第 9 節（記憶體預算與可量測指標）及第 10 節（更嚴格的 CI／E2E）。

---

## 1. 目的與核心原則

Rust 具備編譯期記憶體安全，但無法阻止無界集合、快取未淘汰、子程序堆積或 GPU 表面記憶體膨脹。**框架選型與產品實際常駐記憶體必須具備獨立、誠實的驗證機制。**

本協議定義 Linux 環境下針對原生 Git 工作台（GPUI 原型）與基線系統的量測標準、資料集生成規範、取樣口徑及報表格式。

### 核心原則

1. **誠實口徑 (Provenance & Honesty)**：指標來源、單位與取得途徑透明揭露；不可測或未測項目明確標示，不得以偽造或替代數據宣稱達標。
2. **程序樹取樣 (Process Tree Accounting)**：涵蓋主應用程式及其衍生之子程序。取樣受限於離散取樣點（nominal ~50ms），代表各採樣時點所觀察到的並行記憶體。外部包裝環境（如 `Xvfb`、`dbus-run-session`、`dbus-daemon`）嚴格排除於應用程式程序樹 RAM 之外。
3. **區分啟動峰值與穩態 (Phase Separation)**：測試座親自 spawn、且未指定預期執行檔時，就緒標記前為 launch。Attach 預設為 pre-ready。只有取樣閘道發布時 `/proc/<pid>/exe` 仍不是預期目標、且後續取樣觀察到該 exe 變成目標，就緒前才可標為 launch。`launcher-setup` 與 exe 跨取樣改變或讀不到的 tick 不計入目標峰值。就緒後需連續採集 ≥30 秒穩態，報告穩態中位數、p95 與峰值，不得以最後單一樣本冒充穩態。若採樣時長 <30 秒，報告必須標明「觀察性短取樣 (Observational Short Sample)」。
4. **字面精確比對 (Literal Readiness Matching)**：放棄正則比對，嚴格採用字面比對（支援跨滾動 chunk 邊界匹配），避免字元群組（如 `[READY:...]`）誤匹配任意單一字元而提早結束採樣。
5. **嚴格安全與清理 (Process Group Cleanup)**：所有被測程序及其衍生子程序一律配置獨立 Process Group（`setsid`），攔截 `SIGTERM` 與 `SIGINT`，在正常結束、逾時、崩潰或中斷路徑上一律發送 `SIGTERM` 緊接著 `SIGKILL`，並驗證程序已被回收，杜絕孤兒程序殘留。
6. **有界監控緩衝區 (Bounded Log Buffer)**：採集 stdout/stderr 時使用滾動環形緩衝區（上限 64 KB），杜絕 debug 大量輸出導致測試座自身記憶體耗盡。
7. **執行前取樣器閘道 (Pre-exec Sampler Gate)**：launcher 先寫下 `(pid, starttime)` 並等待 `--sampler-ready-file`。測試座在信號處理與 `/proc` 身分可讀之後才發布該檔，launcher 才 `os.execv`。這不是動態連結器第一條指令的觀測，也不是連續硬體峰值。離散取樣（nominal ~50ms）只在閘道發布時 exe 尚未是目標、且後來觀察到 exe 轉換時，才把轉換後、就緒前的樣本標成 launch。就緒標記出現在錯誤的 exe 上必須失敗，不能當成該目標的成功量測。

---

## 2. 指標定義與計量邊界

| 指標名稱 | 來源途徑 | 說明與約束 |
| --- | --- | --- |
| **RSS (Resident Set Size)** | `/proc/<pid>/smaps_rollup`（降級：`/proc/<pid>/statm`） | 程序樹各程序常駐實體記憶體總和。降級使用 `statm` 時計算 resident pages × 頁面大小（通常 4096 bytes）。若主程序 RSS 無法讀取或為 0，測試座視為採樣失敗，絕不輸出假成功 0 值。 |
| **PSS (Proportional Set Size)** | `/proc/<pid>/smaps_rollup` | 程序樹各程序之比例實體記憶體（包含平攤之共享程式庫頁面）。**若核心不支援或無法讀取 `smaps_rollup`，PSS 嚴格設為 `null`，嚴禁拿 RSS 混充 PSS**。 |
| **主程序 VmHWM** | `/proc/<root_pid>/status` (`VmHWM`) | 主程序生命週期之最高實體記憶體水位。**嚴禁加總子程序之 VmHWM**（不同時點之峰值加總在數學與系統語意上均不成立）。 |
| **取樣並行峰值 RSS** | 取樣時點整棵程序樹之 `sum(RSS)` | 應用運作期間，於任一離散取樣點記錄到的整棵程序樹最大瞬間 RSS。 |
| **穩態中位數與 p95** | 就緒後區間所有取樣值 | 評估穩態常駐開銷之權威指標。 |
| **單調時鐘採樣週期** | `time.monotonic()` | 記錄實際採樣間隔之 min、max、mean、median、p95，反映取樣抖動。 |

### 已知系統量測限制與快取血統

1. **離散採樣點峰值而非絕對硬體峰值 (Discrete Sampled Peaks vs. Absolute Peaks)**：
   - 本套件採 nominal ~50ms 週期取樣。記錄之峰值為「離散採樣時點所觀察到的整棵程序樹實體記憶體峰值」，並非連續硬體匯流排級絕對峰值，次間隔瞬態分配（如生命週期 <50ms 的超短暫 Git 子程序）可能未被離散採樣點記錄。報表與資料結構嚴格宣告為 `sampledPeakRssBytes`，不得宣稱「絕對硬體極值」。
2. **快取血統宣告 (Cache Provenance)**：
   - **程序冷啟動 (Process-Cold)**：每輪基準測試均於全新獨立之程序樹執行，配置私有 XDG 目錄（`XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_CACHE_HOME`）與私有 D-Bus session，排除程序內記憶體殘留。
   - **檔案系統快取未受控 (Filesystem cache uncontrolled)**：本測試座不呼叫 `drop_caches`，也不宣稱檔案系統冷啟動。主機 page cache 冷或暖都沒有被量測。真實檔案系統冷啟動標示為 `UNSUPPORTED`。
3. **GPU 與顯示伺服器配置 (GPU & Display Surfaces)**：
   - Linux `/proc` RSS/PSS 僅統計使用者空間虛擬位址映射之 RAM。
   - GPU 專屬 VRAM、DMA-BUF、DirectX/Vulkan/Mesa 驅動內部配置以及 X11/Wayland 合成器表面記憶體不反映在程序樹 RSS/PSS 中。

### 發布比對 (Release D4)

本測試座不自動接受發布比較，也不產生 `COMPATIBLE` 或節省量。`--compare-baseline` 直接以非零狀態結束並印出 `UNSUPPORTED`。建置收據、雜湊與來源只作為血統事實記入報告：`--build-receipt` 標成 user-supplied，二進位旁的 `*.receipt.json` 標成 sidecar；收據裡的 sha256 與磁碟上的二進位不一致時，在開跑前拒絕。實際的 release 比較由監督者對已對上的具體 run 產物進行。Release D4 gate 仍是必要條件，目前沒有通過。

---

## 3. 工作負載規範 (Workload Generator)

工作負載由 `scripts/workload_generator.py` 透過 `git fast-import` 高效、確定性生成。

### 預設方案 (Presets)

| 方案 | Repos | Tracked Paths/Repo | Commits/Repo | Refs/Repo | 適用情境 |
| --- | :---: | :---: | :---: | :---: | --- |
| **Smoke** | 2 | 10 | 10 | 3 | 本機單元與合約測試（<1 秒完成） |
| **Medium** | 15 | 100 | 60 | 10 | 觀察性基準與日常驗收（<2 秒完成） |
| **Standard** | 15 | 10,000 | 20,000 | 100 | 第 10 節規範之發布標準負載 |

### 狀態覆蓋與 Oracle 驗證

每個工作區均包含真實的 Git 圖形與工作目錄／索引複合狀態：
- **分岔與合併 (Divergent Branch & Merge)**：包含 `feat/divergent` 分支、主幹並行 commit 及雙親 merge commit。
- **samepath staged A working B**：單一檔案（`src/staged_and_working.ts`）在 HEAD 為 V0、在 Index stage 為 VA、在 Working Tree 為 VB，`git status --porcelain` 呈現 `MM`。
- **重新命名 (Renames)**：暫存區重新命名（`R  src/rename_src.ts -> src/rename_dest.ts`）。
- **刪除 (Deletions)**：暫存區刪除（`D `）與工作目錄刪除（` D`）。
- **暫存與未追蹤變更**：`A `、` M`、`??`。

### 安全防護規範

1. **非空目錄拒絕**：目標目錄若已存在且內含檔案，產生器立即中止並拋出錯誤，嚴禁覆寫或清除使用者既有檔案。
2. **符號連結拒絕**：目標路徑本身或其父目錄若為 symlink，立即拒絕以防路徑跳躍意外。
3. **唯自身暫存清理**：產生器僅清理自己建立的臨時目錄；若使用者指定目錄，失敗時絕不遞迴刪除。
4. **Manifest 記錄**：產出 `workload_manifest.json`，完整記錄種子碼、SHA-1 OID、exact counts 與 porcelain 狀態。

---

## 4. 測試輪廓 (Benchmark Profiles)

依據第 9 節產品目標，基準測試套件定義以下輪廓：

| 輪廓代號 | 命令列參數 | 就緒標記 | 產品記憶體預算 (Release) |
| --- | --- | --- | --- |
| **GPUI Idle** | `--workspace <empty_dir> --mode idle` | `[READY:IDLE]` | 穩態 ≤ 100 MiB |
| **GPUI 1 Repo Overview** | `--workspace <1_repo_dir> --mode overview` | `[READY:OVERVIEW]` | 穩態 ≤ 160 MiB |
| **GPUI 15 Repos Overview** | `--workspace <15_repo_dir> --mode overview` | `[READY:OVERVIEW]` | 穩態 ≤ 256 MiB（較 1 repo 增量 ≤ 96 MiB） |
| **GPUI 15 Repos Preview** | `--workspace <15_repo_dir> --mode preview` | `[READY:PREVIEW]` | 穩態 ≤ 384 MiB（瞬間峰值 ≤ 512 MiB） |
| **100-Switch Soak** | *需原生 UI 驅動程式* | *UI Driver* | 標記為 `PENDING`，不虛構切換資料 |
| **3 Workspace Tabs**（`bench_native_memory.py --profile 3tabs`） | `--workspace <1_repo_dir> --mode normal`，再以「+」加輸入路徑開資料集另外兩個 repo，各一個工作區分頁 | 三個分頁的 repo 都載入、點回第一個分頁並完成與 1repo 相同的一次複製後的 nonce 就緒檔 | 第一版只量測、不設門檻；拿到數字後再訂 |
| **Tauri Baseline**（driver 已移除，僅存歷史紀錄） | `scripts/measure_tauri.sh`（WebDriver 驅動真 app，attach 模式取樣） | 隨機 nonce 就緒檔 | idle／1 repo 已量測；15 repo `UNSUPPORTED`（Tauri 只有單一 repo）。見 [`tauri-baseline-measurement.md`](tauri-baseline-measurement.md)，不宣稱節省比例 |

---

## 5. 命令列工具與操作指引

### 工作負載產生器 (`scripts/generate_15_repos.sh`)

```bash
# 1. 產生 medium 基準工作區至指定目錄
./scripts/generate_15_repos.sh /tmp/snip-workload-medium --preset medium

# 2. 產生 smoke 測試工作區
./scripts/generate_15_repos.sh /tmp/snip-workload-smoke --preset smoke

# 3. 指定隨機種子與自訂數量
./scripts/generate_15_repos.sh /tmp/custom --repos 5 --files 50 --commits 30 --refs 5 --seed 12345
```

### 記憶體量測測試座 (`scripts/measure_memory.sh`)

執行單一輪廓量測時，透過 `--` 分隔測試座參數與目標程序參數，避免參數解析衝突：

```bash
# 1. 單一輪廓量測（使用標準 '--' 傳遞目標參數）
./scripts/measure_memory.sh \
  --ready-marker "[READY:OVERVIEW]" \
  --profile-label "GPUI Release (15 Repos Overview)" \
  --steady-seconds 30.0 \
  --out-dir target/benchmark-results \
  -- \
  target/release/snip-desktop-native \
  --workspace /tmp/snip-workload-medium \
  --mode overview

# 2. 或使用 JSON 陣列形式傳遞 argv
./scripts/measure_memory.sh \
  --cmd-json '["target/release/snip-desktop-native", "--workspace", "/tmp/snip-workload-medium", "--mode", "overview"]' \
  --ready-marker "[READY:OVERVIEW]" \
  --profile-label "GPUI Release (15 Repos Overview)" \
  --steady-seconds 30.0 \
  --out-dir target/benchmark-results

# 3. 完整標準套件執行
./scripts/measure_memory.sh \
  --suite \
  --workspace /tmp/snip-workload-medium \
  --bin target/release/snip-desktop-native \
  --steady-seconds 30.0 \
  --out-dir target/benchmark-results
```

### 重複量測

程序冷啟動、檔案系統快取未受控（不 drop_caches）。這組命令只量測，不比較、不宣告 D4 通過：

```bash
python3 scripts/bench_native_memory.py \
  --bin target/release/snip-desktop-native \
  --build-profile release \
  --build-receipt target/release/snip-desktop-native.receipt.json \
  --label "Release Candidate" \
  --workspace /tmp/snip-workload-standard \
  --out-dir target/benchmark-native-10repeat \
  --runs 10 \
  --steady-seconds 30.0 \
  --sample-interval 0.05
```

- 原生 driver 的 launcher 會等取樣閘道再 `execv`。launch 樣本只存在於觀察到 exe 轉換之後；轉換前的 launcher 記憶體是 `launcher-setup`，不計入目標峰值。
- `--compare-baseline` 會以狀態 2 結束並印出 `UNSUPPORTED`。
- `3tabs` 不在預設輪廓裡，要用 `--profile 3tabs` 指定（可與其他 `--profile` 並用），並且 `--workspace` 要是資料集：第一個分頁是 `--repo`（預設資料集的 `repo-01-core`），另外兩個是資料集裡排在前面的其他 repo。等待一律經過 `e2e_scaled`，慢的機器設 `SNIP_E2E_TIMEOUT_SCALE`。

### 自動化合約測試

```bash
# 執行全部 harness 與 generator 合約測試
python3 -B -m unittest discover -s scripts/tests
```

產出結果檔案：
- `raw_samples.jsonl`：每 50ms 一筆之完整 JSON 取樣資料（包含 monotonic 時間戳記、RSS、PSS、PID 清單、有效性與遺失子程序清單）。
- `native_report.json` / `benchmark_report.json`：環境、時間戳記、各 phase 取樣峰值、穩態統計（median/p95/max）、主程序 VmHWM、快取血統與建置收據來源。
- `native_report.md` / `benchmark_report.md`：總表與量測邊界。觀察到的視窗幾何在各 run，不來自 `--screen`。
