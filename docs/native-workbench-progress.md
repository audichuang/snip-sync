> **歷史 worker 紀錄，非驗收報告。** 本文描述的舊 UI 與記憶體數據已被後續審查取代或拒絕；不得據此宣稱完成或通過效能門檻。請以 [交付規格](native-workbench-delivery-spec.md)、[監督紀錄](native-workbench-supervision.md) 和經獨立核實的 checkpoint 為準。

# 原生 Git 工作台驗證進度

## 2026-09-27 進度檢查點：第一批缺口補齊並併入 develop（產品尚未完工）

- **當前基準**：`feature/lightweight-git-workbench-plan`，程式碼提交到 `c92598f`。`origin/develop`（`c05658e`）已是祖先；合併後的檔案樹與合併前相同。未推送、未開 PR、未發版。
- **本輪完成**（[接手清單](native-workbench-handoff-2026-09-27.md)第 1–5 項，以及第 13 項的 lifecycle 部分）：
  - 目錄樹讀取納入 lifecycle：`submit_tree_io` 改走 `spawn_owned`，關閉與退出會等讀取本身結束；排乾期間不接受新的目錄樹工作；關閉或重開後才到的結果以 lifecycle generation 丟棄。
  - Copy 取消：實際點擊 `btn-copy-cancel`，涵蓋檔案與 commit 複製；退出會取消進行中的 commit 複製；Apply 寫入中拒絕切換工作區。
  - 貼上預覽 core patch 已合入，三個檔案的 SHA256 與收據相同。
  - native 貼上預覽改在背景建立並可取消：首次預覽與對應變更後的重算都帶取消 token；建立中不能套用；新貼上、變更對應、取消、Escape、關閉工作區都會作廢舊的讀取。確認後的 Apply 仍不可中途取消。
  - `tests/lifecycle.rs` 已接入 `just preflight`（`just native-lifecycle`）與 CI 的 Native Smoke 工作。
  - 順帶修正兩個會讓必要關卡失敗的既有問題：`lifecycle.rs` 文件註解的連結（`cargo doc`），以及 IME 判斷測試在模組層級匯入 Pillow（harness）。
- **本地驗證**（接手者在 `c92598f` 執行完整 `just preflight`，exit 0）：
  - core：347 個執行期測試與 1 個 doctest 通過。忽略 2 個：硬綁 `/tmp/snip-workload-standard-20260925` 的標準 workload 測試，以及 1 個說明用 doctest。
  - native：67 個單元測試、5 個 smoke、12 個 lifecycle（真實 X11）。
  - 其餘：CLI 15、Tauri 真實 App E2E 22/22、harness 172、前端檢查與建置。
  - 變異驗證：把目錄樹工作改回不納入 lifecycle，兩個目錄樹測試失敗；貼上預覽不把 token 傳進 core，三個貼上測試失敗。兩者還原後通過。
- **更正下方兩個檢查點**：core 334 是舊數字；當時 lifecycle 還有目錄樹缺口，3 個 lifecycle 測試通過不代表所有背景工作都已納管；加入 `lifecycle.rs` 之後 `cargo doc` 其實會失敗，所以當時的 `preflight-rust` 結果不適用於含 lifecycle 的工作樹。
- **尚未驗證**：未推送，CI 尚未執行；lifecycle 與 smoke 只在本機 Linux X11 跑過。Wayland、macOS、Windows 未驗證。排乾失敗後的狀態還原（`c92598f`）沒有專屬測試。
- **後續待辦**：接手清單第 6–12 項，以及第 13 項其餘關卡（IME／協作／資源驗收的入口、標準 workload 測試）。整體產品尚未完成。

## 2026-09-27 進度檢查點：原生任務擁有權、匯出取消與工作區生命週期整合完成（產品尚未完工）

