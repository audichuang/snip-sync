# Native workbench — Claude checkpoint D0+D1

> 註:Tauri 版(`crates/desktop`)與其 driver(`bench_tauri_memory.py`、`measure_tauri.sh`、Tauri E2E)已從 repo 移除;本文提到它們建置、測試或量測的段落是當時的紀錄,回退請取 git 歷史。

日期：2026-09-25（Asia/Taipei）。實作者：Claude。狀態：**D1 修正已提交審查，監督者 findings 待複審（changes submitted, supervisor findings pending）。D1 尚未被接受；D2–D5 尚未開始，整體專案未完成。** 未 commit／push／PR。

## 第二輪：監督者 findings 1–6 的修正（最新，優先閱讀）

Guard 重驗：root `/home/audichuang/research/snip-sync`、branch `feature/lightweight-git-workbench-plan`、HEAD `7ed0279`、`9be8684` 為祖先。

| # | Finding | 修正 | 迴歸證據 |
| --- | --- | --- | --- |
| 1 | probe 在一般執行也啟用、`bounds_log` 永久累積 | 改為 opt-in：只有 `SNIP_NATIVE_E2E=1` 才建立 `Probes`；關閉時 `probe()` 直接回 `None`，不建 canvas、不配置 id 字串、不輸出 `CTRL_BOUNDS`／`VIEWPORT`。開啟時 `ProbeFrame` 只保存「上一幀＋本幀」的控制項；root 最後一個子元素在 prepaint 結束該幀並輸出 `[APP:CTRL_GONE: id=…]`，driver 收到即移除，不會點到過期位置 | 單元：`probe_bookkeeping_is_bounded_by_one_frame`（1000 幀、每幀 3 個不同 id，tracked ≤3）、`probe_reports_changes_and_disappearance`、`e2e_is_off_without_the_env_flag`。真視窗：smoke 開頭先以**未設旗標**啟動 app，Alt+2/1×2、Tab×2 導覽後斷言 stdout 無 `CTRL_BOUNDS/CTRL_GONE/VIEWPORT`，再 Ctrl+Q 乾淨結束 |
| 2 | 新貼上讀取/解析失敗時舊 plan 仍可套用 | `trigger_paste_preview` 先 `paste_preview.take()`（log `PASTE_PLAN_CLEARED`）再讀剪貼簿；錯誤改走 stdout `PASTE_ERR`；`apply_paste_restore` 無 plan 時 log `APPLY_IGNORED: no_plan` | smoke 11b：stale plan 開著時寫入無效文字 → Ctrl+V → `PASTE_PLAN_CLEARED`＋`PASTE_ERR` → `btn-apply` 回報 GONE → Enter → `APPLY_IGNORED` → 新檔未建立、to_delete 仍在、existing 未變 |
| 3 | 套用中仍可取消／改列／新貼上，舊結果可能被 generation 丟棄 | 新增 `paste_busy()`；套用中 `cancel`／`include`／`overwrite`／`preview`／再次 `apply` 一律拒絕並 log `PASTE_BUSY: refused=…`、狀態列顯示原因；UI 取消鈕停用並有 tooltip，列控制變淡加 tooltip；完成回呼**移除 generation 檢查**，結果一定顯示並清除 plan（plan 在套用中不可能被換掉）。未宣稱可中途取消或 rollback | smoke 12：`SNIP_NATIVE_E2E_APPLY_DELAY_MS=2500`（僅 E2E 模式有效）下點套用 → Escape、點 include、Ctrl+V 各得 `PASTE_BUSY`，點停用的取消無作用，Alt+2 切 repo；斷言仍在延遲內完成這些操作、結果為 `created=1 overwritten=0 skipped=1 deleted=1 errors=0`、期間無 `PASTE_CANCELLED/PASTE_SEL_TOGGLED/PASTE_PREVIEW`，磁碟內容正確 |
| 4 | 未證明覆寫 ON 成功寫入 | smoke 13b：新 payload（兩列內容都不同於磁碟）→ 對長中文路徑列先開覆寫再**排除**、對 existing.txt 開覆寫 → 套用 | `created=0 overwritten=1 errors=0`；existing.txt == `dangerous overwrite attempt`；被排除列保持 `new file restored bytes`。原 stale-abort（步驟 11）與預設關閉跳過（步驟 12）保留 |
| 5 | Ctrl+Q 逾時仍綠、不檢查 exit status | `quit_cleanly`：5 s 內須以 status 0 結束；逾時先 kill+wait 再 panic；非 0 直接 fail | 突變測試（見下）|
| 6 | 範圍說明 | 見下方「未完成與風險」更新 | — |

