# Native Cross-Platform CI & Packaging Specification

日期：2026-09-25，2026-09-27 更新為 v0.3.0 發布管線。狀態：**原生版取代 Tauri 成為 release 桌面資產；新的打包順序、Windows 安裝檔與 `release.yml` 搬運流程尚未在 GitHub Actions 實跑（UNVERIFIED）**。

本文件記錄原生跨平台 CI 建置相依性稽核、發布管線與雜湊綁定（macOS arm/intel、Linux、Windows）、產物驗證工具、以及針對 UI 擁有者（`crates/desktop-native`）的唯讀檢視 findings 與整合交接清單。

驗證器修正（Mach-O fat、套件路徑綁定、DMG 失敗即關閉）落在 `fix/native-package-verification`。`feature/native-release-pipeline` 把打包改為使用者看到的 snip-sync 名稱、加上 Windows 安裝檔、把 smoke 移到最終（已簽章／已安裝）執行檔上，並讓 `release.yml` 直接發布 CI 驗收過的產物。macOS `hdiutil` / `codesign`、Windows Inno Setup 與 release 搬運都還沒在 GitHub Actions 實跑；在 Linux 上跑過的單元測試不能解讀成 Darwin／Windows 執行期已通過。

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
- [`.github/workflows/ci.yml`](../.github/workflows/ci.yml) 中的 `lint-rust`、`test`、`native-smoke` 與 `native-acceptance`（Linux 發布包由它產生）均已更新為安裝完整的原生相依套件清單。

### 1.2 macOS (aarch64-apple-darwin, x86_64-apple-darwin)

- **SDK 相依性**：GPUI 在 macOS 上依賴系統框架（Cocoa, Metal, CoreGraphics, CoreText, AppKit, QuartzCore），GitHub Actions 的 `macos-latest` runner 已內建完整 macOS SDK，無須額外透過 Homebrew 安裝 C 函式庫。
- **最低部署版本 (Deployment Target) 與實機執行未驗證聲明**：
  - 選定之編譯建置最低目標為 macOS 11.0 Big Sur（Rust 官方 Tier 1 Apple Silicon 最低支援版本），CI 建置環境與產物中繼資料（`Info.plist` 之 `LSMinimumSystemVersion`）統一且明確設定 `MACOSX_DEPLOYMENT_TARGET=11.0`。
  - **在 macOS 11 實體硬體上的實際執行狀態屬於 UNVERIFIED**：GPUI 0.2.2 底層依賴 Metal、DisplayLink 與近代 macOS framework API，其精確最低支援版本需要實機硬體驗證數據（empirical hardware evidence）。目前 CI 設定在 macOS 26 系列 runner 上進行編譯與 CLI smoke；本次新 matrix 尚未實際執行，不可宣稱已在 macOS 11 上實測驗收通過。
- **目標架構**：Apple Silicon (`aarch64-apple-darwin`) 使用 `macos-latest`；Intel (`x86_64-apple-darwin`) 使用 `macos-26-intel`。兩個 leg 都必須實際執行 DMG 內已簽章 binary 的 CLI smoke；runner 架構不符時失敗，不跳過。
- **執行期限制**：此 matrix 尚待 GitHub Actions 實際執行。CLI 與套件驗證不代表 macOS GUI／IME 已驗證，詳見 [CI 整合收據](native-ci-integration.md)。

### 1.3 Windows (x86_64-pc-windows-msvc)

- **SDK 相依性**：GPUI 在 Windows 上依賴 Direct3D 11/12、DXGI、Direct2D/DirectWrite 與 Win32 API。GitHub Actions 的 `windows-latest` runner 已內建 MSVC 與 Windows SDK。
- **子系統 (Subsystem)**：若編譯為 release binary，必須處理 Console 視窗抑制（見第 4 節）。

---

## 2. 發布管線（v0.3.0 起：原生版取代 Tauri 成為正式桌面 App）

v0.3.0 起，release 的桌面資產改為原生 GPUI App（`crates/desktop-native`，執行檔 `snip-desktop-native`，使用者看到的產品名 `snip-sync`）。CLI（`snip`）照舊由 `release.yml` 的 `build-cli` 建置發布。Tauri 版（`crates/desktop`）仍由 `ci.yml` 的 lint／test／`desktop-e2e` 建置與測試以便回退，但 `release.yml` 不再建置或發布它；`just bump` 仍同步 `tauri.conf.json` 的版本，讓它保持可建置。

