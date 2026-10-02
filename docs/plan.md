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
  - `proto`:幀格式與請求／回應。
  - `tls`:裝置身分、憑證驗證、配對證明。
  - `worker`:監聽與處理請求。
  - `client`:配對,以及 pin 住 worker 憑證後的呼叫。
- **傳輸**:用 std 的阻塞 TCP 加 rustls(TLS 1.3,ring provider),不引入第二個 async runtime。rustls 與 ring 原本就經由 gpui 連進桌面版。每個 socket 都設逾時。master 端的連線逾時 2 s、讀寫的閒置逾時 5 s,讓卡住的 worker 在桌面版 8 s 的 drain 時限內失敗;慢但有在傳的資料不受影響。worker 端的讀寫逾時 30 s,閒置連線 60 s 後關閉;同時最多服務 64 條連線,超過的最多等 4 s 拿到名額(短於 master 的 5 s),排隊的連線也上限 64 條,再多就關閉。
- **協定**:每一幀是 4 位元組 big-endian 長度,接一段 JSON。幀大小上限 8 MiB,超過就拒收,不會先配置記憶體。
  - 第一幀是 `hello`,帶協定版本(`PROTOCOL_VERSION`)。版本不同時回 `version_mismatch`。
  - 請求共有 `list_workspaces`、`list_dir`、`stat`、`read`、`write`、`rename`、`git` 幾種。其中 `write`、`rename`、`git` 目前回 `unsupported`。
- **身分與配對**:
  - 憑證由 rcgen 產生,ECDSA P-256,自簽,CN 固定為 `snip-sync`。對方的身分只看憑證 DER 的 SHA-256 指紋。兩邊都出示憑證(mTLS)。
  - worker 的 TLS 層接受任何客戶端憑證,未配對的 master 只能送 `pair`。
  - 配對證明的算法是 `HMAC-SHA256(key = 配對碼, "snip-sync pair v1\0" ‖ worker 指紋 ‖ master 指紋)`。中間人看到的是另一組憑證,算出的證明對不上。
  - 配對碼由 32 個不易混淆的字元組成,8 碼,約 40 bits。
- **存放位置**:都在設定資料夾(`SNIP_CONFIG_DIR`,或各平台的預設位置;CLI 與桌面版共用)。
  - 裝置身分:`remote-identity.der` 與 `remote-identity.key`(Unix 權限 0600)。
  - worker 端:`remote-trusted-masters.json`。
  - master 端:`remote-workers.json`。
- **桌面版接法**:`WorkbenchModel.remote.session` 有值時,工作區就是遠端的。
  - `submit_tree_io` 改走 `remote::tree_io`,它呼叫 `tree::listed_tree_result`,一次列完,沒有游標。
  - `select_file_in` 對 `SourceKind::File` 改走 `remote::read_preview`。
  - 樹的根是虛擬路徑 `snip-remote://<指紋>/<id>`,不碰本機磁碟,也不跑 repo 探索。
  - 開啟遠端工作區是 `lifecycle::Intent::OpenRemoteWorkspace`,與開本機工作區走同一套關閉檢查。
  - worker 監聽器是程序層級的全域物件,先於視窗啟動,也不隨工作區切換而停止。這是之後做無螢幕常駐(Windows 登入項目或服務)的路徑。
- **測試**:
  - `scripts/remote_e2e.sh`:連線能力的端到端驗證,獨立於容器裡的 native acceptance。它只用 CLI:起真的 `snip worker` 程序,由真的 `snip remote` 經 TLS 配對、瀏覽、逐位元組比對、確認該拒絕的情況、平行讀取、重啟、換憑證。
    - `just remote-e2e`:worker 在本機 127.0.0.1。preflight 會跑;CI 的 `Remote E2E` job 在 Ubuntu、macOS、Windows 各跑一次,列入 CI gate。
    - `just remote-e2e-ssh <host>`:worker 在另一台機器,經 ssh 從 `git archive HEAD` 編出並啟動,走 Tailscale 連線。改到遠端節點的程式時必跑。
  - `crates/remote/tests/loopback.rs`:真實 TLS 走 127.0.0.1,涵蓋配對、拒絕、pin、containment、symlink root。
  - `crates/cli/tests/worker.rs`:真的啟動 `snip worker` 程序。
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
