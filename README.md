# snip-sync

兩台只能透過**剪貼簿**互通的電腦之間同步檔案或 commit：在一台複製 → 在另一台預覽並還原。

- 桌面 App：GPUI 原生 Git 工作臺（v0.3.0 起；v0.2.x 為 Tauri 版）
- CLI：`snip`
- 與 IDE 套件 [ClipCode](https://github.com/audichuang/ClipCode)（IntelliJ）和 [ClipCodeVSCode](https://github.com/audichuang/ClipCodeVSCode)（VS Code）用同一種剪貼簿格式，可互相還原

## 安裝

### macOS（Homebrew，建議）

```bash
brew install --cask audichuang/tap/snip-sync   # 桌面 App
brew install audichuang/tap/snip-cli           # CLI（snip）
```

更新：`brew upgrade --cask snip-sync`、`brew upgrade snip-cli`。

### ⚠️ macOS：打開就閃退／「已損毀，無法打開」

App 沒有 Apple 開發者憑證，只有 ad-hoc 簽章、未經公證。從網路下載的 App 會被加上隔離屬性（quarantine），Gatekeeper 檢查不過就會直接閃退，或顯示「已損毀」。用 `xattr` 清掉隔離屬性，就能跳過這項檢查：

```bash
xattr -cr /Applications/snip-sync.app
```

然後重新打開。每次**手動下載**新版都要再跑一次；Homebrew 安裝的會在安裝後自動處理。

也可以改用圖形介面：系統設定 → 隱私權與安全性 → 「強制打開」（Open Anyway）。

### 手動下載

從 [Releases](https://github.com/audichuang/snip-sync/releases) 下載：

| 平台 | 桌面 App | CLI |
|---|---|---|
| macOS Apple Silicon | `snip-sync_mac_arm.dmg` | `snip-aarch64-apple-darwin.tar.gz` |
| macOS Intel | `snip-sync_mac_intel.dmg` | `snip-x86_64-apple-darwin.tar.gz` |
| Windows x64 | `snip-sync-windows-setup.exe`（免管理員權限）或 `snip-sync-windows-x64.zip` | `snip-x86_64-pc-windows-msvc.zip` |
| Linux x64 | `snip-sync-linux-x86_64.tar.gz`（`bin/snip-desktop-native` 放進 PATH；需 glibc 2.39+，即 Ubuntu 24.04 世代） | `snip-x86_64-unknown-linux-gnu.tar.gz` |

- **Windows**：SmartScreen 警告時選「其他資訊」→「仍要執行」。
- **校驗**：`snip-sync-desktop-SHA256SUMS.txt` 列出桌面檔案的 SHA-256。發版時直接使用 CI 驗收過的同一批檔案，不會重新建置（見[打包文件](docs/native-cross-platform-ci-and-packaging.md)）。

### 從 v0.2.x（Tauri 版）升級

- **macOS**：沿用同一個 bundle id，直接覆蓋即可。覆蓋後記得再跑一次上面的 `xattr`。
- **Windows**：安裝檔會偵測舊版（`%LOCALAPPDATA%\snip-sync`），並詢問是否先解除安裝。選「否」則兩版並存，之後可以到「設定 → 應用程式」移除舊版。

## 範圍

- **檔案模式**：與 IDE 套件同格式，覆蓋還原。
- **commit 模式**：把連續 commit 在另一台重播，保留相同的 message、作者、時間與檔案異動。
- 完全雙向，支援 macOS、Windows、Linux。可以打開單一 repo，也可以打開內含多個 repo 的資料夾。
- 不處理傳輸通道、不做分段與雜湊、不偵測衝突、不保留 commit hash、不做自動更新。

## 文件

| 文件 | 內容 |
|---|---|
| [docs/spec.md](docs/spec.md) | 功能規格：檔案模式、commit 模式、介面、驗收條件 |
| [docs/plan.md](docs/plan.md) | 實作規劃：架構、打包、測試 |
| [docs/porting-notes.md](docs/porting-notes.md) | 從 TS 移植到 Rust 的陷阱與已知且接受的差異 |
| [docs/native-workbench-supervision.md](docs/native-workbench-supervision.md) | 原生工作臺的交付狀態與已知限制 |

各模組的說明：[core](crates/core/README.md) · [cli](crates/cli/README.md)。

## 第三方授權

原生工作臺（`crates/desktop-native`）內嵌下列字型，皆採用 [SIL Open Font License 1.1](https://openfontlicense.org)。授權全文與字型檔放在 `crates/desktop-native/assets/fonts/`。

| 字型 | 版本 | 用途 | 授權檔 |
|---|---|---|---|
| [Inter](https://github.com/rsms/inter)（Regular、Italic、SemiBold） | 4.1 | UI 文字（Italic 只用於預覽分頁名稱） | `Inter-OFL.txt` |
| [JetBrains Mono](https://github.com/JetBrains/JetBrainsMono)（Regular） | 2.304 | 程式碼／編輯器 | `JetBrainsMono-OFL.txt` |

字型未經修改，也不單獨販售。圖示的授權隨桌面套件附上。其餘專案程式碼採用 MIT 授權。
