# snip-sync — 規劃與可行性評估

> 狀態:**規格已定稿([docs/spec.md](docs/spec.md)),尚未開始實作。**

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

## 已定案的範圍(2026-09-23)

- 兩種模式:**檔案模式**(與 IDE 套件同格式、覆蓋還原)與 **commit 模式**
  (連續 commit 在另一台重播成同樣 message、作者、時間與檔案異動)。
- 完全雙向;macOS、Windows、Linux。
- 不管傳輸通道、不做分段與雜湊、不做衝突偵測、不做精確模式、不保留 commit hash。
- 只給自己用:不做簽章公證、自動更新、Homebrew。

其他疑問或反對意見,直接開 issue 或在文件上註記即可。