- **當前基準**：UI/IME 基準維持 `feature/lightweight-git-workbench-plan`（HEAD `9ecc8d7`，保留 `cdcb525` 原生 IntelliJ 風格 UI、`734fd93` 輸入框 XIM 游標跟隨與 `9ecc8d7` 點擊失焦候選窗重設），未碰觸 DTO 或 vendor/gpui。
- **Native 整合完成**：
  - 語意整合任務擁有權（`Lifecycle` 與 `spawn_owned`）覆蓋所有背景讀寫任務（儲存庫探索/載入、變更狀態、預覽、commit 記錄/diff/tree、檔案/commit 複製、確認之 Apply 寫入）。
  - 複製流程接入 `plan_export_with`、`revalidate_with` 與 `plan_commit_export_exact_with`，支援呼叫端 token 與 UI `btn-copy-cancel`，取消或過期時保證不覆寫剪貼簿。
  - 工作區 Close / Open 同行程切換與 Quit 均排乾所有背景任務與 Git 子進程（最多 8 秒），超時或洩漏維持存活並報錯；進行中之 Apply/Replay 嚴格拒絕關閉/退出。
  - 工作區關閉釋放容量（`release_vec`、`release_map`、`release_set`、`release_path`、`reader.release_retained()`、`text_input.clear_retained()`），不清除 OS 剪貼簿。
- **本地驗證**：
  - `cargo fmt --all --check` 與 warnings-denied clippy 通過。
  - `SNIP_REQUIRE_ALL_TESTS=1 scripts/headless-x11.sh cargo test -p snip-desktop-native --locked`（67 unit tests、3 real-OS lifecycle tests、5 smoke tests）全數通過。
  - `just native-smoke`（含截圖校驗）全數通過。
  - `cargo test -p snip-core --locked` 334 required tests + 1 passed doctest（1 ignored 說明範例）通過。所有變更均未 commit。
- **後續待辦**：
  1. Core 唯讀 paste-preview 取消 API 與 native 非同步接線（目前 paste-preview 仍為同步唯讀）；
  2. Fixed-OID basket 支援（UI 缺少加入 basket 控制項）；
  3. 雙機器 × 各 15 repo 真 UI 雙向協同驗收；
  4. D4 記憶體與 fd/watch 資源洩漏關卡；
  5. 跨平台（macOS/Windows）及 D5 交付門檻。整體產品尚未完成。

## 2026-09-27 進度檢查點：共享 Core 匯出整合完成（產品尚未完工）

- **當前基準**：UI/IME 基準維持 `feature/lightweight-git-workbench-plan`（HEAD `9ecc8d7`，保留 `cdcb525` 原生 IntelliJ 風格 UI、`734fd93` 輸入框 XIM 游標跟隨與 `9ecc8d7` 點擊失焦候選窗重設），未碰觸 UI、DTO 或 vendor/gpui。
- **Core 整合完成**：已將受審查之 9 個核心原始碼與測試檔案（嚴格 exact commit export、可取消有界 file export、Session 取消修復、main replay freshness）正式併入本功能分支。保留 legacy wrappers 相容性。
- **本地驗證**：334 項 core tests（含 private X11、`SNIP_REQUIRE_ALL_TESTS=1`）與 1 個 doctest 通過（1 個 ignored 說明範例、0 失敗）；fmt、warnings-denied clippy、`just preflight-rust` 及 `just native-smoke`（5 項 real-app E2E 場景與產物截圖）全數通過。
- **後續待辦**：
  1. Native paste preview 的背景取消 token 與生命週期接線；
  2. Fixed-OID basket 支援；
  3. 2×15-repo 雙端真 UI 驗收；
  4. D4 記憶體與 fd/watch 資源洩漏關卡；
  5. 跨平台（macOS/Windows）及 D5 交付門檻。整體產品尚未完成。

---

日期：2026-09-25。狀態：**GPUI 原生垂直切片原型與量測基線實作完成，正接受審查與缺陷修正；尚未宣稱預算達標或取代 Tauri 正式版本。**

本文件記錄依據 [native-git-workbench-plan.md](native-git-workbench-plan.md) 所實作之 Rust GPUI 原生垂直原型現況、修正項目、實測數據與已知限制。