### 突變檢查（皆已還原；`grep -rn MUTATION crates/desktop-native crates/core/src` 無結果，`clip::write_text(&copy_res.payload)` 仍為 2 處）

1. 移除 Project 複製的 `clip::write_text`（COPY_DONE 仍輸出）→ smoke **FAIL** 於 `smoke.rs:716` `clipboard must contain unchanged.txt`（1 passed／1 failed）。
2. Quit action 改為 no-op → smoke **FAIL** 於 `smoke.rs:244` `app did not exit within 5s of Ctrl+Q (killed)`。

### 第二輪命令與結果（還原後最終狀態）

| 命令 | 結果 |
| --- | --- |
| `cargo fmt --all --check` | exit 0 |
| `RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets --locked -- -D warnings` | exit 0 |
| `SNIP_REQUIRE_ALL_TESTS=1 RUSTFLAGS="-D warnings" cargo test -p snip-desktop-native --locked` | exit 0；unit **12** passed；smoke **2** passed（26.88 s）|
| `just preflight-frontend`（無 pipe） | **exit 0**；Prettier、tsc、oxlint 0 warning、21/21 tests、vite build；log `~/.local/share/rtk/tee/1790335870_just_preflight-frontend.log` |
| `just desktop-e2e`（Tauri 22 scenarios）、完整 `just preflight-rust` | 本輪**未重跑**：第二輪只改 `crates/desktop-native`（不在 Tauri／core／CLI 依賴路徑）。沿用第一輪結果：22/22、preflight-rust exit 0（見下方第一輪紀錄）|

截圖（第二輪最終 run，19:30，fixture `/tmp/.tmpcZ4FVL`，已逐張檢視）：`target/native-e2e-artifacts/` 下 `graph.png`、`file_tree.png`、`paste_preview.png`、`workbench_900x600.png`、`paste_preview_900x600.png`、`workbench_900x600_en.png`。版面與第一輪一致。

---

以下為第一輪紀錄（部分敘述已被上表取代：probe 不再常駐、smoke 流程已擴充）。

## D0 接手與重現

- 清除 `GIT_DIR/GIT_WORK_TREE/GIT_COMMON_DIR` 後確認：root `/home/audichuang/research/snip-sync`、branch `feature/lightweight-git-workbench-plan`、HEAD `7ed02790b63cb8e051bf726828daa1fbfcd765ce`、base `9be8684f…` 為 HEAD 祖先。既有 dirty／untracked 檔全數保留，未 reset／clean。
- 重現 `cargo fmt -p snip-desktop-native -- --check` 失敗（graph_view／main／paste／tree／smoke）。
- 重現 smoke 失敗：`smoke.rs:509` 等 `PASTE_TOGGLED idx=1`，只收到 `PASTE_NAV idx=1`。截圖確認根因：app 以 `app_log!` 輸出**寫死**的座標（`x=980`），實際覆寫按鈕因列中長路徑把它推到約 x≥1000 並溢出 1080 右緣；x=980 落在列本身的空白處，只觸發列選取。兩個症狀是同一缺陷：假座標 + 列內無 min-width／shrink 控制。
- Host：Linux X11 `DISPLAY=:1`；GPUI link 需單次命令 `LIBRARY_PATH=/home/audichuang/.local/lib`（未 commit 任何 linker hack）。

## D1 修改內容

