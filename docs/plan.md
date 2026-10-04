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
- 不處理簽章、公證與自動更新(只給自己用)。

## 2. 架構

> Tauri 版(`crates/desktop`)已從 repo 移除,需要回退時從 git 歷史取回。本節的技術棧、Repo 結構與移植來源描述的是 v0.2.x Tauri 版的原始架構,現行桌面 App 見第 4 節;**「與 IDE 套件的相容性:共用 contract fixture」一小節仍是現行規範**(`.ts-ref` 抽取指令與 fixture SHA)。

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

### 現行引擎(統一後)

CLI 與桌面 App 均統一呼叫核心傳輸引擎與遠端模組,實作參考 `crates/core/src/transfer.rs`、`crates/core/src/transfer/select.rs` 與 `crates/core/src/transfer/changes.rs`:

| 動作 | 共用入口 (core/remote) | CLI | App |
|---|---|---|---|
| 複製檔案／資料夾 | `transfer::plan_export_expanding` (`selection_from_paths`, `expand_folder_items`) / `transfer::plan_export_with` | `snip copy <路徑…>` | 專案樹勾選檔案／資料夾,展開走訪後匯出 |
| 複製 Git 來源 | `transfer::changed_items` + `transfer::plan_export_with` 搭配 `SourceKind::{Working,Unstaged,Staged,Commit,Range}` | `snip copy --working` / `--staged` / `--commit` / `--range` | Git 檢視勾選變更項目,依選取清單匯出 |
| 複製 commits | `transfer::plan_commit_export_with` | `snip copy --commits -n <N>` / `<a>..<b>` | 時間軸選取連續 commit 複製 |
| 貼上檔案 | `transfer::plan_import_with` + `TransferImportPlan::apply` 搭配 `ImportMapping` | `snip paste [--dry-run \| --apply]` | 貼上預覽視窗、衝突與新鮮度檢驗後確認套用 |
| 貼上 commits | `transfer::CommitReplayPreview` (`capture` / `plan` / `revalidate` / `apply`) | `snip paste` (`--overwrite` 門禁) | commit 貼上預覽視窗、覆寫開關後確認重播 |
| 路徑重定位 | `restore::suggest_restore_base`、`transfer::ImportMapping::from_restore_base` | 一組建議,由 `--adjust-paths` 全域套用 | 貼上預覽中逐 prefix 選擇(D4) |
| 配對清單 | `snip_remote::WorkerStore` / `snip_remote::TrustedMasterStore` | `snip remote` / `snip worker` | 工作區選單「遠端節點」 |

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

> Tauri 版(`crates/desktop`)已從 repo 移除,需要回退時從 git 歷史取回;下表的系統匣、plugin、前端元件列是 v0.2.x Tauri 版的紀錄。

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

## 4. 原生打包與發布

v0.3.0 起發布的桌面 App 是 GPUI 原生版(`crates/desktop-native`),細節見
[native-cross-platform-ci-and-packaging.md](native-cross-platform-ci-and-packaging.md) 第 2 節。
Tauri 版(本文件第 2、3 節的技術棧)已從 repo 移除,不再建置、測試或發布;回退請取 git 歷史。

| | Windows | macOS | Linux |
|---|---|---|---|
| 產物 | Inno Setup `snip-sync-windows-setup.exe`(per-user)+ zip | `snip-sync_mac_{arm,intel}.dmg` + `.app.tar.gz` | `snip-sync-linux-x86_64.tar.gz` |
| 在哪裡編譯 | `ci.yml` 的 `windows-latest` | `ci.yml` 的 `macos-latest` / `macos-26-intel` | `ci.yml` 的 `native-acceptance`(`ubuntu-24.04`,glibc ≥ 2.39) |
| 未簽章的後果 | SmartScreen 警告;公司政策可能直接禁止執行 | Gatekeeper 阻擋:系統設定「強制打開」或 `xattr -cr` | 無 |