---

## 1. 已完成產物與架構修正（Completed Artifacts & Fixes）

1. **原生工作台 Crate（`crates/desktop-native`）**：
   - 依賴精確鎖定 `gpui = "=0.2.2"`，直接引用並共用 `snip-core`。
   - **三欄式工作台佈局**：
     - 左欄：儲存庫列表（顯示名稱、分支、staged / unstaged / untracked 數量）。
     - 中欄：Commit 歷史列表與工作目錄變更列表（支援單一檔案選取/排除切換）。
     - 右欄：選取檔案內容與 Git diff 預覽（包含行號與差異著色）。
   - **選取與剪貼簿複製行為（Defect 1 修正）**：
     - **排除全部守衛**：若變更檔案被全部取消勾選，複製按鈕自動禁用，執行複製立即拒絕並保留剪貼簿既有內容不變，不再觸發核心預設「未傳即複製全部」之行為。
     - **非同步背景化**：複製作業移至 `background_executor`，進入複製忙碌狀態，不阻塞 UI 主執行緒。
     - **明確來源界限**：目前複製明確限定於工作目錄（Working Tree）。選取 Commit 檢視時，複製按鈕明確標記唯讀並禁用，避免產生複製該 Commit 內容之誤解。
   - **世代與識別保護（Defect 3 修正）**：
     - 引入 `generation: u64` 世代計數器。在切換儲存庫、檔案、Commit 或重新整理時遞增，並立即清空舊預覽。
     - 非同步回呼比對世代，非當前世代之過期回應一律捨棄。
     - 重新整理儲存庫時安全維護已選取索引，若舊儲存庫不在列表中則安全重設為 0 或清除。
     - 儲存庫讀取失敗時於 UI 明確顯示錯誤訊息，而非靜默忽略。
   - **資源有界保護（Defect 4 修正）**：
     - 預覽內容加入有界裁切函式 `bound_preview_text`，限制最多 500 行或 64 KiB，避免 GPUI 排版樹耗盡記憶體；若截斷則附帶顯式提示。
     - Commit diff 檢視限制輸出大小。
     - 移除狀態列「記憶體有界架構」宣傳文字，改為顯示真實進程 PID 與工作區路徑。

2. **核心模組修正（`crates/core/src/browser.rs` & `crates/core/src/gitsrc.rs`，Defect 2 修正）**：
   - `RepoSummary` 移除未匯出之 `#[derive(TS)]`，避免 DTO 漂移。
   - 區分並回傳真 tracked unstaged 數量（索引至工作目錄差異）、staged 數量、untracked 數量與 conflict 數量。
   - 完整保留 `GitSource::Working` 原有語意。
   - `discover_repositories` 加入有界掃描限制（預設上限 50 個儲存庫、200 個目錄項目）。
   - 新增真實 Git 回歸測試：
     - staged-only 情況下 unstaged 為 0。
     - staged A 與 working B 獨立計數與預覽內容。
     - 新 staged 檔案在工作目錄刪除時正確計入 unstaged 刪除。
     - untracked 檔案獨立計數。

3. **15 儲存庫測試負載產生器（`scripts/generate_15_repos.sh`）**：
   - 快速生成 15 個具備真實提交、分支、tags、staged、dirty、untracked 與 clean 狀態之 Git 儲存庫。
   - 採用子程序隔離 `( cd "$rdir" && ... )` 確保目錄乾淨。

4. **連續取樣記憶體量測套件（`scripts/measure_memory.sh`，Defect 6 修正）**：
   - 採用連續 50ms 取樣，採集整棵進程樹（主程序與所有子程序）之並行 RSS 與 PSS。
   - 引入各階段顯式就緒標記（`[READY:IDLE]`、`[READY:OVERVIEW]`、`[READY:PREVIEW]`），杜絕未就緒的假性低值。
   - 誠實分開揭露「取樣並行峰值 RSS」與「主程序 VmHWM」，不進行跨程序 VmHWM 累加。
   - 同時輸出 Markdown 報告與 JSON 格式報告。
   - 移除所有未經長期 soak 檢驗之百分比宣稱。

