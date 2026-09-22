# snip-sync — 規劃與可行性評估

> 狀態:**規劃階段,尚未開始實作。** 本 repo 目前只有文件,目的是請同事評估可行性。

## 一句話

做一個**獨立的桌面小工具**(Go + Wails v3,常駐系統匣),讓兩台只能透過**剪貼簿**互通的電腦,
以「git commit 級別」同步檔案:在 A 機複製 → 在 B 機預覽並還原。

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
| [docs/plan.md](docs/plan.md) | 完整規劃:架構、打包、測試、分階段計畫 |
| [docs/porting-notes.md](docs/porting-notes.md) | 從 TS 移植到 Go 的技術細節與已知陷阱 |

## 請同事協助評估的問題

最能決定可不可行的是 **Phase 0** 這幾題,請優先看:

1. **兩台電腦之間的剪貼簿通道是什麼?**(RDP、Citrix、VDI、VM 共用剪貼簿…)
   有沒有大小上限?超過時是截斷、失敗、還是無聲無息少掉一段?
2. **公司的 Windows 能不能執行未簽章的 exe?**
   有沒有 AppLocker / WDAC、SmartScreen 強制、或只允許白名單軟體?
3. **WebView2 在公司 Windows 上可用嗎?**(Win10/11 通常內建,但可能被政策移除或鎖版本)
4. **剪貼簿內容有沒有被 DLP 或稽核工具檢查 / 攔截?**
   大量程式碼經過剪貼簿是否違反資安規範?這是最可能讓整個方案不成立的一點。
5. **是否需要把工具給其他人用?** 若需要,就要處理程式碼簽章(Windows 憑證、Apple Developer 帳號與公證)。

其他疑問或反對意見,直接開 issue 或在文件上註記即可。