| 檔案 | 目的 |
| --- | --- |
| `crates/desktop-native/src/theme.rs`（新） | 共用色彩／尺寸 token：IntelliJ New UI dark（editor `#1e1f22`、panel `#2b2d30`、選取 `#2e436e`、accent `#3574f0`、Git 狀態色），header 38px、status 22px、row 24px、rail 36px、UI 13px／小字 12px／code 12.5px |
| `crates/desktop-native/src/ui.rs`（新） | 整個 render 重寫並從 main.rs 移出：header（workspace／repo／branch + 複製／貼上預覽／重新整理／語言）、窄 tool-window rail（專案、變更、Git 記錄）、左側 Project／Changes tool window、可拖曳垂直／水平 splitter、中央 editor（tab + breadcrumb + 來源標籤 + 行號高亮）、跨越左欄+editor 的底部 Git Log（refs 側欄分組 local／remote／tags + graph／訊息／作者／日期／雜湊欄）、status bar。貼上預覽取代 editor 區，**只有一組 套用／取消**；每列：納入 checkbox、操作（建立／覆寫／刪除／跳過／已排除）、獨立覆寫開關、路徑（…截短）、大小；下方為原因與內容 |
| `crates/desktop-native/src/main.rs` | 移除舊 render（~2000 行 Catppuccin 版面）與所有假座標 log（`PASTE_ROW_POS/PASTE_OPT_BTN_POS/APPLY_BTN_POS/TREE_ROW_POS`）；加入版面狀態（`left_w/bottom_h/left_visible/bottom_visible/dragging/bounds_log`） |
| `crates/desktop-native/src/graph_view.rs` | row 高改用 theme；ref label 改為克制的文字色 + 共用底色，移除 🏷 emoji |
| `crates/desktop-native/src/syntax.rs` | IntelliJ dark 語法色 |
| `crates/desktop-native/src/tree.rs` | 移除 ❌ emoji |
| `crates/desktop-native/src/i18n.rs` | 新 UI 的繁中／英文字串 |
| `crates/desktop-native/tests/smoke.rs` | 以真實 bounds 取代寫死座標（見下） |
| `crates/core/src/graph.rs` | **僅測試碼**：3 處 `&[x.clone()]` → `std::slice::from_ref(&x)`。這是已 commit 的 `7ed0279` 在目前 clippy 下的既有失敗，阻擋 `just preflight-rust`；未改任何非測試邏輯 |

### 實作選擇

- **真實 bounds 取代假座標**：每個測試會點的控制項內放一個 absolute `canvas` probe，在 prepaint 取得實際 layout bounds（× `scale_factor` 轉實體像素），僅在變動時輸出 `[APP:CTRL_BOUNDS: id=… x y w h]`；另輸出 `[APP:VIEWPORT: WxH]`。id 用語意識別（`tree-row:<rel_path>`、`tree-chk:<rel_path>`、`paste-overwrite:<path>`、`btn-apply` 等），不用列 index。canvas 無 hitbox，不攔截點擊。
- **溢出修正**：所有含文字的 flex 列：文字 `flex_1 + min_w_0`，控制項 `flex_shrink_0`；覆寫開關固定寬度欄位放在路徑之前，不會被長路徑推出畫面。
- **「…」截短**：GPUI 0.2.2 在 `whitespace_nowrap` 時以 wrap_width=None 快取第一次（未受限）的文字 layout，之後不再截短，只裁切。改用 `line_clamp(1) + text_ellipsis()`，寬度參與快取鍵，縮放視窗會重新截短。
- **導覽不改選取**：舊 ↑/↓ 在變更清單會 toggle 勾選（違反 UI02「導航不偷改選取」）。改為只移動預覽；空白鍵才切換勾選。
- **Rail 行為**：點另一個 tool window 開啟之；點目前啟用者收合面板；切換不清除任何待複製選取。Git Log 可由 rail 或「隱藏」收合；splitter 值收合後保留，渲染時依視窗大小夾限（左欄 ≤45%、底部 ≤55%），不改寫使用者設定值。
- 未新增任何依賴；仍為 GPUI 0.2.2 + snip-core。圖示以小型 div 形狀繪製（資料夾、檔案、變更、log），無 emoji、無 SVG asset。
- 貼上仍走既有 `PastePreviewPlan`（`plan_restore`／`execute_restore_plan`＋ freshness 檢查），CLI／wire 格式／contract fixture 未變。

