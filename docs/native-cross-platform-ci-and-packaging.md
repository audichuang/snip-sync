# Native Cross-Platform CI & Packaging Specification

日期：2026-09-25。狀態：**候選產物管線與本地測試實作就緒，CI 尚未於 GitHub Actions 執行，執行期前提待測（UNVERIFIED）**。

本文件記錄原生跨平台 CI 建置相依性稽核、候選產物包裝管線（macOS arm/intel、Linux、Windows）、產物驗證工具、以及針對 UI 擁有者（`crates/desktop-native`）的唯讀檢視 findings 與整合交接清單。

驗證器修正（Mach-O fat、套件路徑綁定、DMG 失敗即關閉）落在 `fix/native-package-verification`。這次沒有修改 `.github/workflows/ci.yml`；該檔既有的 macOS fast git runner gate 維持原樣。四目標 candidate CI、macOS `hdiutil` / `codesign` 實機，以及 Windows 真 PE 仍待後續 CI 執行。在 Linux 上跑過的單元測試不能解讀成 Darwin 執行期已通過。

---

## 1. 原生建置相依性稽核 (Native Build System Dependencies)

原生工作台（`crates/desktop-native`）採用 GPUI 0.2.2 及 `arboard` 剪貼簿引擎。在不同作業系統環境下，CI 建置與執行所需之系統相依性如下：

### 1.1 Linux (Ubuntu 24.04 / 22.04)

GPUI 在 Linux 上同時支援 X11 及 Wayland，並透過 Mesa/Gallium llvmpipe 與 Vulkan 驅動提供軟體繪圖 fallback。原先 CI 僅在 `native-smoke` 安裝部分相依套件，且遺漏 `libwayland-dev`，導致 `lint-rust` 與 `test` 在建置整個 workspace 時可能面臨連結失敗。

**完整相依清單**：
- **X11 / XKB**: `libxkbcommon-dev`, `libxkbcommon-x11-dev`
- **Wayland (arboard & GPUI)**: `libwayland-dev`
- **字型與渲染 (FreeType & Fontconfig)**: `libfontconfig1-dev`, `libfreetype6-dev`, `fonts-dejavu-core`, `fonts-noto-cjk`
- **圖形驅動 (OpenGL / EGL / Vulkan)**: `libgl1-mesa-dri`, `libegl1`, `libegl-mesa0`, `mesa-vulkan-drivers`
- **測試與截圖 (Headless X11)**: `xvfb`, `xdotool`, `x11-apps`, `imagemagick`

**CI 修改點**：
- [`.github/workflows/ci.yml`](../.github/workflows/ci.yml) 中的 `lint-rust`、`test`、`native-smoke` 與 `native-candidate-artifacts` 均已更新為安裝完整的原生相依套件清單。

### 1.2 macOS (aarch64-apple-darwin, x86_64-apple-darwin)

- **SDK 相依性**：GPUI 在 macOS 上依賴系統框架（Cocoa, Metal, CoreGraphics, CoreText, AppKit, QuartzCore），GitHub Actions 的 `macos-latest` runner 已內建完整 macOS SDK，無須額外透過 Homebrew 安裝 C 函式庫。
- **最低部署版本 (Deployment Target) 與實機執行未驗證聲明**：
  - 選定之編譯建置最低目標為 macOS 11.0 Big Sur（Rust 官方 Tier 1 Apple Silicon 最低支援版本），CI 建置環境與產物中繼資料（`Info.plist` 之 `LSMinimumSystemVersion`）統一且明確設定 `MACOSX_DEPLOYMENT_TARGET=11.0`。
  - **在 macOS 11 實體硬體上的實際執行狀態屬於 UNVERIFIED**：GPUI 0.2.2 底層依賴 Metal、DisplayLink 與近代 macOS framework API，其精確最低支援版本需要實機硬體驗證數據（empirical hardware evidence）。目前 CI 設定在 macOS 26 系列 runner 上進行編譯與 CLI smoke；本次新 matrix 尚未實際執行，不可宣稱已在 macOS 11 上實測驗收通過。
