# snip-sync — 規劃與可行性評估

> 自 v0.3.0 起，正式版的桌面 App 是 GPUI 原生版（`crates/desktop-native`，產品名 snip-sync），CLI 照舊。
> Tauri 版（`crates/desktop`）仍在 CI 建置與測試以便回退，但不再發布。各平台驗收狀態見 [交付狀態](docs/native-workbench-supervision.md)。

## 一句話

做一個**獨立的桌面小工具**(Rust + Tauri 2,常駐系統匣;技術棧與打包流程比照 [aghub](https://github.com/audichuang/aghub)),讓兩台只能透過**剪貼簿**互通的電腦,
同步檔案或 commit:在任一台複製 → 在另一台預覽並還原。

## 為什麼不繼續只用 IDE 套件

現在的做法是兩個 IDE 套件:
- IntelliJ 外掛 [ClipCode](https://github.com/audichuang/ClipCode)(Kotlin)
- VS Code 擴充 [Snipcode / ClipCodeVSCode](https://github.com/audichuang/ClipCodeVSCode)(TypeScript)

兩者用同一種剪貼簿格式、可以互相還原,但這代表**同一套規則有兩份實作**,
大部分維護成本都花在讓兩邊逐位元組一致(Java 與 JS 的 regex、trim、路徑語意都不同)。
實際需求其實只是「兩台電腦之間同步」,IDE 對使用者來說主要是看異動用。
一個獨立工具只有一份實作,跨平台也只要把那一份在三個平台測過即可。

兩個 IDE 套件**不會被取代**,新工具與它們使用同一個線上格式,互相相容。

## 文件

| 文件 | 內容 |
|---|---|
| [docs/spec.md](docs/spec.md) | **功能規格(定稿)**:檔案模式、commit 模式、介面、驗收條件 |
| [docs/plan.md](docs/plan.md) | 實作規劃:架構、打包、測試、分階段計畫 |
| [docs/porting-notes.md](docs/porting-notes.md) | 從 TS 移植到 Rust 的技術細節與已知陷阱 |

各模組的規範寫在模組自己的 README:[core](crates/core/README.md) · [cli](crates/cli/README.md) · [desktop](crates/desktop/README.md)。

## 安裝

從 [Releases](https://github.com/audichuang/snip-sync/releases) 下載，或用 Homebrew（macOS）：

```bash
brew install --cask audichuang/tap/snip-sync   # 桌面 App
brew install audichuang/tap/snip-cli           # CLI（snip）
```

| 平台 | 桌面 App | CLI |
|---|---|---|
| macOS Apple Silicon | `snip-sync_mac_arm.dmg`（或 `snip-sync_mac_arm.app.tar.gz`） | `snip-aarch64-apple-darwin.tar.gz` |
| macOS Intel | `snip-sync_mac_intel.dmg`（或 `snip-sync_mac_intel.app.tar.gz`） | `snip-x86_64-apple-darwin.tar.gz` |
| Windows x64 | `snip-sync-windows-setup.exe`（免管理員權限，裝到 `%LOCALAPPDATA%\Programs\snip-sync`）或 `snip-sync-windows-x64.zip` | `snip-x86_64-pc-windows-msvc.zip` |
| Linux x64 | `snip-sync-linux-x86_64.tar.gz`（`bin/snip-desktop-native` 放進 PATH；需 glibc 2.39 以上，即 Ubuntu 24.04 世代） | `snip-x86_64-unknown-linux-gnu.tar.gz` |

`snip-sync-desktop-SHA256SUMS.txt` 列出桌面檔案的 SHA-256；這些檔案就是 CI 驗收過的同一批位元組，發版時不重新建置（見 [打包文件](docs/native-cross-platform-ci-and-packaging.md)）。

**未簽章**：沒有 Apple／Windows 憑證，macOS 版只有 ad-hoc 簽章、未公證。

- macOS 第一次開啟若被擋：系統設定 → 隱私權與安全性 → 「強制打開」（Open Anyway）；舊版 macOS 可在 Finder 對 App 按右鍵 → 打開。或直接移除隔離屬性：`xattr -cr /Applications/snip-sync.app`。
- Windows SmartScreen 警告：「其他資訊」→「仍要執行」。
- 從 v0.2.x（Tauri 版）升級：macOS 的 `snip-sync.app` 沿用同一個 bundle id，直接覆蓋；Windows 新安裝位置不同，舊的 Tauri 版請從「應用程式」另外解除安裝。

## 已定案的範圍(2026-09-23)

- 兩種模式:**檔案模式**(與 IDE 套件同格式、覆蓋還原)與 **commit 模式**
  (連續 commit 在另一台重播成同樣 message、作者、時間與檔案異動)。
- 完全雙向;macOS、Windows、Linux。
- 不管傳輸通道、不做分段與雜湊、不做衝突偵測、不做精確模式、不保留 commit hash。
- 只給自己用:不做簽章公證、自動更新。正式版發到 Homebrew tap(`brew install --cask audichuang/tap/snip-sync`、`brew install audichuang/tap/snip-cli`)。

其他疑問或反對意見,直接開 issue 或在文件上註記即可。