5. **真視窗自動化整合測試（`crates/desktop-native/tests/smoke.rs`，Defect 5 修正）**：
   - 測試包含 2 個獨立 Git 儲存庫。
   - 遵循 `AGENTS.md`：無 DISPLAY 時依循 `SNIP_REQUIRE_ALL_TESTS` 進行檢查。
   - 測試前注入隨機唯一 sentinel 至剪貼簿。
   - 驗證「排除全部檔案」時複製無動作且剪貼簿保持 sentinel。
   - 驗證「選取單一檔案」時剪貼簿精確包含該檔案且不含排除檔案。
   - 驗證還原至暫存目錄，確認磁碟檔案內容精確符合且排除檔案不存在。
   - 驗證儲存庫切換與預覽連動。

---

## 2. 可執行指令（Runnable Commands）

```bash
# 1. 執行原生 GPUI 工作台（開發模式）
just native --workspace /path/to/repos

# 2. 執行原生 GPUI 原生切片自動化 Smoke 測試（真實 X11 視窗、剪貼簿與還原驗證）
just native-smoke

# 3. 執行連續取樣之記憶體量測基準套件
just bench-memory

# 4. 執行既有 Tauri 應用的端到端 E2E 測試（確認未破壞既有功能）
just desktop-e2e

# 5. 執行完整 Rust 與前端 preflight 檢查
just preflight
```

---

## 3. 實測記憶體數據（15 Repos Workload，連續 50ms 取樣）

量測環境：
- 作業系統：Ubuntu 24.04.5 LTS (Linux 7.0.0-31-generic)
- CPU：12th Gen Intel(R) Core(TM) i7-12700 (20 cores)
- 實體記憶體：31,831 MiB
- 顯示環境：X11 (DISPLAY=:1)
- 工作負載：15 個真實 Git 儲存庫（多分支、staged、dirty、untracked、clean）

| 方案與測試階段 | 程序數 | 穩態 RSS (MiB) | 穩態 PSS (MiB) | 取樣並行峰值 RSS (MiB) | 主程序 VmHWM (MiB) |
| --- | :---: | :---: | :---: | :---: | :---: |
| **GPUI Release (Idle/Empty)** | 1 | 107.48 | 61.90 | 107.48 | 107.48 |
| **GPUI Release (15 Repos Overview)** | 1 | 113.67 | 67.17 | 113.67 | 113.67 |
| **GPUI Release (Repo + Preview Active)** | 1 | 114.04 | 67.47 | 114.04 | 114.04 |
| GPUI Debug (Idle/Empty) | 1 | 129.99 | 84.46 | 129.99 | 129.99 |
| GPUI Debug (15 Repos Overview) | 1 | 138.08 | 91.58 | 138.08 | 138.08 |
| GPUI Debug (Repo + Preview Active) | 1 | 138.08 | 91.56 | 138.08 | 138.08 |
| **Tauri Debug (Idle/Startup)** | 3 | 430.16 | 221.45 | 433.45 | 163.80 |

*備註：以上數據為 Linux 本機實測讀數，不包含推估或假定之百分比節省宣稱。*

---

## 4. 平台實況、已知限制與阻礙（Blockers & Gaps）

1. **中文輸入（IME）**：
   - Linux X11 + Fcitx5 拼音已用 `scripts/check_native_ime.py` 實測（私有 Xvfb／D-Bus／Fcitx 設定，結束碼 0）：候選窗跟著搜尋框插入點、縮放後跟著移動，組字／送出／搜尋／退格正確，組字中點其他控制項候選窗會消失。
   - 最後一項靠 `vendor/gpui` 的本地 patch：GPUI 0.2.2 只在 `composing` 時於點擊 reset XIM，Fcitx 預設 off-the-spot 模式不會設它。升級 gpui 時照 `vendor/gpui/SNIP_PATCH.md` 重做。
   - 未驗證：Wayland（text-input-v3）、macOS、Windows 的 IME。