- **目標架構**：Apple Silicon (`aarch64-apple-darwin`) 使用 `macos-latest`；Intel (`x86_64-apple-darwin`) 使用 `macos-26-intel`。兩個 leg 都必須實際執行對應 binary 的 CLI smoke；Intel runner 架構不符時失敗，不跳過。
- **執行期限制**：此 matrix 尚待 GitHub Actions 實際執行。CLI 與套件驗證不代表 macOS GUI／IME 已驗證，詳見 [CI 整合收據](native-ci-integration.md)。

### 1.3 Windows (x86_64-pc-windows-msvc)

- **SDK 相依性**：GPUI 在 Windows 上依賴 Direct3D 11/12、DXGI、Direct2D/DirectWrite 與 Win32 API。GitHub Actions 的 `windows-latest` runner 已內建 MSVC 與 Windows SDK。
- **子系統 (Subsystem)**：若編譯為 release binary，必須處理 Console 視窗抑制（見第 4 節）。

---

## 2. 候選產物包裝管線 (Native Candidate Artifact Pipeline)

依監督者明確指示：
1. **嚴禁在尚未通過 P5 驗收前將原生 prototype 自動發布為 release 資產**。
2. [`.github/workflows/release.yml`](../.github/workflows/release.yml) 完整保留原有穩定發布流程（Tauri 桌面應用、CLI、Homebrew Cask），完全不做任何自動切換或發布污染。
3. 原生 4 平台建置、打包與產物驗證獨立為 [`.github/workflows/ci.yml`](../.github/workflows/ci.yml) 的 `native-candidate-artifacts` job，僅透過 `actions/upload-artifact@v4` 上傳 candidate artifacts，並列入 `CI gate` 必要前置工作。
4. 正式切換（P5 native cutover）必須等待三平台真實 E2E、15 repo soak 記憶體量測通過，由監督者正式審查與決策。

### 2.1 支援矩陣與候選資產命名

| 平台 | Target Triple | 候選 CI 資產檔名 | 內部結構 |
| --- | --- | --- | --- |
| **macOS (Apple Silicon)** | `aarch64-apple-darwin` | `snip-desktop-native-mac-arm.dmg`<br>`snip-desktop-native-mac-arm.tar.gz` | `snip-desktop-native.app`<br>`Contents/MacOS/snip-desktop-native`<br>`Contents/Info.plist` (ad-hoc signed)<br>DMG 根目錄含 `/Applications` 連結 |
| **macOS (Intel)** | `x86_64-apple-darwin` | `snip-desktop-native-mac-intel.dmg`<br>`snip-desktop-native-mac-intel.tar.gz` | 同上（二進位為 Mach-O 64 x86_64） |
| **Linux (x64)** | `x86_64-unknown-linux-gnu` | `snip-desktop-native-linux-x86_64.tar.gz` | `bin/snip-desktop-native`<br>`share/applications/snip-desktop-native.desktop`<br>`README.txt` (含版本資訊) |
| **Windows (x64)** | `x86_64-pc-windows-msvc` | `snip-desktop-native-windows-x64.zip` | `snip-desktop-native/snip-desktop-native.exe`<br>`snip-desktop-native/README.txt` |

### 2.2 堅持使用原生效能平台工具

不引入第三方重量級打包框架或自製包裝引擎，完全使用各平台主流維護工具：
1. **macOS**:
   - 建立標準 `.app` 目錄結構與 `Info.plist`。
   - 使用 macOS 原生 `codesign` 進行 ad-hoc 簽署：
     `codesign --force --deep --sign - "snip-desktop-native.app"`
   - 使用 macOS 原生 `hdiutil` 製作 UDZO 唯讀壓縮 DMG，並配置標準 `/Applications` 捷徑：
     `hdiutil create -volname "snip-desktop-native" -srcfolder "$DMG_STAGE" -ov -format UDZO "$DMG_OUT"`
   - Darwin target 必須同時具備 `codesign` 與 `hdiutil`。任一工具不存在時，`scripts/package_native.sh` 以非 0 結束，不省略 DMG、也不把未簽署 bundle 當成成功產物。