- **發布流程:** `just release X.Y.Z`(`Cargo.toml` 必須已 bump 並 commit)推 tag →
  `verify-ci` 找出該 SHA 在 main 上的綠色 `ci.yml` run → `changelog`(git-cliff)、
  `verify-native`(從該 run 下載已驗收的桌面產物,核對 SHA256SUMS、版本與 Linux build receipt)、
  `build-cli`(四個 target 編 `snip`、smoke、打包)都只產生 workflow artifact →
  全部成功後 `publish-release` 以 draft 建立 Release、附上 12 個資產、確認後一次公開。
  任何檢查失敗都不會留下公開的 Release。正式版(tag 不含 `-`)再由 `publish-homebrew` 更新
  `audichuang/homebrew-tap` 的 `Formula/snip-cli.rb` 與 `Casks/snip-sync.rb`(需要 repo secret `HOMEBREW_TAP_TOKEN`)。
  `just bump` 同步 `Cargo.toml` 與 `Cargo.lock` 的版本號。
- **桌面版不重新建置:** release 發布的就是 CI 驗收過的位元組(交付規格 §10),所以 release 不改版號、不重編。
- **macOS 簽章:** ad-hoc(`codesign --sign -`),在 CI 以 `codesign --verify --deep --strict` 驗證。不做 Apple Developer 憑證與公證。
- **不做:** 自動更新。
- 本機打包:`just package-native <target> <out> <bin> <version>`。

## 5. 測試策略

| 層 | 內容 | 能否自動化 |
|---|---|---|
| 1 核心 | `snip-core` 跑共用 contract fixture(build / parse / token / path) | ✅ 三平台 |
| 2 CLI E2E | 檔案模式:建 fixture repo(一般 commit、刪除、rename、octopus merge)→ `snip copy` → `snip paste` 到空目錄 → 比對檔案樹。commit 模式:連續 3 個 commit(含 rename、刪除、merge、二進位檔)→ 貼到另一個 clone 的不同分支 → 比對 message、作者、作者時間與檔案內容;不連續選取要被拒絕 | ✅ 三平台 |
| 3 跨工具 | 同一 repo 與設定下,Rust 產生的 payload 等於 TS 產生的;Rust 能還原 TS 與 Kotlin 產生的 payload,反之亦然 | ✅ |
| 4 真實剪貼簿 | 寫入系統剪貼簿再讀回:Unicode、大 payload、換行 | ✅ Windows / macOS runner 有桌面;Linux 用 xvfb |
| 5 Python harness | `scripts/tests/`:記憶體 harness、workload 產生器、驗收 driver 的契約測試 | ✅ Linux |
| 6 操作真實 App | 原生版在 Linux X11(Xvfb + xdotool)以 `native-acceptance` 的同一份 release build 跑 `crates/native-e2e/tests/smoke.rs`、`lifecycle.rs`、IME、18 個協作情境與資源 gate;UI 邏輯另有三平台都跑的 `#[gpui::test]`,macOS／Windows 打包後會實際開窗(`smoke_native.py --launch`)。舊的 Tauri WebDriver 情境已隨 Tauri 版移除 | ✅ Linux |
| | macOS / Windows 的原生 GUI 輸入 | ❌ 手動(CI 只 smoke 打包後的 binary) |

- **測試歸屬劃分**:
  - 核心傳輸規劃與安全防護 → `crates/core/tests/transfer_planning.rs`(涵蓋匯出／匯入規劃、目的地新鮮度快照、碰撞阻擋、取消權杖,以及驗證「舊引擎輸出 == 新引擎輸出」的 parity 比對測試)。
  - CLI 行為與命令列輸出 → `crates/cli/tests/cli.rs`(涵蓋命令列參數解析、`--stdout`/`--stdin` 串接、錯誤退出代碼 exit 1/2,以及與舊引擎 `collect_payload` 的輸出位元組比對)。
  - 位元組往返與 TS 相容性 → `crates/cli/tests/e2e.rs`(第 2 層 CLI E2E 往返與第 3 層比對 `.ts-ref` 抽取之 TS 參考實作)。

