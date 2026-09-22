# 完整規劃

## 1. 目標與範圍

**要做到的:**
- 在來源機上把「一組檔案」放進剪貼簿,在目標機上預覽後還原成同樣的檔案樹。
- 「一組檔案」至少涵蓋 **git commit 級別**:
  working tree 的變更、staged(index)、單一 commit、commit 區間(`a..b`)、merge commit。
- Windows、macOS 都能原生執行;Linux 盡量支援。
- 低資源:常駐時的記憶體與 CPU 越少越好,不依賴 Node / JVM / Electron。

**明確不做的:**
- 不取代兩個 IDE 套件。
- 不做即時同步或網路傳輸,只走剪貼簿(這正是限制條件)。
- 不追求「還原結果與原檔逐位元組相同」:目前的線上格式在設計上不保留檔尾換行、
  前後空行,CRLF 會變成 LF。這是現有格式的已知限制,維持不變才能與 IDE 套件互通。
  要做到逐位元組相同必須改格式,屬於另一個決定。

## 2. 架構

```
┌──────────── snip-sync(單一執行檔)────────────┐
│ Wails v3                                      │
│  ├─ 系統匣選單(原生)                            │
│  └─ 預覽 / 確認視窗(TS + Svelte,前端)            │
│             │  Wails binding                   │
│ Go 核心 ◄────┘                                  │
│  ├─ format   payload 產生與解析                  │
│  ├─ paths    路徑解析、containment、symlink 防護   │
│  ├─ restore  plan 與執行、非 UTF-8 / placeholder 防護│
│  ├─ gitsrc   git plumbing 讀取 commit 級內容       │
│  └─ clip     系統剪貼簿(Windows 走 CF_UNICODETEXT)│
│ CLI:snip copy / snip paste(同一組 Go 函式)        │
└───────────────────────────────────────────────┘
```

- **Go 做所有邏輯。** 前端與系統匣只是薄殼,只呼叫已經被測過的 Go 函式。
  這是讓 GUI 難以自動化測試的部分影響降到最低的關鍵(見第 5 節)。
- **為什麼用 Go:** 單一靜態執行檔、常駐記憶體小、`GOOS`/`GOARCH` 即可跨平台編譯、
  工具鏈小且編譯快。Rust 的優勢在此用不到,編譯佔用的磁碟與時間卻明顯較高。
- **為什麼用 Wails v3 而不是 v2:** v2 沒有內建系統匣,另接 systray 套件會與 Wails 搶 macOS
  主執行緒。v3 內建系統匣選單,也支援「點圖示彈出、對齊圖示位置的視窗」。
  v3 目前(2026-09)是 **beta**:desktop API 已穩定、已有人用於正式環境,但要鎖定版本並充分測試。
- **沒有前後端分離的伺服器。** TS 只是 Wails 視窗裡的畫面;Go 與 TS 在同一個程序內透過
  binding 呼叫。

### 移植來源:VS Code 那份 TypeScript

VS Code 擴充的核心模組**不依賴 VS Code API**,已確認沒有任何 `vscode` import:
`clipboardFormat.ts`、`pathResolver.ts`、`restore.ts`、`copy.ts`、`filterMatcher.ts`、
`gitCopy.ts`、`graphCopy.ts`、`gitHistory.ts`、`gitContent.ts`、`catFile.ts`、`restoreBase.ts`。

它也是線上格式的權威來源(共用 fixture 由它產生),所以以它為藍本移植。
Kotlin 版的規則與它對等(有 fixture 釘住),Kotlin 多出來的能力(外部 library、反編譯 `.class`、
IDE 的 Local Changes)都綁在 IntelliJ 平台上,這個工具用不到。

唯一要換掉的一層:TS 版的 commit 讀取是透過 VS Code git 擴充的 Repository 物件
(`repo.log`、`diffBetweenWithStats`、`buffer`、`show`)。Go 版改為直接呼叫 git plumbing,
見 [porting-notes.md](porting-notes.md)。

