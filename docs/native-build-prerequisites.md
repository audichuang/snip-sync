# 原生工作台建置環境與 CI 整合規範

文件狀態：配合原生預覽（`crates/desktop-native`）與 P0–P5 階段規劃（詳見 `docs/native-git-workbench-plan.md`）。

---

## 1. 介面與相容目標

- **Workspace Member**：`crates/desktop-native`
- **套件 / 執行檔名稱**：`snip-desktop-native`
- **精確相依鎖定**：`gpui = "=0.2.2"`（嚴格等號鎖定，不使用 `^` 語意相容範圍；此為原生工作台 feature 分支介面規範）
- **真實 OS 輸入整合測試**：`crates/desktop-native/tests/smoke.rs`（Linux X11 環境下使用 `xdotool`、`x11-apps`、`imagemagick` 驅動真實輸入與可攜式截圖）

---

## 2. 測試分工與 3-OS 覆蓋架構

為徹底避免同一 display 或剪貼簿爭用（clipboard / focus race condition），CI 與本地預檢實施明確的測試標的分工：

### A. 三平台（Linux / macOS / Windows）單元與編譯檢查（`test` & `lint-rust` Job）
- **全工作區檢查**：
  - `cargo clippy --workspace --all-targets --locked -- -D warnings`
  - `cargo doc --workspace --no-deps --locked`
- **舊有工作區完整測試（排除原生 binary crate）**：
  - `cargo test --workspace --exclude snip-desktop-native --locked --no-fail-fast`（Linux 於 Xvfb 下執行；保留全部 core、CLI、desktop 整合測試，絕不改為 `--lib` 導致丟失整合測試）
- **原生 binary crate 純單元測試**：
  - `cargo test -p snip-desktop-native --bin snip-desktop-native --locked --no-fail-fast`（三平台皆執行，不觸發 OS GUI input）

### B. 僅 Linux 執行原生真實 OS 輸入測試（`native-smoke` Job）
- **獨立無頭執行環境**：專屬 `xvfb-run -a` 實體，杜絕 display 與 clipboard 爭用。
- **指令**：
  - `just native-smoke`：建立輸出目錄，透過 `scripts/headless-x11.sh` 設定獨立 Xvfb 與 lavapipe，強制 `SNIP_REQUIRE_ALL_TESTS=1`，保留測試失敗狀態並驗證必要截圖。
  - `just native-lifecycle`：同樣的無頭環境，執行 `tests/lifecycle.rs`（關閉／重開／退出的排乾，以及複製與貼上預覽的取消）。記錄寫到 `lifecycle.log`，截圖寫到輸出目錄下的 `lifecycle/`。
  - 除了 smoke 需要的工具，lifecycle 另外需要 `xclip`，以及 `cc` 與 `libx11-dev`（編譯送出 `WM_DELETE_WINDOW` 的小工具）。截圖用 `convert`，ImageMagick 6 與 7 都有。

### C. 跨平台驗證現況與發布門檻
- **全三平台（Linux, macOS, Windows）之原生 UI / IME 互動目前均為尚未完成驗證（pending）**。
- Linux 目前僅完成無頭 X11 輸入煙霧測試；macOS 與 Windows 完全無法執行此 X11 native input。
- 依據規劃，在通過 P5 門檻前，**全面原生發布切換嚴格阻擋**；`release.yml` 維持原狀，不提早發布未就緒之原生套件。

---

## 3. 截圖契約與構件驗收（Artifact Contract）

Native smoke 測試與 CI 驗收協定如下：

1. **輸出目錄**：`$SNIP_E2E_OUT`
   - CI runner：`${{ runner.temp }}/native-e2e-artifacts`
   - 本機預設：`target/native-e2e-artifacts`
   - 不使用任何硬編碼路徑（如 `/home/...`），一律採用可攜式相對路徑或環境變數。
2. **必要產出檔案**：
   - `smoke.log`：測試執行標準輸出記錄。
   - `graph.png`：分支圖畫面截圖。
   - `file_tree.png`：檔案樹畫面截圖。
   - `paste_preview.png`：貼上預覽畫面截圖。
3. **強制斷言檢驗（禁止 `cp || true` 掩蓋缺失）**：
   - 檢驗 `smoke.log` 存在且非空（`test -s "$SNIP_E2E_OUT/smoke.log"`）。
   - 檢驗 3 張 PNG 檔案皆存在、非空，且開頭 8 位元組為合法 PNG 魔術位元組（`\x89PNG\r\n\x1a\n`）。
   - CI 構件上傳設定 `if-no-files-found: error`，缺少任何檔案即標記失敗。

---

## 4. Ubuntu 24.04 (noble) 套件相依需求

CI runner 固定釘選 `ubuntu-24.04`。GPUI 與無頭截圖驅動所需套件如下：

### 建置與無頭圖形渲染套件
- 基礎 X11 / XKB：`libxkbcommon-dev`, `libxkbcommon-x11-dev`
- 字型函式庫：`libfontconfig1-dev`, `libfreetype6-dev`, `fonts-dejavu-core`, `fonts-noto-cjk`
- 軟體圖形驅動（Noble 支援套件）：`libgl1-mesa-dri`, `libegl1`, `libegl-mesa0`, `mesa-vulkan-drivers`
  - *注意：Ubuntu 24.04 已無 `libegl1-mesa` 候選套件，必須使用 `libegl1` 與 `libegl-mesa0`。*
- 輸入與截圖驅動：`xvfb`, `xdotool`, `x11-apps`（提供 `xwd`）, `imagemagick`（提供 `convert`）

### 關於主機環境之嚴格規範
- 開發主機缺少 `libxkbcommon-x11-dev` 時，**嚴禁**使用 `$HOME` 符號連結或自訂 linker flag 作為發布組態。
- 正式建置與 CI 一律以標準系統 package manager 提供的標頭檔與 `pkg-config` 為準。

---

## 5. 本機 preflight 指令

本地預檢與 CI 邏輯一致：

```bash
# 執行量測／工作負載／打包合約測試（以當次測試輸出為準）
just preflight-harness

# 執行原生 X11 煙霧測試（產出至 target/native-e2e-artifacts 並驗證 3 張 PNG 與 smoke.log）
just native-smoke

# 執行原生 lifecycle 測試（產出 lifecycle.log 與 lifecycle/ 下的截圖）
just native-lifecycle

# 預覽 dry-run
just --dry-run native-smoke

# 執行完整本機預檢（preflight-workflows、preflight-rust、preflight-harness、native-smoke、native-lifecycle、native-acceptance）
just preflight
```