2. **Linux**:
   - 使用標準 POSIX tar，保留 `0755` 執行權限。
3. **Windows**:
   - 使用標準 `zipfile` / PowerShell `Compress-Archive`。

### 2.3 平台獨立 Checksum

每個 target 的打包輸出目錄均生成 `SHA256SUMS-${TARGET}.txt`，避免在多 target 同時收集產物時檔名碰撞互斥。驗證工具要求完整覆蓋率（目錄下所有產物必須全數登記，且檔案不可為空）。

---

## 3. 產物驗證工具 (`scripts/verify_artifacts.py`)

新增之 [`scripts/verify_artifacts.py`](../scripts/verify_artifacts.py) 使用 Python 3 純標準函式庫，具備跨平台執行能力，用於在整合檢出或 CI 發布管線中對建置產物進行嚴格審計：

### 3.1 驗證能力與安全特性
1. **二進位魔數與架構解析 (Binary Header Auditing)**：
   - **ELF**: 解析 `e_ident`、位元數 (32/64)、Endianness，並根據 `e_machine` 驗證 `x86_64` (0x3E) 或 `aarch64` (0xB7)。
   - **Mach-O**: 解析 thin `MH_MAGIC_64` / `MH_CIGAM_64`（`0xfeedfacf` / `0xcffaedfe`）及 CPU type（`0x01000007` 為 x86_64，`0x0100000c` 為 ARM64）。Fat32（`0xcafebabe` / `0xbebafeca`）與 fat64（`0xcafebabf` / `0xbfbafeca`）依魔數決定 endian，讀取 `nfat_arch`（限 1–8）與 arch table，並檢查每個 slice 的 offset、size、alignment 落在檔案範圍內。Slice 內的 thin header 必須是結構完整的 `MH_EXECUTE`，且其 CPU 與 table 一致。預期架構必須真的有對應 slice。沒有「看到 universal 就跳過架構比對」的豁免。Java class、截斷 header、空的 fat、以及只含另一個架構的 fat 一律拒絕。
   - **PE (Windows)**: 解析 DOS header `MZ`、PE signature offset，並根據 machine 欄位驗證 `0x8664` (AMD64) 或 `0xaa64` (ARM64)。
2. **安全串流解析 (Safe Stream Parsing)**：
   - 不採用盲目全目錄解壓縮至硬碟；改由 `tarfile` 與 `zipfile` 串流直接讀取檔案標頭與中繼資料。
   - 拒絕路徑遍歷（檔名含 `..` 或前綴 `/`）、重複檔案項目或危險符號連結。
3. **版本與套件路徑**：
   - macOS tarball 的執行檔必須是 `snip-desktop-native.app/Contents/MacOS/snip-desktop-native`，版本來自同一個 bundle 的 `Contents/Info.plist`（`CFBundlePackageType=APPL`、`CFBundleExecutable=snip-desktop-native`、`CFBundleShortVersionString` 與 `CFBundleVersion`）。
   - Linux tarball 的執行檔必須是 `snip-desktop-native-<version>/bin/snip-desktop-native`，應用程式版本來自同一個 package 的 `README.txt`（精確一行 `Version: <version>`）。`share/applications/snip-desktop-native.desktop` 要有 `[Desktop Entry]`、`Type=Application`，且 `Exec` 啟動 `snip-desktop-native`。`.desktop` 的 `Version=1.0` 是 Desktop Entry 規格版本，不是應用程式版本。
   - Windows zip 的執行檔必須是 `snip-desktop-native/snip-desktop-native.exe`，版本來自同一個目錄的 `README.txt`。
   - 誘餌 plist / README、重複候選、缺少執行權限、setuid/setgid、以及 symlink 一律拒絕。
   - 有寫明的 target triple 就決定架構、二進位格式、執行檔名稱與產物種類。再傳入的架構、格式或執行檔名稱若跟該 target 衝突，直接拒絕，不能蓋過 target。`arm64` 與 `aarch64` 是同一架構。沒有 target 時維持結構檢查。