### Smoke test（真 OS 輸入）

`crates/desktop-native/tests/smoke.rs` 保留所有原有行為斷言（sentinel、真剪貼簿 bytes、Escape 不寫、own-export Apply bytes、stale 阻擋且 create/delete/overwrite 均未執行、fresh Apply 後 create／overwrite-off 保留／delete），並：

- 所有點擊改由 `click(id)`：讀 app 最新回報的 bounds，**斷言控制項完整位於 viewport 內**後點其中心（舊版面會在此斷言失敗）。
- fixture 新增長中文目錄／檔名與長中文 branch；3 項貼上 payload 的建立項改為長中文路徑。
- 新增：Project→Changes→Project 後斷言 `selected=1`（切換不清選取）；Git Log refs 點 `HEAD` 斷言 `commits=3`（排除未合併 tip），點「全部 refs」回到 `commits=4`；左／下 splitter 真滑鼠拖曳並斷言 `SPLIT_RESIZED`；`xdotool windowsize` 到 900×600 後斷言 header 與貼上面板控制項都在視窗內、以真滑鼠點「取消」且目的檔不變；再開預覽、真滑鼠只打開 existing.txt 的覆寫、點「套用」，斷言 `created=0 overwritten=1 skipped=1`、existing.txt 變為 payload bytes、覆寫關閉的另一列不變（UI04）；`alt+l` 切英文後在 900×600 再斷言 header 四個按鈕都在視窗內。
- 新增 `bounds_line_parsing` 單元測試。

## 執行命令與結果

全部於主 checkout，`LIBRARY_PATH=/home/audichuang/.local/lib` 僅用於單次命令。

| 命令 | 結果 |
| --- | --- |
| `cargo fmt --all --check` | exit 0 |
| `RUSTFLAGS="-D warnings" cargo clippy -p snip-desktop-native --all-targets --locked -- -D warnings` | exit 0 |
| `SNIP_REQUIRE_ALL_TESTS=1 RUSTFLAGS="-D warnings" cargo test -p snip-desktop-native --locked -- --nocapture` | exit 0；unit 9 passed；smoke 2 passed（`native_desktop_smoke_and_clipboard_verification` 約 10 s）|
| `SNIP_REQUIRE_ALL_TESTS=1 just preflight-rust`（fmt、workspace clippy、doc、workspace test） | exit 0；core 177、contract 7、clipboard 1、CLI 2+7、e2e 6、native 9+2、tauri lib 2（1 ignored 為既有 `export_dto`）、doctest 1 |
| `just desktop-e2e` | **22/22 scenarios passed**（I01–I03、H01、S00、C01–C10、F01–F07）；log `~/.local/share/rtk/tee/1790335021_just_desktop-e2e.log`，截圖 `/tmp/snip-e2e-shots-VDBWkS` |
| `just desktop-e2e` exit code | exit 0 |
| `just preflight-frontend`（最終） | exit 0；Prettier 通過、tsc、oxlint 0 warning、node tests 21 pass／0 fail、vite build；log `~/.local/share/rtk/tee/1790335321_just_preflight-frontend.log` |
| 最後修改後（refs probe、UI04、英文 900×600）：`cargo fmt --all --check`、`cargo clippy --workspace --all-targets --locked -D warnings`、`SNIP_REQUIRE_ALL_TESTS=1 cargo test -p snip-desktop-native --locked --test smoke` | 全部 exit 0；smoke 2 passed（12.98 s）|

`just preflight` 三段（rust／frontend／desktop-e2e）皆分段執行過且 exit 0；未以單一 `just preflight` 指令執行。`preflight-rust` 是在最後一輪 smoke 修改（refs／UI04／英文檢查）**之前**跑的；之後重跑了 fmt、workspace clippy 與 smoke，未重跑整個 workspace test（core／CLI 未再變動）。CI（audit、DTO drift、clean checkout、Windows／macOS）未跑。