### 與 IDE 套件的相容性:共用 contract fixture

兩個 IDE 套件共用一份 golden fixture,並以 SHA 鎖定、兩邊逐位元組相同:
[`clipboard-contract.json`](https://github.com/audichuang/ClipCodeVSCode/blob/main/test/fixtures/clipboard-contract.json)。
目前包含:

| 區塊 | 內容 | 筆數 |
|---|---|---|
| `buildCases` | 檔案清單 + 設定 → payload 位元組 | 24 |
| `parseCases` | payload → 解析出的檔案 | 34 |
| `tokenCases` | 通知裡的字元 / 行 / 字 / token 統計 | 20 |
| `pathCases` | 剪貼簿路徑 → 寫入與刪除目標(固定的目錄佈局) | 72 |

Go 版成為**第三個使用者**:跑同一份 fixture、鎖同一個 SHA。這樣不必從零證明相容,
fixture 通過就代表與兩個套件互通。

## 3. 功能

**CLI**
```
snip copy <路徑…>                 # 目前的檔案內容
snip copy --staged                # index 內容
snip copy --commit <sha>          # 一個 commit 的變更(merge 取所有 parent 的聯集)
snip copy --range <a>..<b>        # 一段 commit 區間
snip paste --dry-run              # 只列出會新增 / 覆寫 / 刪除 / 跳過哪些檔案與原因
snip paste --apply [--overwrite | --skip-existing]
```

**桌面 App(Wails)**
- 系統匣:複製上次的選取、從剪貼簿貼上、開啟預覽視窗。
- 預覽視窗:列出每個檔案的動作與原因(例如「目標不是 UTF-8,不覆寫」),可逐檔勾選,
  覆寫的檔案可看 diff,確認後才執行。

**分段與完整性(待定的設計)**
若兩台電腦間的剪貼簿有大小上限(常見於 RDP / VDI),大 payload 需要分段,並附上整份內容的雜湊,
讓貼上端能確認收到的是完整的。**設計原則:** 分段與雜湊是選用的外層包裝,只在 Go 對 Go 時使用;
單段的純 payload 必須與兩個 IDE 套件互通。外層包裝的格式要在 Phase 3 開工前定下來。

## 4. 原生打包

| | Windows | macOS |
|---|---|---|
| 產物 | 單一 `.exe`,可選 NSIS 安裝檔 | `.app`,可做 universal(arm64 + x64) |
| WebView | WebView2(Win10/11 內建) | 系統 WKWebView,不必另裝 |
| 在哪裡編譯 | 可從其他平台跨編譯(不需要 cgo),但仍要在 Windows 上測 | **必須在 macOS 上編**(cgo + Xcode Command Line Tools) |
| 未簽章的後果 | SmartScreen 警告;公司政策可能直接禁止執行 | Gatekeeper 阻擋;自用可右鍵「打開」,正式散佈需 Apple Developer 帳號並公證 |

GitHub Actions 的 `windows-latest` 與 `macos-latest` runner 可以各自產出原生安裝包。
兩個 IDE 套件的 repo 已經在這三種 runner 上跑完整測試,證明環境可用。

## 5. 測試策略

| 層 | 內容 | 能否自動化 |
|---|---|---|
| 1 核心 | Go 版跑共用 contract fixture(build / parse / token / path) | ✅ 三平台 |
| 2 CLI E2E | 建 fixture repo(一般 commit、刪除、rename、octopus merge)→ `snip copy` → `snip paste` 到空目錄 → 比對檔案樹 | ✅ 三平台 |
| 3 跨工具 | 同一 repo 與設定下,Go 產生的 payload 等於 TS 產生的;Go 能還原 TS 與 Kotlin 產生的 payload,反之亦然 | ✅ |
| 4 真實剪貼簿 | 寫入系統剪貼簿再讀回:Unicode、大 payload、換行 | ✅ Windows / macOS runner 有桌面;Linux 用 xvfb + xclip |
| 5 前端畫面 | Wails 前端換成 mock binding,在 headless Chrome 跑 | ✅ |
| 6 操作真實 App | Windows:WebView2 可開遠端除錯 port,讓 Playwright 透過 CDP 操作 | ✅ 僅 Windows |
| | macOS 的 WKWebView 沒有 WebDriver 也沒有 CDP | ❌ 手動 |
| | 系統匣選單本身 | ❌ 兩個平台都難以自動化 |

**對策:** 所有邏輯放在 Go、由第 1–3 層覆蓋;無法自動化的部分只剩「按鈕有沒有接對」,
用一份簡短的手動檢查清單涵蓋,每次發布前在 macOS 上跑一次。

## 6. 分階段計畫(每一階段通過驗收才進下一階段)

### Phase 0 — 可行性驗證
- 回答 README 裡的五個問題(剪貼簿通道與上限、未簽章 exe、WebView2、DLP、是否散佈)。
- 鎖定 Wails v3 版本。
- 做最小 demo:系統匣 + 一個視窗 + 寫入 / 讀取剪貼簿,在**兩台實際的電腦**上跑起來。
- **量測剪貼簿上限**:送 1 KB、100 KB、1 MB、10 MB,在另一端比對雜湊。

**驗收:** 兩台實機都能跑 demo,且知道剪貼簿的實際上限。任一題答案是否定的,就在這裡停下重新評估。

### Phase 1 — Go 核心
移植格式、路徑解析、restore 的 plan 與執行、過濾規則、統計。

**驗收:** 共用 contract fixture 全部通過,並鎖定相同 SHA;三平台 CI 綠燈。

### Phase 2 — Git commit 級複製
working tree、staged、單一 commit、commit 區間;merge 取每個 parent 的聯集;刪除的檔案帶刪除前內容;
rename 標為 `[MOVED]`;偵測 shallow clone;非 UTF-8 跳過並計數;大小與數量上限;
讀不到的 placeholder 不計為已複製、也不佔數量上限。

**驗收:** 第 2、3 層測試通過;Go 與 TS 對同一 repo 產生的 payload 逐位元組相同
(先做差分,穩定後凍結成 fixture 的新區塊)。

### Phase 3 — CLI
上面第 3 節的指令;定下分段 / 雜湊外層包裝的格式。

**驗收:** 第 2–4 層測試通過;在兩台實機之間實際同步一個真實 repo 的 commit 區間。

### Phase 4 — Wails App
系統匣、預覽視窗(逐檔勾選、覆寫 diff、原因說明)、確認後執行。

**驗收:** 第 5、6 層測試通過;macOS 手動檢查清單通過。

### Phase 5 — 打包與發布
三平台 CI;macOS universal `.app`、Windows exe / NSIS;推 tag 時發布。簽章與公證視是否散佈決定。

**驗收:** 從 Release 下載的安裝包能在兩台實機上安裝並完成一次完整同步。

## 7. 風險

| 風險 | 影響 | 在哪一階段確認 |
|---|---|---|
| 公司資安政策禁止未簽章 exe、或 DLP 攔截剪貼簿 | 方案不成立 | Phase 0 |
| 剪貼簿通道有大小上限且會無聲截斷 | 還原出殘缺檔案 | Phase 0(量測)、Phase 3(分段 + 雜湊) |
| Wails v3 仍是 beta | API 變動、平台 bug | Phase 0 鎖版本;Phase 4 前再確認 |
| Go 移植產生語意差異(regex、trim、Unicode、路徑) | 與 IDE 套件不相容 | Phase 1,由 fixture 抓出 |
| macOS GUI 無法自動化測試 | GUI 退化只能靠手動發現 | 以架構緩解:GUI 只當薄殼 |
| 需要散佈時的簽章成本 | Apple Developer 年費、Windows 憑證 | Phase 5 |