### 2.1 雜湊綁定：發布的就是驗收過的位元組

交付規格（`native-workbench-delivery-spec.md` §9 發布責任、§10 最末條）要求「發布 native 產物須沿用 CI 實際驗收的執行檔並核對 hash、run 與 artifact 身分；相同 source SHA 的重新建置不等於相同已驗收執行檔；簽章改變 binary 時，驗收必須綁定最終 binary」。做法：

1. **每個 target 只建置一次，而且在 `ci.yml`**。`release.yml` 不編譯原生版。
2. **驗收跑在最終產物上**：
   - Linux：`native-acceptance` 先用 `run_native_acceptance.py --gate all` 建置並凍結一個 release 執行檔，跑 IME、18 個協作案例與 functional-short 資源 gate；通過後才用 **該凍結執行檔**（`build-receipt.json` 的 `binary`）打包、`verify_artifacts.py` 審計、比對 tarball 內執行檔 sha256 等於 receipt 的 `sha256`，並對解開後的執行檔跑 `smoke_native.py`。上傳為 `native-candidate-x86_64-unknown-linux-gnu`；證據另上傳為 `native-acceptance-linux`。
   - macOS：`codesign` 會改寫 Mach-O，所以順序是 建置 → 打包（含 ad-hoc 簽章、DMG）→ `verify_artifacts.py`（在 Darwin 掛載 DMG、`codesign --verify --deep --strict`）→ 掛載 **要發布的 DMG**，對裡面的 `snip-sync.app/Contents/MacOS/snip-desktop-native` 跑 smoke，並要求它與 `.app.tar.gz` 內的執行檔 sha256 相同。Intel leg 在 `macos-26-intel` 上實際執行，架構不符即失敗。
   - Windows：打包（zip + Inno Setup 安裝檔）→ `verify_artifacts.py` → 以 `/VERYSILENT /DIR=…` 真的執行 **要發布的安裝檔**，要求安裝出的 exe、zip 內的 exe、建置出的 exe 三者 sha256 相同，再對安裝出的 exe 跑 smoke。
   - 每個 target 的輸出目錄都有 `SHA256SUMS-<target>.txt`，在跑上述檢查之前就寫好。
3. **`release.yml` 只搬運**：`verify-ci` 找出 tag SHA 在 `main` 上最新一次成功的 push `ci.yml` run（`event=push`、`head_branch=main`、`head_sha` 相符、`conclusion=success`；整個 run 綠代表上面每個 gate 都過了），輸出 run id。`publish-native` 以 `gh run download` 取該 run 的四個 `native-candidate-*` 與 `native-acceptance-linux`，然後：
   - tag 版本必須等於該 SHA 的 `Cargo.toml` 版本（CI 用它建置，發版不再改版號）；
   - 每個目錄的檔案集合必須剛好是下表的檔名，`sha256sum --check --strict` 對 `SHA256SUMS-<target>.txt` 全數通過；
   - Linux／Windows 再跑一次 `verify_artifacts.py --dir`；macOS 的 `.app.tar.gz` 跑結構檢查（DMG 無法在 Linux 掛載，它的內容已在 CI 的 macOS 上驗過，位元組由 SHA256SUMS 釘住）；
   - Linux：`acceptance.json` 為 `PASSED`／`gate=all`，`build-receipt.json` 的 `sourceSha` 等於 tag SHA，tarball 內執行檔 sha256 等於 receipt 的 `sha256`；
   - 原封不動上傳，另產生 `snip-sync-desktop-SHA256SUMS.txt`，並在 release notes 附上 CI run 連結、Linux 已驗收執行檔 sha256、全部 sha256 與未簽章說明。
4. 因此 RC tag（例如 `v0.3.0-rc.1`）也必須先把 `Cargo.toml` bump 成同一字串並在 `main` 上跑綠 CI；`just release` 的 CI 等待上限因 `native-acceptance`（120 分鐘 timeout）調為 150 分鐘。

### 2.2 發布資產（檔名固定）