- **CI 採最嚴格設定(`.github/workflows/ci.yml`):** 每個 PR 與 push 都跑全部 job,沒有路徑過濾;
  Rust 與 rustdoc 的警告視為錯誤,`--locked`;三平台 clippy 與 `cargo test`;`cargo audit` 有漏洞即失敗;
  Python harness 測試;Linux 跑原生真實 App 的 smoke / lifecycle / acceptance;
  macOS / Windows 打包並 smoke 原生 binary;每個 job 結束時 checkout 必須乾淨。
  `CI gate` 彙整全部 job,任何一個不是 success(含 skipped)就失敗。
- **E2E 驗收(Tauri 版情境的原則,已隨 Tauri 版移除,原生 driver 沿用同樣要求):** 歷史必須自動載入;以原生 Shift+click 選取並比對完整 commit 集合。貼上等待自己的預覽／錯誤,不把背景 toast 當結果。檢查精確路徑、勾選集合、來源切換、重新整理與 commit 快照;每個情境必須有斷言,未捕捉的前端錯誤會使測試失敗。CI 與 preflight 禁止以 `SNIP_E2E_ONLY` 略過情境。
- **本機:** `just preflight` 跑一遍 CI 在 Linux 上會跑的全部東西(actionlint、Rust、Python harness、原生真實 App gate),push 或打 tag 前必跑。
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
(歷史紀錄:Tauri 版已由 v0.3.0 的原生版取代,並已從 repo 移除。)
系統匣、主視窗(時間軸選 commit、選檔案來源)、預覽(逐檔勾選、覆寫 diff、原因說明、commit 清單)、確認後執行。

**驗收:** 第 5、6 層測試通過;macOS 手動檢查清單通過。

### Phase 5 — 打包與發布
套用第 4 節的 release.yml;推 tag 時發布。

**驗收:** 從 Release 下載的安裝包能在兩台實機上安裝並完成一次完整同步。

## 6.5 遠端節點模式(spec 第 8 節)

- **crate**:`crates/remote`(`snip-remote`)。它不依賴 GPUI,所以 CLI(`snip worker`)與桌面 App 共用同一份 worker 程式。模組分工:
  - `proto`:幀格式、請求／回應與協商版本常數。
  - `tls`:裝置身分、憑證驗證、配對證明。
  - `worker`:監聽與連線派送。
  - `client`:配對、連線池、重試與呼叫機制。
  - `jobs`:worker 端背景任務管理（名額 admission、heartbeat、期限、取消）。
  - `gitserve`:worker 端處理 `ScanRepos` 與 `GitView` 查詢、圍界與參數驗證、錯誤對映。
