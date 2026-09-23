# 完整規劃

## 1. 目標與範圍

> 功能的權威定義在 [spec.md](spec.md),本文件講怎麼做。

**要做到的:**
- 在一台電腦複製,在另一台預覽後貼上還原;完全雙向。
- **檔案模式**:與 IDE 套件相同的格式,覆蓋還原。來源涵蓋 working tree、staged、單一 commit、
  commit 區間(`a..b`)、merge commit。
- **commit 模式**:選一段連續 commit,在另一台重播成同樣 message、作者、時間與檔案異動的 commit。
- macOS、Windows、Linux 都能原生執行。
- 低資源:常駐時的記憶體與 CPU 越少越好,不依賴 Node / JVM / Electron。

**明確不做的:**
- 不取代兩個 IDE 套件。
- 不做即時同步或網路傳輸,只走剪貼簿;剪貼簿通道本身(大小上限、截斷)不在範圍內,不做分段與雜湊。
- 不追求逐位元組還原(不做精確模式):檔案模式沿用現有格式的限制(不保留檔尾換行與前後空行,CRLF 變 LF)。
- 不做衝突偵測與合併,一律覆蓋;不保留 commit hash。
- 不處理簽章、公證、自動更新與 Homebrew(只給自己用)。

## 2. 架構