| 平台 | Target Triple | Release 資產 | 內部結構 |
| --- | --- | --- | --- |
| **macOS (Apple Silicon)** | `aarch64-apple-darwin` | `snip-sync_mac_arm.dmg`<br>`snip-sync_mac_arm.app.tar.gz` | `snip-sync.app`（`CFBundleIdentifier=com.audichuang.snip-sync`、ad-hoc 簽章、`Contents/MacOS/snip-desktop-native`、`Contents/Resources/icon.icns`）<br>DMG 根目錄含 `/Applications` 連結 |
| **macOS (Intel)** | `x86_64-apple-darwin` | `snip-sync_mac_intel.dmg`<br>`snip-sync_mac_intel.app.tar.gz` | 同上（Mach-O x86_64） |
| **Linux (x64)** | `x86_64-unknown-linux-gnu` | `snip-sync-linux-x86_64.tar.gz` | `snip-sync-<version>/bin/snip-desktop-native`<br>`snip-sync-<version>/share/applications/snip-sync.desktop`<br>`snip-sync-<version>/README.txt` |
| **Windows (x64)** | `x86_64-pc-windows-msvc` | `snip-sync-windows-setup.exe`<br>`snip-sync-windows-x64.zip` | 安裝檔：Inno Setup，per-user（不需 UAC），裝到 `%LOCALAPPDATA%\Programs\snip-sync\snip-desktop-native.exe`，開始功能表捷徑「snip-sync」<br>zip：`snip-sync/snip-desktop-native.exe`、`snip-sync/README.txt` |
| 全部 | — | `snip-sync-desktop-SHA256SUMS.txt` | 上列桌面檔案的 sha256 |

DMG 檔名沿用 Tauri 時代的 `snip-sync_mac_arm.dmg`／`snip-sync_mac_intel.dmg`，舊下載連結不斷。Tauri 的 `snip-sync-linux.AppImage` 與 Tauri 的 `snip-sync_<version>_<arch>.dmg` 不再產生。Homebrew cask 改指向 `snip-sync_mac_#{arch}.dmg`（`arch arm: "arm", intel: "intel"`）、`app "snip-sync.app"`，加上 `depends_on macos: ">= :big_sur"` 與 Gatekeeper caveats。

**Bundle id 沿用 Tauri 的 `com.audichuang.snip-sync`**。好處：cask 的 `zap` 路徑仍正確；同名同 id 直接覆蓋 `/Applications/snip-sync.app`，LaunchServices 視為同一 App 升級；回退到 Tauri 版也是覆蓋回去。代價：ad-hoc 簽章的 designated requirement 是 cdhash，TCC 權限（例如輔助使用）本來就不會跨版本延續；同時保留兩個同 id 的 App 會讓 LaunchServices 混淆，所以不要並存。Windows 安裝檔刻意 **不** 沿用 Tauri 的 `%LOCALAPPDATA%\snip-sync` 與其 NSIS 解除安裝項：兩者並存時「應用程式」清單會出現兩筆，舊版需手動解除安裝，但回退不會互相覆蓋檔案。

### 2.3 簽章

沒有 Apple／Windows 憑證，發布未簽章版本。macOS 以 `codesign --force --deep --sign -` ad-hoc 簽章（arm64 必須有簽章才能執行），未公證；Gatekeeper 第一次會擋，使用者需「系統設定 → 隱私權與安全性 → 強制打開」或 `xattr -cr /Applications/snip-sync.app`（README 與 release notes 都有寫）。Windows 安裝檔與 exe 未簽章，SmartScreen 會警告。

### 2.4 平台工具

不引入第三方打包框架：
1. **macOS**：自建 `.app` 與 `Info.plist`（`LSMinimumSystemVersion=11.0`、圖示取自 `crates/desktop/src-tauri/icons/icon.icns`），`codesign` ad-hoc 簽章，`hdiutil create -volname snip-sync -format UDZO` 製作 DMG。缺 `codesign` 或 `hdiutil` 時 `scripts/package_native.sh` 失敗。
2. **Linux**：POSIX tar，保留 `0755`。不做 AppImage：需要額外下載 appimagetool 與 FUSE，tarball 已足夠。執行檔在 `ubuntu-24.04` 建置（`native-acceptance` 的 runner），因此需要 glibc 2.39 以上；Tauri 版原本在 22.04 建置，這是已知的相容範圍縮小。
3. **Windows**：Python `zipfile` 產生 zip；安裝檔用 Inno Setup（GitHub `windows-2025` 映像預裝 `C:\Program Files (x86)\Inno Setup 6\ISCC.exe`，NSIS 沒有預裝），腳本在 `crates/desktop-native/packaging/windows/snip-sync.iss`，圖示取自 Tauri 的 `icon.ico`。Git Bash 會改寫 `/D`、`/O` 開頭的參數，所以呼叫時設 `MSYS_NO_PATHCONV=1` 並用 `cygpath -w` 轉路徑。找不到 `ISCC.exe` 時打包失敗，不會少產一個格式。exe 本身沒有內嵌圖示（需要改 `crates/desktop-native` 的建置腳本，不在本次範圍）。