- **傳輸**:用 std 的阻塞 TCP 加 rustls(TLS 1.3,ring provider),不引入第二個 async runtime。rustls 與 ring 原本就經由 gpui 連進桌面版。每個 socket 都設逾時。master 端的連線逾時 2 s、讀寫的閒置逾時 5 s,讓卡住的 worker 在桌面版 8 s 的 drain 時限內失敗;慢但有在傳的資料不受影響。master 端 Git 呼叫上限 `GIT_CALL_LIMIT = 90 s`（大於 worker job 期限）。worker 端的讀寫逾時 30 s,閒置連線 60 s 後關閉;同時最多服務 64 條連線,超過的最多等 4 s 拿到名額(短於 master 的 5 s),排隊的連線也上限 64 條,再多就關閉。
- **協定**:每一幀是 4 位元組 big-endian 長度,接一段 JSON。幀大小上限 8 MiB,超過就拒收,不會先配置記憶體。
  - 第一幀是 `hello`,帶基礎版本 `version` (1) 與可選的上限 `max_version`（`PROTOCOL_MAX = 2`、`GIT_VIEWS_VERSION = 2`）。新 worker 協商 `min(master.max_version, worker_max)`；舊 worker（0.5.0）無此欄位被 serde 忽略，回覆無 `max_version`，協商為 1。master 在發送 `ScanRepos` 或 `GitView` 前檢查連線版本，不足時重連；重連仍不足則直接回報 `WorkerTooOld`，絕不將新請求送給舊 worker。`snip worker` 提供隱藏旗標 `--max-protocol <N>` 供測試模擬。
  - 請求包含 `list_workspaces`、`list_dir`、`stat`、`read`、`write`、`rename`、`scan_repos`、`git_view`（原保留的 `git` 已移除）。其中 `write`、`rename` 回 `unsupported`。
  - 長時間任務（`ScanRepos`、`GitView`）由 worker 背景 job 執行，每秒（`HEARTBEAT = 1 s`）發送 `Response::Pending` heartbeat 幀，master 在等待期間重設逾時，避免慢操作誤判。
  - 錯誤碼（`ErrorCode`）新增 `NotARepository`、`InvalidRevision`、`Timeout`、`Busy`、`Cancelled`、`OutsideShare`。
- **worker 的圍界與參數驗證**:
  - 圍界（boundary）：worker 在分享目錄下以 `LocalRepo::open_within` 開啟 repo，驗證 canonical toplevel、`git_dir`、`common_dir` 均在 boundary 內；透過 `--git-path` 驗證 objects、refs、index、shallow、config 等路徑皆在 boundary 內；遞迴檢查 `objects/info/alternates`（深度 ≤ 5）亦須在 boundary 內。拒絕主 repo 在分享外的 linked worktree、alternates 指向分享外的 repo，以及空 `.git` 目錄（防止向上逃逸到父 repo）。working tree 檔案讀取（Working source）經 `browser::inside` 與 realpath 檢查，阻擋指向分享外的 symlink。錯誤訊息經 `scrub` 去除分享外部路徑與詳細內部 stderr。
  - 參數驗證：rev 嚴格限制為 4..=64 位元 hex、`HEAD` 或合法的單層 ref 名稱（`valid_rev`），並一律在 worker 上先解析成完整 OID（`resolve_commit_with`），再交給 core，杜絕偽選項與參數注入（如 `:/regex`、`HEAD@{n}`、`--output=`）；path 與 dir 嚴格要求相對且為 Normal 元件；tips 僅收 hex 且上限 50,000；`limit` 上限 1,000；輸出 stdout min 到 `SERVED_MAX_STDOUT = 4 MiB`。
- **job 與 Served 名額**:
  - job 管理（`jobs.rs`）：同時執行的 GitView/ScanRepos 上限 `MAX_GIT_JOBS = 2`（其中掃描 `MAX_SCAN_JOBS = 1`），等待名額上限 `MAX_JOBS_WAITING = 16`，超過或等候逾時（10 s）立即回報 `Busy`。整體期限 View 60 s、Scan 75 s，逾時取消 job 並回報 `Timeout`。master 斷線或寫入 Pending 失敗即取消 job；取消分享（`set_roots`）與 worker 停止（`stop`）時取消對應 workspace 或全部 job。連線槽位以 RAII guard 確保即便 panic 也能安全釋放。
  - Served 名額池（`gitrun.rs`）：遠端讀取的 git 程序皆使用獨立的 `GitPool::Served`（`MAX_CONCURRENT_SERVED_GIT = 1`、`MAX_QUEUED_SERVED_GIT = 4`），不計入本機的 `in_flight()`/`queued()`/`leaked_slots()`，因此遠端 master 的讀取絕不排擠 worker 主人自己的 UI，也不會阻擋桌面版的 drain（切換工作區與結束程式）。master 端對遠端 Git 呼叫亦加上並行上限 `MAX_GIT_CALLS_IN_FLIGHT = 4`，避免合併 log 請求塞滿 worker 佇列。