## 截圖（2026-09-25 19:21，最後一次 smoke run，fixture `/tmp/.tmp1zfm6J`，HEAD `7ed0279` + 本 working tree）

`/home/audichuang/research/snip-sync/target/native-e2e-artifacts/`

- `graph.png`（1080×720）：Changes tool window、working diff、底部 Git Log：fork／merge rails、未合併 tip、長中文 branch label「…」截短。
- `file_tree.png`（1080×720）：Project 樹，repo 節點含 branch 與 `+1 ~1 ?2`，展開資料夾、長中文目錄「…」截短、nested.txt 內容與「工作目錄檔案」來源標籤。
- `paste_preview.png`（1080×720）：單一 套用／取消；建立（長中文路徑…截短）／跳過（existing.txt，覆寫關）／刪除三列；內容預覽與原因。
- `workbench_900x600.png`、`paste_preview_900x600.png`：900×600 所有按鈕可見、無裁切。
- `workbench_900x600_en.png`：英文 locale 900×600，header 按鈕完整。

六張皆為同一輪產生，已逐張檢視。已知視覺限制：graph 為 24px 列高，lane 較緊湊；IME／縮放／高 DPI 未驗證。

## 未完成與風險（交 D2–D5）

- Hindsight knowledge 搜尋工具在此 session 被權限拒絕（無核准介面），未查閱其既有決策頁。
- 只在 Linux X11 驗證；macOS／Windows 原生輸入、IME、accessibility、scale factor ≠1 未驗證（probe 已乘 scale，但未實測）。
- Changes 仍是既有 `GitSource::Working` 相容集合，標示為「工作目錄變更」，未拆 staged／unstaged／untracked／conflicts（D2/D3）。Project 與 Changes 各自保有選取，複製來源依目前 tool window，狀態列顯示「複製來源」；統一待複製籃屬 D3。
- Header 顯示 workspace／repo／branch 為文字，沒有下拉選單（依規格不畫未實作的選單）；repo 切換在 Project 樹或 Alt+1/2。
- 無 SHA／message 搜尋、比較、collapse、歷史檔案樹（D3）；bounded runner、transfer 整合、absence freshness 等 P1 阻擋項未處理（D2）。
- 記憶體量測未做（D4）；`native-workbench-progress.md` 內舊數據仍未被接受。
- （第二輪）probe 僅在 `SNIP_NATIVE_E2E=1` 啟用；一般執行不建立。其他 `APP:` 事件 log（REPO_LOADED、PASTE_* 等）仍一律 println+flush，屬低頻事件，未改。
- **記憶體未驗收**：編輯器與貼上預覽的 `code_lines` 會為全部行（上限 500 行／64 KiB 的有界文字）建立元素，沒有虛擬化；`PasteItem.content` 在 plan 中完整複製、套用時再 clone 一次 plan。這些截圖不代表 retained-data／memory 預算達標，virtualized rendering 與 retained-data budget 屬 D2/D3，量測屬 D4。
- **歷史 commit 複製未支援**：選取 commit 時「複製」停用並顯示唯讀原因；commit／range 複製與 replay 接入 native UI 是 D3 缺口，不是已完成功能。
- `SNIP_NATIVE_E2E_APPLY_DELAY_MS` 是測試專用鉤子，只在 E2E 模式生效；套用本身仍不可中途取消，UI 以鎖定控制項處理，不宣稱 rollback。
- smoke 結束時 xdotool 對已關閉視窗送 key 會印出一行 `BadWindow`，不影響結果。
- `crates/core/src/graph.rs` 的 3 個 clippy 問題也存在於 `snip-native-graph` worktree 的 `1e115bd`；整合時會再遇到。
- `native-workbench-progress.md` §1 仍描述已被取代的三欄式原型；該檔是 worker 紀錄，未修改，請視為過時。
- `Tab` 全域綁定為切換 Project／Changes（沿用既有），會和鍵盤 focus traversal 衝突，屬 U01（D3/D4）。
- 狀態列的操作訊息（如「儲存庫「repo-a」…」）仍是 main.rs 內寫死的繁中字串，英文模式下未翻譯（見 `workbench_900x600_en.png`）。