### 2.5 平台獨立 Checksum

每個 target 的打包輸出目錄生成 `SHA256SUMS-${TARGET}.txt`，避免多 target 收集時檔名碰撞。驗證工具要求完整覆蓋（目錄下所有產物都要登記，checksum 檔不可為空）。

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
   - macOS tarball 的執行檔必須是 `snip-sync.app/Contents/MacOS/snip-desktop-native`，版本來自同一個 bundle 的 `Contents/Info.plist`（`CFBundlePackageType=APPL`、`CFBundleExecutable=snip-desktop-native`、`CFBundleShortVersionString` 與 `CFBundleVersion`）。
   - Linux tarball 的執行檔必須是 `snip-sync-<version>/bin/snip-desktop-native`，應用程式版本來自同一個 package 的 `README.txt`（精確一行 `Version: <version>`）。`share/applications/snip-sync.desktop` 要有 `[Desktop Entry]`、`Type=Application`，且 `Exec` 啟動 `snip-desktop-native`。`.desktop` 的 `Version=1.0` 是 Desktop Entry 規格版本，不是應用程式版本。
   - Windows zip 的執行檔必須是 `snip-sync/snip-desktop-native.exe`，版本來自同一個目錄的 `README.txt`。
   - 誘餌 plist / README、重複候選、缺少執行權限、setuid/setgid、以及 symlink 一律拒絕。
   - 有寫明的 target triple 就決定架構、二進位格式、執行檔名稱與產物種類。再傳入的架構、格式或執行檔名稱若跟該 target 衝突，直接拒絕，不能蓋過 target。`arm64` 與 `aarch64` 是同一架構。沒有 target 時維持結構檢查。
4. **macOS DMG 與候選集合**：
   - Apple 目錄必須同時有該 target 的 `.app.tar.gz` 與 `.dmg`（檔名見第 2.2 節），且沒有其他產物。Linux 目錄只接受 `snip-sync-linux-x86_64.tar.gz`；Windows 目錄只接受 `snip-sync-windows-x64.zip` 與 `snip-sync-windows-setup.exe`。安裝檔只做結構檢查（格式正確的 PE、至少 1 MiB；Inno Setup 的 stub 是 32 位元 x86，不比對架構），內容與 exe 的綁定由 CI 的實際安裝加 sha256 比對負責。
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
   - 現況：CI 在 `native-candidate-artifacts` 中驗證二進位、DMG 內已簽章執行檔的 `--help`／`--version`、Bundle 結構與 ad-hoc 簽署。編譯最低部署目標為 macOS 11.0，但 macOS 11 實體硬體上的實際執行狀態屬於 UNVERIFIED（GPUI 精確最低版本需實機量測數據支持）。
   - **未驗證環境前置條件**：真實視窗輸入注入需依賴 macOS 輔助使用權限（Accessibility / TCC，例如 `AXUIElement` / `CGEventCreateKeyboardEvent`）。非互動式自動化環境下之 TCC 授權狀態未經實機探針量測前標記為 UNVERIFIED；真實視窗操作未在此 CI 階段被驗證。
3. **Windows**：
   - 現況：CI 驗證 PE 二進位、zip、Inno Setup 安裝檔的靜默安裝，以及安裝出的 exe 的 `--help`／`--version`。
   - **未驗證環境前置條件**：Windows runner 在無實體顯示器環境下的視窗焦點行為、Direct3D 11/12 WARP 軟體光柵化支援、以及 Win32 UI Automation / `SendInput` 事件注入能力未經實機探針量測前標記為 UNVERIFIED；真實視窗操作未在此 CI 階段被驗證。
4. **安裝包與回退狀態**：
   - 現況：v0.3.0 起 release 只發布原生桌面資產（第 2 節）；Tauri 版留在 `crates/desktop`，CI 照常建置與跑 E2E，需要回退時把 `release.yml` 的 `build-tauri` job 從 v0.2.0 版還原即可。
   - 未涵蓋：macOS／Windows 真視窗輸入、單一實例互斥鎖仍未在 CI 驗證；Linux 的 IME 與協作驗收由 `native-acceptance` 負責。