2. **跨平台實機**：
   - 本機為 Linux x86_64；macOS（Metal）與 Windows（DirectX）實機效能與記憶體開銷尚未於對應硬體上實測，不可假定數值與 Linux 一致。
3. **Linux 系統編譯先決條件（Defect 7）**：
   - 移除了先前 crate 層級 `build.rs` 搜尋 `~/.local/lib` 之非標準 hack。
   - 在 Linux 上編譯 GPUI 需要 `libxkbcommon-x11-dev`（或軟體連結 `libxkbcommon-x11.so`）。若環境未安裝 dev 套件，可設定標準 `LIBRARY_PATH`（`justfile` 已加入友善 fallback）。

---

## 5. 後續階段目標（Next Steps）

- **P1**：儲存庫深層路徑瀏覽、巢狀 Git 儲存庫邊界隔離、背景 Git 執行並行數上限限制（Semaphore 控制）。（已於核心 PR #19 完成）
- **P2**：檔案變更虛擬捲動清單、超大型 patch 行級虛擬化算繪。
- **P3**：拓撲 DAG 圖形整合與多 parent 合併分支渲染。（已於核心與原生整合）
- **P4**：待複製清單跨 repo 聚集與貼上還原。第 7 節才是目前範圍；這裡不能算完成。
- **P5**：macOS / Windows 實機驗證與產品化發布準備。

---

## 7. D3 修正檢查點（2026-09-26 第二輪，尚未完成 D3）

第 6 節的完成宣告已撤回。第一輪把「沒對上 basename 的路徑默默放進主要目錄」和「超大目錄可以超過保留上限」留了下來，監督者也重現了略過的非 UTF-8 目標事後變成可寫入時仍會重放。這一輪修那四件事。**不代表 D3 驗收通過。**

共用的是 `snip-core` 的 `plan_commit_replay` 與 `commits::replay`，沒有第二套重放：

- `CommitReplayPreview::capture` 先做出計畫，再用 symlink-safe 的方式記下目的地 HEAD、分支 ref、index，以及每個已命名目標的位元組或「當時不存在」，然後立刻再計畫一次。兩次計畫不同就不儲存預覽。`revalidate` 比對這份新鮮度，並再跑一次 `plan_commit_replay`。非 UTF-8 的略過會記下那個檔案；它後來變成可寫的 UTF-8，或 unsafe symlink 父目錄被換成真實目錄，預覽都是 stale，`execute` 在呼叫 `commits::replay` 之前返回。symlink 只哈希連結自己的文字，不打開目標。`NotCopied` 仍因 payload 沒有位元組而略過，不進這份新鮮度。整段 commit、覆寫預設關閉、不連續 OID 拒絕，維持上一輪。
- 沒有 `// clipcode-root:` 時，路徑的每一個第一段都要使用者明確選：對到某個完整目的目錄，或留在主要目錄下當成相對路徑。同名 basename 不會自動挑一個，也不可從「來源 repo 沒開」推成安全。有 root marker 的單根匯出不開對應列，路徑維持原樣。對應未完成時套用按鈕與 `execute` 都不寫入。
- 工具列「複製選取籃 (n)」和狀態列的 n 是整個選取籃，複製匯出籃內所有 repo 的檔案與 Git 變更。只瀏覽不加選取；切換分頁或 repo 保留已選身份。清空選取籃不碰剪貼簿。同一路徑有兩筆選取（含檔案列與 Git 變更）會拒絕複製，不靜默丟掉其中一筆。
- Commit tree 的硬上限是 256KiB，比規格裡 8MiB 的 tree 預算更嚴，沒有例外。目錄清單、順序、鍵、錯誤、展開狀態，以及這些容器的 capacity，都算進 `retained_bytes`。單一目錄若放不下，會先截斷或直接拒絕，再談保留。淘汰順序是最舊的非根清單，然後才是根；展開與錯誤也依各自的舊到新順序，不用 HashMap 的迭代順序。