- **身分與配對**:
  - 憑證由 rcgen 產生,ECDSA P-256,自簽,CN 固定為 `snip-sync`。對方的身分只看憑證 DER 的 SHA-256 指紋。兩邊都出示憑證(mTLS)。
  - worker 的 TLS 層接受任何客戶端憑證,未配對的 master 只能送 `pair`。
  - 配對證明的算法是 `HMAC-SHA256(key = 配對碼, "snip-sync pair v1\0" ‖ worker 指紋 ‖ master 指紋)`。中間人看到的是另一組憑證,算出的證明對不上。
  - 配對碼由 32 個不易混淆的字元組成,8 碼,約 40 bits。
- **存放位置**:都在設定資料夾(`SNIP_CONFIG_DIR`,或各平台的預設位置;CLI 與桌面版共用)。
  - 裝置身分:`remote-identity.der` 與 `remote-identity.key`(Unix 權限 0600)。
  - worker 端:`remote-trusted-masters.json`。
  - master 端:`remote-workers.json`。
  - `remote-workers.json` 與 `remote-trusted-masters.json` 一律僅透過 `snip_remote::WorkerStore` 與 `TrustedMasterStore`(`crates/remote/src/store.rs`)存取:每次變更皆先取得旁車鎖檔 `<name>.lock` 的獨占建議鎖(exclusive advisory lock),在鎖內重新載入清單、套用變更,並透過獨立命名的暫存檔(`save_json`:寫入 `<name>.<pid>.<count>.tmp` 後 rename 覆蓋)原子寫入,因此 CLI 與桌面版絕不互相覆蓋彼此剛加入的配對;`add` 會移除相同指紋或位址的既有項目並將新項目置於首位;`forget` 若查無相符項目則不重新寫入檔案。
- **桌面版接法**:`WorkbenchModel.remote.session` 有值時,工作區就是遠端的。
  - 引入 `GitHost`（`githost.rs`）：區分 `Local` 與 `Remote`。UI 的 Changes、Log、Diff、歷史樹等統一向 `GitHost` 索取 `Box<dyn RepoView>`，本機走 `LocalRepo`，遠端走 `RemoteRepo`（將方法轉為型別化 `GitQuery` 經 `Client` 發送）。
  - `submit_tree_io` 改走 `remote::tree_io`,它呼叫 `tree::listed_tree_result`,一次列完,沒有游標。
  - `select_file_in` 對 `SourceKind::File` 改走 `remote::read_preview`。
  - 樹的根是虛擬路徑 `snip-remote://<指紋>/<id>`,不碰本機磁碟。開啟遠端工作區後在背景發送 `ScanRepos` 掃描 repo（深度 8、上限 256、期限 75 s），若未完成顯示 `remote_scan_incomplete`。Refresh（重新整理）觸發重掃。繼續探索僅支援 depth-limited 資料夾。
  - 前綴路由：遠端 repo 的樹 IO 與預覽路徑自動加上相對於 session root 的 repo 前綴。單一 repo 分享時復用單一樹，避免多餘的虛擬工作區樹。
  - 空狀態：Changes 與 Log 引入純函式判定，嚴格遵守「沒讀到不顯示成乾淨」（Changes 包含 no_workspace、scanning、loading、no_repository、scan_failed、no_match、clean；Log 包含 no_workspace、scanning、loading、no_repository、failed、empty），並在 UI 埋入 `changes-empty`、`log-empty` probe 與 `[APP:CHANGES_EMPTY]` / `[APP:LOG_EMPTY]` 日誌。
  - 遠端守門：遠端工作區下複製、貼上、加入 repo 路徑、為複製勾選皆嚴格阻擋（回報 `remote_unsupported`）；右鍵選單的 repo 與檔案列僅提供複製 worker 路徑，不提供本機 reveal。
  - 開啟遠端工作區是 `lifecycle::Intent::OpenRemoteWorkspace`,與開本機工作區走同一套關閉檢查。
  - worker 監聽器是程序層級的全域物件,先於視窗啟動,也不隨工作區切換而停止。這是之後做無螢幕常駐(Windows 登入項目或服務)的路徑。