技術棧與 [aghub](https://github.com/audichuang/aghub) 相同:**Rust + Tauri 2**,
前端 **React 19 + TypeScript + HeroUI v3 + Tailwind CSS v4**(Vite、bun)。

```
┌──────────── snip-sync(Tauri 2 App)──────────────┐
│  ├─ 系統匣選單(Tauri tray-icon,原生)              │
│  └─ 預覽 / 確認視窗(React + HeroUI)                │
│             │  Tauri command(invoke)             │
│ snip-core ◄──┘                                   │
│  ├─ format   payload 產生與解析                    │
│  ├─ paths    路徑解析、containment、symlink 防護     │
│  ├─ restore  plan 與執行、非 UTF-8 / placeholder 防護 │
│  ├─ filter   過濾規則                              │
│  ├─ stats    字元 / 行 / 字 / token 統計             │
│  ├─ gitsrc   git plumbing 讀取 commit 級內容         │
│  └─ clip     系統剪貼簿(arboard)                   │
│ snip(CLI):copy / paste,呼叫同一個 snip-core       │
└─────────────────────────────────────────────────┘
```

### Repo 結構(比照 aghub)

```
Cargo.toml                 # workspace,版本號統一在 [workspace.package]
crates/
  core/                    # snip-core:所有邏輯,純 Rust,不依賴 Tauri
  cli/                     # snip-cli:產出 `snip` 執行檔(clap)
  desktop/                 # Tauri App(沒有獨立的 desktop crate)
    package.json           # React 前端,bun
    src/                   #   pages/ components/ lib/ generated/(ts-rs 產生的 DTO)
    src-tauri/             #   Tauri 後端,Cargo 套件名 snip-sync;commands/ 放 Tauri command
fixtures/                  # 共用 contract fixture 的副本 + SHA
justfile                   # build / test / lint / preflight / bump / release / desktop-bundle
cliff.toml                 # git-cliff 產生 changelog
.github/workflows/ci.yml, release.yml
```

- **所有邏輯都放在 Rust(`snip-core`)。** 前端與系統匣只是薄殼,只呼叫已經測過的 core 函式。
  GUI 很難自動化測試,這樣做可以把它的影響降到最低(見第 5 節)。
- **為什麼用 Rust + Tauri 2:** aghub 已經在正式環境用這套,系統匣、剪貼簿、自動更新、
  開機啟動、單一實例、三平台 CI、簽章與 Homebrew 發布都已經跑通、也踩過坑。
  Tauri 2 是穩定版;原本考慮的 Wails v3 仍是 beta。兩者的畫面都跑在同一種系統 WebView 上,
  UI 能做到的程度一樣,差別在周邊成熟度與可以沿用的現成成果。
  Rust 編譯較慢、較佔磁碟,用 aghub 的 sccache + rust-cache 設定緩解。
- **與 aghub 刻意不同的地方:不做內嵌 HTTP API sidecar。** aghub 的 App 透過 localhost 上的
  `aghub-api` 取資料,因為它要支援遠端 VM。snip-sync 沒有這個需求,Tauri command 直接呼叫
  `snip-core`,少一個程序與一組打包步驟。
- **DTO 用 ts-rs 產生。** core 對外的型別(restore plan、檔案動作、原因、統計)加上 `#[derive(TS)]`,
  由 `bun run generate:dto` 產生到 `crates/desktop/src/generated/`,前端不手寫型別。
- **大型 payload 不經過前端。** 剪貼簿的讀寫都在 Rust 端(`clip` 模組),前端只拿到解析後的
  plan 與統計,避免幾 MB 的字串在 IPC 上來回傳。

### 移植來源:VS Code 那份 TypeScript

VS Code 擴充的核心模組**不依賴 VS Code API**,已確認沒有任何 `vscode` import:
`clipboardFormat.ts`、`pathResolver.ts`、`restore.ts`、`copy.ts`、`filterMatcher.ts`、
`gitCopy.ts`、`graphCopy.ts`、`gitHistory.ts`、`gitContent.ts`、`catFile.ts`、`restoreBase.ts`
(合計約 3,200 行,依賴只有 `node:fs` / `node:path` 與 git 子程序)。

它也是線上格式的權威來源(共用 fixture 由它產生),所以以它為藍本移植。
Kotlin 版的規則與它對等(有 fixture 釘住),Kotlin 多出來的能力(外部 library、反編譯 `.class`、
IDE 的 Local Changes)都綁在 IntelliJ 平台上,這個工具用不到。

唯一要換掉的一層:TS 版的 commit 讀取是透過 VS Code git 擴充的 Repository 物件
(`repo.log`、`diffBetweenWithStats`、`buffer`、`show`)。Rust 版改為直接呼叫 git plumbing,
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
| `restoreCases` | payload → 還原計畫(creates / deletes / skips) | 16 |

鎖定版本:ClipCodeVSCode commit `0aa24c8`(2026-09-23),fixture SHA-256
`df317eb7b412d4bd71222d71d4cd64a1652fbcac2d82468ec417e4ce95ec2468`。
取得 TS 參考原始碼:`git -C ../IntellijPlugin/ClipCodeVSCode archive 0aa24c8 src test scripts AGENTS.md | tar -x -C <目錄>`
(本機 checkout 可能落後,一律以這個 commit 為準)。

Rust 版成為**第三個使用者**:`snip-core` 的整合測試讀同一份 fixture、鎖同一個 SHA。
這樣不必從零證明相容,fixture 通過就代表與兩個套件互通。

## 3. 功能

完整規格見 [spec.md](spec.md)。這裡只記實作上的對應:

| 功能 | 實作 |
|---|---|
| 檔案模式複製 / 貼上 | `snip-core` 的 `format`、`paths`、`restore`,移植自 TS,由 contract fixture 驗證 |
| git 來源(working / staged / commit / range) | `gitsrc`:`git diff-tree` / `git diff` 的 `--raw -z --no-abbrev -M` + `git cat-file --batch`(見 porting-notes 第 5 節) |
| commit 模式複製 | `git rev-list --first-parent` 驗證連續;每個 commit 以 `git diff-tree` 對 first parent 取異動,`git log -1 --format` 取 message、作者、作者時間;序列化成 marker + JSON(`serde_json`) |
| commit 模式貼上 | 依序寫入檔案 → `git add -A -- <paths>` → `git commit --no-verify --allow-empty --author=… --date=… -F - -- <paths>` |
| 預覽的 diff | Rust 端用 `similar` 算好 hunk,前端 `@pierre/diffs` 呈現 |
| 系統匣 | Tauri `tray-icon`;視窗貼齊圖示用 `tauri-plugin-positioner`;關閉視窗時縮回系統匣(同 aghub 的 `minimize_to_tray`) |
| 其他 plugin | 沿用 aghub 的 `single-instance`、`autostart`、`store`、`log`、`dialog`、`opener`;不裝 `updater` |
| 時間軸 / 檔案樹 | `@tomplum/react-git-log`(HTML Grid)、`@rc-component/tree` |
| 版面、i18n | 沿用 aghub(HeroUI、i18next,繁中 / 英文) |

## 4. 原生打包(比照 aghub 的 release.yml)

| | Windows | macOS | Linux |
|---|---|---|---|
| 產物 | NSIS `setup.exe` | `.dmg`,arm64 與 x64 各一份 | `.AppImage` |
| WebView | WebView2(Win10/11 內建) | 系統 WKWebView | WebKitGTK(CI 需安裝) |
| 在哪裡編譯 | `windows-latest` | `macos-latest`(兩個 target) | `ubuntu-22.04` |
| 未簽章的後果 | SmartScreen 警告;公司政策可能直接禁止執行 | Gatekeeper 阻擋;自用可右鍵「打開」 | 無 |

- **發布流程照 aghub:** 推 `vX.Y.Z` tag → `verify-ci` 確認該 commit 的 CI 是綠燈 →
  git-cliff 產生 changelog 並建立 Release → `build-tauri` 四個 target 以 `tauri-action` 打包 →
  `build-cli` 四個 target 編 `snip`、跑 smoke test、打包成 tar.gz / zip → 上傳到 Release。
  (不含 aghub 的 `publish-homebrew` 與 updater 的 `latest.json`。)
  `just bump` 同步 `Cargo.toml`、`package.json`、`tauri.conf.json` 的版本號;`just release` 包辦 tag 與驗證。
- **macOS 簽章:** 比照 aghub 用 ad-hoc(`APPLE_SIGNING_IDENTITY: "-"`),並在 CI 以
  `codesign --verify --deep --strict` 驗證。只給自己用,不做 Apple Developer 憑證與公證。
- **不做:** `tauri-plugin-updater`、Homebrew tap。要給別人用時再從 aghub 的 release.yml 搬過來。
- 本機快速測試用 `just desktop-bundle`,不必走完 tag → CI → 下載(aghub 的 `desktop-dmg` 尚未搬過來)。

## 5. 測試策略

| 層 | 內容 | 能否自動化 |
|---|---|---|
| 1 核心 | `snip-core` 跑共用 contract fixture(build / parse / token / path) | ✅ 三平台 |
| 2 CLI E2E | 檔案模式:建 fixture repo(一般 commit、刪除、rename、octopus merge)→ `snip copy` → `snip paste` 到空目錄 → 比對檔案樹。commit 模式:連續 3 個 commit(含 rename、刪除、merge、二進位檔)→ 貼到另一個 clone 的不同分支 → 比對 message、作者、作者時間與檔案內容;不連續選取要被拒絕 | ✅ 三平台 |
| 3 跨工具 | 同一 repo 與設定下,Rust 產生的 payload 等於 TS 產生的;Rust 能還原 TS 與 Kotlin 產生的 payload,反之亦然 | ✅ |
| 4 真實剪貼簿 | 寫入系統剪貼簿再讀回:Unicode、大 payload、換行 | ✅ Windows / macOS runner 有桌面;Linux 用 xvfb |
| 5 前端 | 比照 aghub:`node --test` 跑 `src/**/*.test.ts`(純邏輯與 source-scan 守衛);`typecheck`、oxlint、prettier | ✅ |
| 6 操作真實 App | `crates/desktop/e2e/scenarios.mjs`:`tauri-driver`(WebDriver)操作真的 App,16 個情境(commit 模式與檔案模式)各建新的 git repo,完成後以 git 驗證。Windows 走 msedgedriver(`windows-2022`)、Linux 走 WebKitWebDriver | ✅ Windows / Linux |
| | macOS 的 WKWebView 沒有 WebDriver | ❌ 手動 |
| | 系統匣選單本身 | ❌ 各平台都難以自動化 |

- **CI 採最嚴格設定(`.github/workflows/ci.yml`):** 每個 PR 與 push 都跑全部 job,沒有路徑過濾;
  Rust 與 rustdoc 的警告視為錯誤,`--locked`;三平台 clippy 與 `cargo test`;`cargo audit` 有漏洞即失敗;
  DTO 必須與 Rust 型別同步;前端 typecheck / oxlint 零警告 / prettier / 測試 / build;
  Linux 與 Windows 跑真實 App 情境;每個 job 結束時 checkout 必須乾淨。
  `CI gate` 彙整全部 job,任何一個不是 success(含 skipped)就失敗。
- **本機:** `just preflight` 跑一遍 CI 會跑的東西,push 或打 tag 前必跑。
  它只能跑本機平台,碰到路徑 / 檔案系統的程式碼要在 Linux 上**模擬**其他平台的情況
  (例如透過 symlink 的暫存目錄模擬 macOS 的 `/var` → `/private/var`)。

**對策:** 所有邏輯放在 Rust、由第 1–3 層覆蓋;無法自動化的部分只剩「按鈕有沒有接對」,
用一份簡短的手動檢查清單涵蓋,每次發布前在 macOS 上跑一次。

## 6. 分階段計畫(每一階段通過驗收才進下一階段)

### Phase 0 — 可行性驗證
- 確認兩台實機都有 git、能執行自己編譯的 App、WebView2 可用(Windows)。
- 從 aghub 抽出骨架:workspace、justfile、CI、`crates/desktop` 的 Tauri + React + HeroUI 設定,
  拿掉 aghub 專屬的部分(api sidecar、remote、deep-link、inference 等)。
- 做最小 demo:系統匣 + 一個視窗 + 寫入 / 讀取剪貼簿,在**兩台實際的電腦**上跑起來。

**驗收:** 兩台實機都能跑 demo。任一項不成立,就在這裡停下重新評估。

### Phase 1 — Rust 核心
移植格式、路徑解析、restore 的 plan 與執行、過濾規則、統計。

**驗收:** 共用 contract fixture 全部通過,並鎖定相同 SHA;三平台 CI 綠燈。

### Phase 2 — Git commit 級複製
working tree、staged、單一 commit、commit 區間;merge 取每個 parent 的聯集;刪除的檔案帶刪除前內容;
rename 標為 `[MOVED]`;偵測 shallow clone;非 UTF-8 跳過並計數;大小與數量上限;
讀不到的 placeholder 不計為已複製、也不佔數量上限。

**驗收:** 第 2、3 層測試通過;Rust 與 TS 對同一 repo 產生的 payload 逐位元組相同
(先做差分,穩定後凍結成 fixture 的新區塊)。

### Phase 3 — commit 模式 + CLI
spec 第 4 節:連續性檢查、marker + JSON 格式、依序重播建立 commit;spec 第 5.2 節的全部 CLI 指令。

**驗收:** 第 2–4 層測試通過;在兩台實機之間實際雙向同步一段真實 repo 的 commit(檔案模式與 commit 模式各一次)。

### Phase 4 — Tauri App
系統匣、主視窗(時間軸選 commit、選檔案來源)、預覽(逐檔勾選、覆寫 diff、原因說明、commit 清單)、確認後執行。

**驗收:** 第 5、6 層測試通過;macOS 手動檢查清單通過。

### Phase 5 — 打包與發布
套用第 4 節的 release.yml;推 tag 時發布。

**驗收:** 從 Release 下載的安裝包能在兩台實機上安裝並完成一次完整同步。

## 7. 風險

| 風險 | 影響 | 在哪一階段確認 |
|---|---|---|
| 某台機器沒有 git 或不能執行自編的 App | 該機器無法使用 | Phase 0 |
| 剪貼簿被通道無聲截斷(不在範圍內,不做雜湊) | 檔案模式可能還原出殘缺檔案;commit 模式的 JSON 會解析失敗而被擋下 | 使用者自行留意通知裡的字元數 |
| commit 模式重播時覆蓋本機未 commit 的修改 | 本機修改遺失 | 設計如此(spec 4.3);預覽中列出會被覆蓋的路徑 |
| Rust 移植產生語意差異(regex、trim、Unicode、路徑) | 與 IDE 套件不相容 | Phase 1,由 fixture 抓出 |
| Rust 編譯時間與磁碟佔用 | 本機與 CI 變慢 | 沿用 aghub 的 sccache、rust-cache、Windows Defender 排除設定 |
| macOS GUI 無法自動化測試 | GUI 退化只能靠手動發現 | 以架構緩解:GUI 只當薄殼 |
