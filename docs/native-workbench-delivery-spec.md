# 原生 Git／檔案工作台：後續交付規格

日期：2026-09-25。狀態：**已授權實作，尚未完成驗收**。

## 0. 執行責任與規格優先序

使用者最新指示（2026-09-26）：AGY 完成開發與測試，並授權搭配 Grok 4.7 交叉使用；Codex 撰寫規格、指揮、審查與獨立驗收。Claude 的三個工作已停止，既有修改保留後交接；產品範圍不變。Codex 不代寫產品實作，AGY／Grok 依派工責任開發或審查，同一批檔案僅有一個實作 owner。

後續分工更新（2026-09-26）：使用者先自行建立開發checkpoint commit，同事在另一個worktree負責UI視覺優化。本工作繼續Git／檔案同步正確性、bounded memory、背景工作與資源回收、E2E、CI及發版驗收；不另派視覺重做。保留使用者合法前進的HEAD與同事改動，最終整合版本重新綁定source／binary身份並跑必要驗收。

最新分工（2026-09-27）：AGY 額度耗盡後，使用者明確授權改派 Codex 子 agent 完成實作；Codex 主 agent 繼續指揮、決策、審查與獨立驗證。這項授權取代上方 AGY／Grok 專責實作的限制，產品範圍與每批單一檔案 owner 不變。先完成 Linux，再跑實際 macOS CI runner，由使用者安裝候選包做 Mac runtime 驗收。

本文件把既有規劃轉成可執行的交付順序與驗收清單。完整功能／數值依 `native-git-workbench-plan.md`；視覺依 `native-workbench-ui-acceptance.md`；已驗證成果與未解缺陷依 `native-workbench-supervision.md`。既有正式行為仍依 `spec.md`、`plan.md`、`porting-notes.md`。剪貼簿相容性以 pinned TS reference 和 immutable contract fixture 為準。若有矛盾，先記錄實際差異，由監督者裁定，不可自行縮減需求或宣告完成。

交付不是只有 mockup、只有文件、只有原型，或只改顏色。要讓使用者以清楚、低記憶體的介面判斷來源並完成兩台電腦間的剪貼簿同步。

## 1. 產品結果

使用者要同時開約 15 個 repo，在 agent 編寫程式時：

1. 看懂各 repo／worktree 的 branch、staged、unstaged、untracked、conflicts 及更新時間。
2. 從專案樹閱讀 working file 或任意 commit 的檔案，不切換 checkout。
3. 用清晰的 graph 看全部已取得的 local／remote-tracking refs、tags、fork／merge／parents，定位／比較 commit。
4. 選明確的內容版本，預覽實際會複製的檔案／刪除項目／commit 範圍。
5. 在另一個 workspace 明確 mapping 到目標，確認預覽後套用，看到完整成功／失敗／跳過結果。

不加入編譯、執行、debugger、LSP、終端機、Python 環境、插件市場或 agent 管理。一般 Git checkout／stage／push／merge 不屬於這次檢視工具；Git 寫入保持既有還原／commit replay。

## 2. IntelliJ 風格重做（必須整體改版）