仍未做，之後仍要補：取消貫穿匯出與 commit API（`plan_export` 的第三個參數是 payload 位元組上限，不是 `CancelToken`）、其餘保留預算、生命週期、15 個 repo、IME、平台與發布。D4 量測與 D5 preflight／CI／發布都還沒做。這一輪沒有改 clipboard contract fixture。

## 6. D3 交付階段（2026-09-26，舊實作紀錄，完成宣告已撤回）

下面這份清單是當時的實作紀錄。其中「D3 已完成」、選取籃會自動跨 repo、對應已明確、commit replay 已安全、Rev tree 已是 LRU，都已被第 7 節撤回。

依據當時的交付規格所做的整合嘗試：

1. **來源識別與分組變更（Source Identity & Grouped Changes）**：
   - 區分並呈現 `group_conflicted`、`group_staged`、`group_unstaged`、`group_untracked` 分組標題 (`change-header:{group}`)。
   - 獨立來源狀態徽章：`S`（Staged, 綠色）、`M`（Unstaged, 橙色）、`U`（Untracked, 灰色）、`!`（Conflicted, 紅色）。
   - 同時支援通用探針 `change-row:{path}` 與來源特定探針 `change-row:{source}:{path}`。
   - 同一相對路徑之 staged 與 unstaged 變更享有獨立選取、預覽與匯出。

2. **跨儲存庫選取籃（Multi-Repo Basket Export）**：
   - 跨儲存庫選取籃維護 `ExportItem` 識別（不預載 payload 位元組），切換儲存庫與標籤頁保持勾選。
   - 標籤頁作用域（Tab-Scoped）：
     - 專案樹標籤頁（`FileExplorer`）：複製目前專案樹中已勾選之檔案。
     - 變更標籤頁（`GitChanges`）：複製跨儲存庫之變更選取籃。
   - 整合 `snip_core::transfer::plan_export` 嚴格把關 wire payload 預算與排他性。

3. **多來源目的地映射與貼上預覽（Multi-Source Destination Mapping）**：
   - 貼上預覽採用 `snip_core::transfer::ImportMapping` 與多儲存庫前綴比對。
   - 貼上列呈現目標儲存庫徽章 `[item.dest_root_name]`。
   - 支援個別項目覆寫切換（`paste-overwrite:{path}`），預設關閉覆寫。
   - 目的地過期即時偵測拒絕寫入。

4. **Commit Replay 匯出與套用（Commit Replay）**：
   - Git Log 工具列提供 `btn-copy-commits` 按鈕。
   - 匯出連續 first-parent 範圍透過 `snip_core::transfer::plan_commit_export` 與 `commits::to_clipboard_text`。
   - 貼上時自動識別 commit payload（`commits::is_commit_payload`），呈現 commit replay 預覽與套用。

5. **有界快取與取消語意（Bounded Cache & CancelToken）**：
   - `CancelToken` 貫穿所有非同步背景讀取。
   - `RevTree` 目錄與項目上限限制（`MAX_CACHED_DIRS = 50`, `MAX_CACHED_ENTRIES = 10_000`），採 LRU 驅逐非根目錄。
   - 徹底移除 `core_shim.rs`，所有功能直接消費 `snip-core`。

6. **驗證與產物（Verification & Artifacts）**：
   - `snip-desktop-native` 31 個單元測試全數通過（`cargo test -p snip-desktop-native --bin snip-desktop-native`）。
   - 真實 OS input（xdotool in Xvfb, Lavapipe 軟體渲染）E2E smoke 測試全數通過：
     - 生成 6 張真視窗截圖於 `target/native-e2e-artifacts/`：
       - `graph.png`
       - `file_tree.png`
       - `paste_preview.png`
       - `workbench_900x600.png`
       - `paste_preview_900x600.png`
       - `workbench_900x600_en.png`
     - 包含分組變更標題點擊、來源預覽、Commit Replay 複製、覆寫開關與貼上還原之端到端驗證。