4. **macOS DMG 與候選集合**：
   - Apple 候選目錄必須同時有該 target 的 `.tar.gz` 與 `.dmg`（檔名見第 2.1 節），且沒有其他產物。Linux 目錄只接受 `snip-desktop-native-linux-x86_64.tar.gz`；Windows 目錄只接受 `snip-desktop-native-windows-x64.zip`。
   - 在 Darwin 上，透過 `hdiutil attach -nobrowse -readonly -mountpoint "$mnt"` 唯讀掛載 `.dmg`。
   - 斷言 DMG 內有且僅有一個 `.app`，`Applications` 連結指向 `/Applications`，對內部 `.app` 做架構、版本與 `codesign --verify --deep --strict` ，並在 `finally` 無條件 `hdiutil detach`。detach 失敗視為驗證失敗。
   - 非 Darwin 主機無法完成這項掛載。完整 Apple 審計在此時失敗，不得把未驗證 DMG 計入通過。直接對 tarball / zip 做的結構檢查是有限模式（`audit_scope=structural`），輸出不得當成完整平台審計通過。
   - 二進位與 metadata 只做有上限的串流讀取（二進位檢視上限 512 MiB，metadata 64 KiB），不把封存解到磁碟。
5. **完整覆蓋 Checksum 驗證**：
   - 嚴格校驗雜湊；目錄中若有未在 checksums 登記的產物，或 checksum 檔案為空，一律中斷報錯。
6. **macOS 簽署審計**：
   - 在 macOS runner 上自動執行 `codesign --verify --deep --strict --verbose=2`，並明確斷言為 ad-hoc 簽署，**嚴禁宣稱已通過 Apple Notarization**。

---

## 4. 嚴格可攜 CLI Smoke 測試器 (`scripts/smoke_native.py`)

原先 `smoke_native.sh` 在版本未實作或逾時時回報 `[GAP]` 卻仍以 exit 0 偽造通過，且依賴 macOS 上預設沒有的 GNU `timeout`。

現已全面重構為 [`scripts/smoke_native.py`](../scripts/smoke_native.py)：
- **純 Python 標準函式庫**：使用 `subprocess.Popen` 與行程群組隔離逾時機制，相容 Linux、macOS、Windows。
- **嚴格失敗判定**：
  - 必須明確傳入 `--bin` 與 `--expected-version`，不設任何預設 fallback。
  - `--help`：必須回傳 status 0 且輸出包含應用程式名稱。
  - `--version`：必須回傳 status 0 且輸出精確包含版本號；**若逾時（如 pilot 進入 GUI 迴圈）或回傳非 0，無條件以 exit 1 失敗**。
  - 逾時時徹底清除行程樹（`os.killpg` / `taskkill`），絕不遺留孤兒行程。
- **定位標籤**：明確標記為 `Native CLI Smoke (CLI Interface Only)`；Linux 真實 GUI 由現有 `native-smoke` 負責，macOS/Windows 真視窗 GUI 明確標記為未驗證。

---

## 5. UI 擁有者整合檢視報告 (Read-Only Findings for UI Owner)

依指派對 `crates/desktop-native/src/main.rs` 進行唯讀稽核，發現以下必要修正項目，需由 UI 擁有者（Native UI owner）配合修復：

### 5.1 缺陷 1：缺少 `--version` 及 `-V` 支援 (致命)

- **現況**：
  在 `crates/desktop-native/src/main.rs` 的 `parse_cli_args()` 中：
  ```rust
  match args[i].as_str() {
      "--workspace" if i + 1 < args.len() => { ... }
      "--mode" if i + 1 < args.len() => { ... }
      "--restore-dir" if i + 1 < args.len() => { ... }
      "--help" | "-h" => { ... std::process::exit(0); }
      _ => {}
  }
  ```