視覺參考：IntelliJ 官方 [New UI](https://www.jetbrains.com/help/idea/new-ui.html) 及 Compact mode。保留 snip-sync 身份。使用一致的中性深灰、細分隔線、克制藍色選取、小圖示；不要滿版彩色膠囊按鈕、emoji 檔案圖示、大面積留白或重複操作工具列。

```text
┌ 工作區 / repo ▾   branch ▾                       複製  貼上  設定 ┐
│窄│ Project / Changes  │ file tab                                  │
│工│ ▾ repo-a           │ path · working / staged / commit <OID>      │
│具│   ▾ src            │                                           │
│列│     file.rs        │       檔案內容 / diff                      │
│  │ ▸ repo-b           │                                           │
│  ├───────────────────┴───────────────────────────────────────────┤
│  │ Git  [Log] [Changes]     repo ▾  refs ▾  搜尋                   │
│  │ refs / branches │ graph / message / author / date              │
│  │                 │ ●─── merge                                  │
│  │                 │ │ ● feature                                 │
├──┴─────────────────┴─────────────────────────────────────────────┤
│ 來源與選取數 / 最近更新 / 操作狀態                                │
└──────────────────────────────────────────────────────────────────┘
```

- 窄側邊工具視窗列；Project／Changes 切換不清除待複製選取。
- 左側樹寬、下方 Git Log 高可調整；工具視窗可收合，恢復時保留位置。
- 中央檔案 tab 與 breadcrumb 清楚顯示來源（working/index/commit），預設僅保留一份重預覽資料。
- Git Log 橫向跨主要工作區，有足夠欄寬讀 message、refs、作者、日期；不能塞在左側樹下的小格。
- 相同色彩／尺寸／字型從小型共用 theme 定義取得，避免把同一色散落數千行。僅按實際面板拆模組，不引入通用 UI 框架。
- UI 12–13px、程式碼等寬字、24–28px 資料列、36–40px 主列、約22px 狀態列為初始設計目標；依截圖可讀性調整。
- 在 1080×720 與 900×600，繁中、長路徑、長 branch 名都不擠掉按鈕；溢出內容有截短／水平捲動／tooltip。
- 不畫沒有實作的搜尋、選單或功能按鈕。disabled 狀態有原因；錯誤、loading、empty、clean 各自清楚。
- 支援繁中／英文、可見鍵盤焦點、主流程快捷鍵、合理 tab 順序、可讀 label。完整中文 IME／縮放／accessibility 驗證不可用單張截圖代替。

### 貼上面板

使用單一貼上預覽 tool window／對話區域，顯示來源群組、目標 mapping、逐檔 create／overwrite／delete／skip 與原因、內容／diff。只有一組 Apply／Cancel；overwrite 預設關閉，可逐項選取。切換選中列只切內容預覽，不隱式打開覆寫。只有明確 Apply 才能寫入；Cancel／Escape 不寫入。

## 3. 有界共享核心與資料正確性

### 身份、探索與工作區

- 一個 repo 的 packages 共用一張 graph；15 獨立 repo 各自擁有 graph，不偽造跨 repo parent edges。
- nested repo／submodule／linked worktree／detached／unborn 都有明確身份及狀態；未初始化 submodule 不當錯誤 repo 靜默消失。
- canonical root／git directory／common directory 分開，處理 symlink、Unicode、空白、同 basename、Windows 大小寫差異，不一律 lower-case。
- 目錄與 repo 探索限深度、總工作量、結果 bytes、cursor 狀態；不可先把全部目錄 collect／sort 再檢查上限。超限可見並可繼續／手動加入。
- NUL 路徑解析，不將非 UTF-8 路徑有損轉換後用來寫入。

### Git／檔案 I/O

- 所有新工作流透過共享 snip-core；不保留第二套 native Git reader／paste engine。
- 全域最多2個 Git 子程序，per-worktree 重工作序列化，queue 有界；取消真正停止讀取、子程序與 reader，清理完成才釋放 permit。
- timeout／cancel／overflow／Git failure 是不同結果；strict API 不允許丟棄 truncation flag 回傳成功。
- Windows process job 與 Unix process group／pipe cleanup 必須有平台對應設計及測試；新增依賴不代表已解決 cleanup。
- blob 先固定 OID 並查 size，再依可用预算讀；working file 開檔前驗證型別／路徑，讀取中仍有 byte cap。
- Permission denied、損壞 repo、Git stderr 不可吞成 clean／unborn／not found；只對精確 missing case 做退化。
- mutable source 用 generation 與前後狀態檢查；快速切 repo 的舊結果不可更新新畫面。

### Git 來源

- Staged = HEAD→index，內容從 index；Unstaged = index→working tree；Untracked／Conflicts 獨立。
- 既有 Working 複製集合不可冒名為 Unstaged。相同檔 staged A／working B 預覽及複製必須與選取來源一致。
- root commit 對空樹；merge 讀取顯示 parent，複製及 replay 按既有 pinned contract。
- refs 包含 local／remote-tracking／tags／HEAD，upstream 缺失顯示未知；沒有自動 fetch。

## 4. 閱讀、graph 與選取

- Lazy 可展開樹、Git 裝飾、目錄勾選與檔案勾選、已選摘要；避免父子 repo 重複選取。
- working 與任意 commit tree／file 內容，可看刪除前內容；歷史不依賴目前 HEAD 的檔案仍存在。
- 高亮、行號、檔內搜尋、跳行、文字選取複製；大檔／binary／超長行以有界預覽和可理解原因退化。
- inline 及 side-by-side diff、rename／delete、parent／revision compare 語意明確。
- graph 使用真 parent edges、穩定 topo、merge junction／octopus／shallow／未載入邊界。
- graph 有界分頁、回讀、refs 聚焦、SHA／message／author 搜尋、定位 HEAD、鍵盤導航、範圍選取、merge-path collapse／expand。篩選或摺疊不偽造直接 parent。
- 一個快照換頁 lane／color 穩定；refs 更新可重建快照但保留 OID 錨點。過寬 frontier 顯示有理由的退化視圖。
- 分支多選集合與可 replay first-parent 連續範圍分開；非法集合在預覽就拒絕。

## 5. 複製、mapping、freshness 與 replay

- 待複製籃保存 root/source/revision/path 識別，不預存所有檔案內容。預覽顯示明確來源及跳過原因。
- 共用 transfer 模組；總 payload admission 包含 header／escaping／separator 的實際序列化 bytes。超限整筆拒絕，不能截斷成功或先配置完才檢查。
- filtering、檔數／單檔限制、binary／unreadable／deleted semantics 保持 pinned TS byte parity；fixtures 不改。
- source HEAD／ref／index 與實際 working bytes 要 fresh；刪除路徑的「不存在」也是快照，預覽後重建檔案必須使計畫失效。
- 多 repo 目的 mapping 明確，無法唯一推斷時不自動寫入；路徑碰撞／symlink 越界預先拒絕。
- apply 前驗證 target HEAD／ref／index／內容與 absence。過期先停止，不在舊預覽後偷偷重算並寫入。
- file restore 及 commit replay 都經真 clipboard，保留現有 delete／overwrite／skip 與 metadata 規則。
- replay 接受同 repo 連續 first-parent 範圍，逐 repo 明確目標；保留 message、author、author time，檢查真 tree，不宣稱 hash 相同。
- partial failure 顯示已完成項目與失敗；不承諾 filesystem 跨檔原子 rollback。

## 6. 記憶體與生命週期

完整數值依原規劃第9節。初始 retained-data 合計64MiB：preview32、graph16、tree8、highlight8。此數不含 renderer、GPU、font、allocator、clipboard 和 Git child。

產品初始目標：idle≤100MiB；1repo≤160；15repo overview≤256且比1repo增量≤96；15repo active≤384；預設copy/paste峰值≤512。這些是待驗證目標，不是保證／量測結果。

- 僅當前詳細視圖保留重資料；100次切 repo／預覽後要回到 cache budget。
- 共用監聽、debounce、bounded refresh；hidden/tray停止非必要輪詢，workspace close回收watcher／task／child／cache。
- 同一標準15repo資料、相同release profile／尺寸／硬體比較Tauri和native。保留原始sample、binary revision/hash、OS、指標、child歸屬與readiness證據。
- Linux PSS/RSS、macOS footprint、Windows private working set/bytes不可混除。startup peak 與 post-ready steady 分別報告。
- 相同profile10次冷／暖量測報median/p95/worst；100switch soak；UI延遲／idle CPU依原規劃。native活動記憶體相較可比基線下降30%是初始切換門檻。
- 既有 `/tmp/snip-tauri-baseline-20260925` 報告已被拒絕，不得引用其數據為通過證據。null preview、選錯 PID、猜測 metadata 都讓該輪失效。

## 7. 接手現況與整合順序

主checkout `/home/audichuang/research/snip-sync`，branch `feature/lightweight-git-workbench-plan`，HEAD `7ed0279`，base `9be8684f0b7bd556f59159c6887fd5af339de148`。先檢查actual root/branch/base，再編輯。保留所有既有dirty檔及使用者文件，不能reset／clean。

初次接手時的來源快照（歷史狀態；最新接受範圍以 supervision 為準）：

| 來源 | 可沿用 | 尚未接受 |
| --- | --- | --- |
| `/home/audichuang/.codex/worktrees/snip-native-graph/snip-sync` | graph `1e115bd`（主checkout已cherry-pick）；transfer `4c886a8` | bounded runner整合、absence freshness、完整UI接線 |
| `/home/audichuang/.codex/worktrees/snip-memory-harness/snip-sync` | workload/memory `ea7ab85`、CI `d42c739` | dirty Tauri baseline scripts與測量有效性 |
| `/home/audichuang/.codex/worktrees/snip-bounded-core/snip-sync` | dirty P1草稿僅作參考 | process cleanup、bounded discovery、errors等阻擋項 |
| 主checkout | native UI原型與測試 | 最新E2E、fmt失敗；UI視覺被使用者拒絕 |

不盲目整批覆蓋 core/lib.rs、Cargo.lock 或 justfile。保留 graph／transfer export，合併必要差异，解釋 lockfile 大量變動的來源。外部來源 worktree 預設唯讀；本輪 AGY 分別在 UI、core、harness 三個既有 worktree 寫入，禁止跨所有權改檔。

### 交付順序與中間檢查點

1. **D0 接手／重現**：讀規格與supervision，盤點當前差異、重現最新native E2E/fmt失敗；記錄native dependency與host條件。
2. **D1 UI重做**：先交可實際操作、可截圖的IntelliJ版面與貼上overflow修正。原生input regression全綠才交第一輪review；不得停在設計建議。
3. **D2 bounded core整合**：修P1阻擋、transfer整合、source semantics、race/cancel/path tests。這是UI多repo功能的安全基礎。
4. **D3完整功能**：15repo、歷史檔案、graph導航／搜尋／比較／collapse、selection/mapping/replay接入共用core，補缺失UI控制與國際化。
5. **D4量測與桌面品質**：修baseline、真native量測與soak、watcher/tray/IME/accessibility/scale、三平台package/installer驗證。
6. **D5總驗收／交付**：全部preflight、CI matrix、artifact檢查、更新spec/plan/porting notes/README與版本流程。由監督者審查後才提交／push／PR，CI綠再merge/release。

中間回報是review checkpoint，不等於縮小D2–D5範圍。若框架或平台功能不可完成，附具體重現／可行替代交給監督者；不可把未完成標為done或隱藏required gate。

## 8. 必須有的可執行驗收

| ID | 操作／資料 | 核心斷言 |
| --- | --- | --- |
| UI01 | 1080×720、900×600、繁中、長路徑 | 控制項可見可點、無裁切、可調split；真截圖逐張檢查 |
| UI02 | repo→expand tree→read→checkbox→copy | 真clipboard bytes與指定來源完全相符，導航不偷改選取 |
| UI03 | own export→另一目錄preview→Escape→preview→Apply | 取消不寫；Apply後內容正確；未選檔不變 |
| UI04 | create/overwrite/delete三項，逐項開覆寫 | 真滑鼠命中控制，狀態及目的檔與選取一致 |
| UI05 | preview後外部改target或HEAD/index | Apply失效；create/delete不執行；原因可見 |
| W01–04 | 15repo、nested/submodule/worktree、symlink、快速切換 | 狀態比對Git；身份不重複；error不冒充clean；旧結果不覆蓋 |
| G01–04 | root/merge/octopus/shallow、多頁、refs、staged A working B | graph edges有oracle，source bytes正確，lane有界穩定 |
| F01 | 歷史tree／blob、大檔、binary、長行 | 不checkout，配置前限制，明確退化 |
| C01–03 | 跨repo mapping、first-parent replay、來源／目的更動 | contract、tree/author/message一致；錯mapping／過期不寫 |
| M01–02 | 100switch、hidden、workspace close、巨量輸出、descendant pipe | budgets內，child/readers/watchers回收，strict超限不成功 |
| U01 | keyboard、IME、focus、scale | 真視窗完整主流程可操作，無無法離開的焦點陷阱 |

W/G/F/C/M/U 細則依原規劃第10節。保留舊Tauri全部22 real-app scenarios及contract gates，不以unit替代E2E。Native tests必要時用真OS input與screenshot，不能只直接call handler/core。穩定control identity或真render bounds取代硬編碼假座標。測試必須能因斷開copy handler而失敗。

缺display／driver／reference時required測試fail；不得skip成綠。每次push前`just preflight`，包含新增native gate；CI額外audit、DTO、clean checkout、Windows/macOS不可移除。

### 每個review checkpoint必交

- 修改檔案與目的、仍未完成的要求。
- 執行命令、exit code、測試個數、log/artifact絕對路徑；不可只寫「全部通過」。
- `graph.png`、`file_tree.png`、`paste_preview.png` 及小視窗證據，產生時間／revision明確，禁止沿用舊圖充數。
- 測量的raw samples、profile／dataset／PID tree／readiness；沒有的欄位明寫未測。
- 已知風險與平台限制，說明有無改wire格式／行為及相應文件。

## 9. Host條件與發布責任

目前Linux缺標準libxkbcommon-x11開發linkage。本機必要時可在單次命令使用 `LIBRARY_PATH=/home/audichuang/.local/lib`，不可commit HOME-dependent linker hack；CI安裝標準dev package。Native computer-use surface原先不可用，可使用現有X11真input harness；不得把它宣稱為macOS/Windows CUA驗證。

使用者已授權CI綠後合併及發布，不需重問例行permission。AGY本輪只實作／測試並回報；Codex負責獨立review及Git交付。正式切換必須通過P5。main只放released code，feature→develop，再develop→main release PR；在main上依`just release X.Y.Z`。不得繞過branch protection／CI、不得把原型當新正式版。


## 10. 2026-09-26 最終驗收補充：多人協作與洩漏門檻

使用者明確要求交付前建立約 15 個 repo 的複雜多人協作工作區，並嚴格檢查記憶體洩漏。這是 D5 必要條件，不能由單元測試或短時間 idle 取樣替代。

### 兩台電腦、各 15 個 repo

建立獨立的 A/B 工作區與確定性的 truth manifest，固定作者與時間，包含 diverged branches、merge、未合併 tips、remote-tracking refs、tags、rename/delete、staged A / working B，並涵蓋 conflicts 與不同目標名稱的 mapping。沿用既有 Git fixture 工具；標準效能資料集保持獨立，避免用小型功能 fixture 冒充 standard benchmark。

透過真 OS input 與真 clipboard 執行 A→B 及 B→A 的檔案、commit 複製／貼上。每步獨立比對精確 bytes、Git tree、作者／訊息／作者時間、HEAD／index／未選檔案；graph parent edges 與 refs 直接對照 Git。非法或跨 repo commit 範圍、過期 preview、取消、錯誤 mapping、覆寫未授權必須不寫入。保留 screenshots、操作紀錄、來源與目的快照、binary hash 和 fixture hash。

來源過期在來源端 preview／selection→Copy 階段驗證；跨機傳遞後 clipboard 已固定，目的端不能得知另一台電腦後續的來源變更。目的過期在目的端 preview→Apply 階段驗證。負向案例故意修改狀態後，以「按 Apply 前」的全工作區快照比對拒絕後狀態，另外保存故意修改的差異，避免把測試準備動作誤判為 app 寫入。

### 記憶體及資源洩漏

- 檢查暖機後的記憶體趨勢和回收後基線，不能只看 process exit 釋放或最後一個 RSS 樣本。RAM 與 GPU 指標分別報告，無法量測的欄位不得填零。
- 真正反覆操作 15 repo 的 repo/tree/history/diff views、copy/paste preview、取消、workspace close/reopen；hidden/tray 與 quit cleanup 需其實作及平台能力的實測證據。
- 同時記錄 app process tree RSS/PSS、threads、file descriptors、Git children／owned tasks／watchers 可觀測數量；子程序身分使用 PID + starttime，結束後不得有存活的 owned child。
- Linux 的 inotify watch 數須另外觀測：一個 fd 可持有許多 watch，fd 數穩定不能证明 watcher 已回收。final settled window 必須位於完整 soak 與所有操作之後；force-kill 只能作失敗後清理，不能當 graceful cleanup 通過。
- 本地 preflight 與 PR CI 執行有界 soak；release gate 執行較長 soak 與標準 15 repo release workload。保存 raw samples 和 exact revision/binary/dataset 身分。missing、skip、unsupported、逾時或錯誤身分不能變成綠燈。
- gate 必須有負向測試：刻意持續配置記憶體、漏 fd／thread／child、未做 UI 操作、過短樣本、錯 SHA／binary／fixture、失敗或缺少報告，都要非零結束。合成故障只能證明 gate 有效，不能充當 app 通過。
- 推送前跑包含新增 gate 的 `just preflight`；發版僅接受 exact main SHA 的必要檢查與同版產物證據。測試能提供已覆蓋範圍的證據，不宣稱數學上證明永無洩漏。
- 發布 native 產物須沿用 CI 實際驗收的執行檔並核對 hash、run 與 artifact 身分；相同 source SHA 的重新建置不等於相同已驗收執行檔。若簽章或其他發布步驟改變 binary，必要驗收必須綁定最終 binary hash。