- **測試**:
  - `crates/remote/src/store.rs`:單元測試(涵蓋 add / forget / find 基本操作、過期實例與兩個 process 交錯 add 不遺失項目、不可寫位置回報錯誤、暫存檔命名唯一且不留殘檔)。
  - `crates/remote/src/jobs.rs`:單元測試（admit 名額、等待上限 → Busy、取消、心跳幀與逾時）。
  - `crates/remote/tests/git_views.rs`:loopback 真 TLS 測試，涵蓋三種工作區掃描、`RemoteRepo` 與 `LocalRepo` 同函式對照、圍界安全測試、版本協商與斷線取消。
  - `crates/remote/tests/git_queue.rs`:獨立測試，驗證大量並行請求下 Served 池與 job 佇列的有界等待、`Busy` 回報及不排擠 Local git。
  - `crates/cli/tests/worker.rs`:真的啟動 `snip worker` 程序，測試 CLI 唯讀 Git 子命令（repos/changes/log/show/diff）與 `--max-protocol`。
  - `crates/desktop-native`:gpui 整合測試，涵蓋遠端多 repo / 單 repo / 非 repo 工作區、空狀態判定、守門拒絕與 Refresh。
  - `scripts/remote_e2e.sh`:連線能力的端到端驗證,獨立於容器裡的 native acceptance。它只用 CLI:起真的 `snip worker` 程序,由真的 `snip remote` 經 TLS 配對、瀏覽、逐位元組比對、確認該拒絕的情況、平行讀取、重啟、換憑證,並包含配對後 forget 其他項目不影響現有配對的檢查；包含 `== git views` 段，驗證跨機器 TLS 上的唯讀 Git 檢視、圍界防護、參數注入防護以及 `.git/index` 不變性。
    - `just remote-e2e`:worker 在本機 127.0.0.1。preflight 會跑;CI 的 `Remote E2E` job 在 Ubuntu、macOS、Windows 各跑一次,列入 CI gate。
    - `just remote-e2e-ssh <host>`:worker 在另一台機器,經 ssh 從 `git archive HEAD` 編出並啟動,走 Tailscale 連線。改到遠端節點的程式時必跑。
  - `crates/remote/tests/loopback.rs`:真實 TLS 走 127.0.0.1,涵蓋配對、拒絕、pin、containment、symlink root。
  - `main.rs` `tests::in_process::remote_workspace_pairs_lists_and_previews_through_a_worker`:配對表單、遠端樹、預覽。

## 7. 風險

| 風險 | 影響 | 在哪一階段確認 |
|---|---|---|
| 某台機器沒有 git 或不能執行自編的 App | 該機器無法使用 | Phase 0 |
| 剪貼簿被通道無聲截斷(不在範圍內,不做雜湊) | 檔案模式可能還原出殘缺檔案;commit 模式的 JSON 會解析失敗而被擋下 | 使用者自行留意通知裡的字元數 |
| commit 模式重播時覆蓋本機未 commit 的修改 | 本機修改遺失 | 設計如此(spec 4.3);預覽中列出會被覆蓋的路徑 |
| Rust 移植產生語意差異(regex、trim、Unicode、路徑) | 與 IDE 套件不相容 | Phase 1,由 fixture 抓出 |
| Rust 編譯時間與磁碟佔用 | 本機與 CI 變慢 | CI 只用 rust-cache,且只有 push 到 main 時寫入(0.3.2 起拿掉 sccache:兩者加各分支快取超過 repo 10 GB 配額,互相淘汰到命中率約 0%) |
| macOS GUI 無法自動化測試 | GUI 退化只能靠手動發現 | 以架構緩解:GUI 只當薄殼 |