- **問題行為**：
  當使用者或 CI / 發布腳本執行 `snip-desktop-native --version` 時，該參數被 `_ => {}` 靜默忽略，程式繼續進入 GPUI 的視窗初始化與事件迴圈！
  - 在本機實測：執行 `snip-desktop-native --version` 會跳出視窗並載入 repo，直到 timeout。
  - 在無螢幕或 CI 環境下：會因為找不到 display server 而 panic 或 hang！
  - 無法透過標準 CLI 取得目前二進位版本。
- **建議修復**：
  在 `parse_cli_args()` 加入：
  ```rust
  "--version" | "-V" => {
      println!("snip-desktop-native {}", env!("CARGO_PKG_VERSION"));
      std::process::exit(0);
  }
  ```
  並同步在 `--help` 輸出中列出 `-V, --version`。

### 5.2 缺陷 2：Windows Subsystem 未設定（Release 模式跳出 Console 視窗）

- **現況**：
  `crates/desktop-native/src/main.rs` 頂部未加入 Windows Subsystem 屬性。
- **問題行為**：
  在 Windows 上以 Release 模式點擊 `snip-desktop-native.exe` 時，Windows 會預設為該程式開啟一個全黑的 `cmd.exe` 控制台視窗，影響原生桌面體驗。
- **建議修復**：
  在 `crates/desktop-native/src/main.rs` 頂部加入：
  ```rust
  #![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
  ```

### 5.3 缺陷 3：未知 CLI 參數靜默吞沒

- **現況**：
  `_ => {}` 忽略所有未知參數。若使用者輸入拼錯的參數（例如 `--versio` 或 `--wrokspace`），會無預警以預設 normal 模式啟動。
- **建議修復**：
  對無法識別的選項回報錯誤並以 exit code 2 結束，符合標準 CLI 規範。

---

## 6. 原生作業系統自動化前置條件與未驗證環境說明

依據規範，各平台真實視窗輸入、剪貼簿與單一實例（single-instance）測試有具體之未驗證環境前置條件，嚴禁宣稱已全數通過：

1. **Linux (X11)**：
   - 現況：透過 `./scripts/headless-x11.sh` 提供 Xvfb + lavapipe 虛擬螢幕，配合 `xdotool` 與 Mesa 軟體光柵化進行真實輸入與截圖（`native-smoke`）。
   - 限制：arboard 在 Linux 上啟用 `wayland-data-control`，但 `xdotool` 僅支援 X11。純 Wayland 下的端對端輸入注入目前不可用。
2. **macOS**：
   - 現況：CI 在 `native-candidate-artifacts` 中驗證二進位、`--help`、`--version`、Bundle 結構與 ad-hoc 簽署。編譯最低部署目標為 macOS 11.0，但 macOS 11 實體硬體上的實際執行狀態屬於 UNVERIFIED（GPUI 精確最低版本需實機量測數據支持）。
   - **未驗證環境前置條件**：真實視窗輸入注入需依賴 macOS 輔助使用權限（Accessibility / TCC，例如 `AXUIElement` / `CGEventCreateKeyboardEvent`）。非互動式自動化環境下之 TCC 授權狀態未經實機探針量測前標記為 UNVERIFIED；真實視窗操作未在此 CI 階段被驗證。
3. **Windows**：
   - 現況：CI 驗證 PE 二進位、`--help`、`--version` 與候選 zip 包裝。
   - **未驗證環境前置條件**：Windows runner 在無實體顯示器環境下的視窗焦點行為、Direct3D 11/12 WARP 軟體光柵化支援、以及 Win32 UI Automation / `SendInput` 事件注入能力未經實機探針量測前標記為 UNVERIFIED；真實視窗操作未在此 CI 階段被驗證。
4. **安裝包與回退狀態**：
   - 現況：穩定版 Tauri 發布資產原封不動保留於 release 工作流程中；原生候選產物僅作為 CI artifact 保存。
   - 待完成項：原生 NSIS / PKG 真正安裝器、單一實例互斥鎖（single-instance mutex）、中文輸入法（IME）驗收仍待 P5 階段完成，不可過早宣稱可無痛全面替代。
