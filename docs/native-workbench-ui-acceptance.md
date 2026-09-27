# 原生工作台 UI 驗收補充

2026-09-25，使用者明確要求以 IntelliJ 風格重做目前原型。本文是監督者的派工／驗收規格；不是完成報告。既有完整功能及記憶體門檻仍依 `native-git-workbench-plan.md`。

## 參考與版面

已閱讀 [IntelliJ New UI 官方文件](https://www.jetbrains.com/help/idea/new-ui.html)，並透過瀏覽器檢視官方 [Compact mode 圖片](https://resources.jetbrains.com/help/img/idea/2026.2/ij-new-ui-compact-mode.png)。沿用其緊湊工具列、Project 工具視窗、editor tabs、窄工具視窗列及一致的資訊密度；應用仍保留 snip-sync 名稱。

- 頂部只保留一組工作區／repo／分支資訊與主要同步操作。
- 左侧窄工具列，Project／Changes 面板可調寬；樹的展開、導航與待複製勾選是不同操作。
- 中央保留主要閱讀空間：檔案 tab、路徑及來源、行號、高亮／diff。
- 下方 Git Log 是橫向工具視窗，可調高／收合，包含 repo／ref 篩選及 graph、message、author 等欄位。不可再把 graph 擠在左側樹底部。
- 貼上為獨立且完整的預覽區，單一 Apply／Cancel 區域，顯示目的地、操作、覆寫選擇與內容。不能有兩套重複按鈕。
- 使用一致的中性深灰背景、細分隔線、克制的藍色選取與 Git 狀態色；以可辨識小圖示取代 emoji。UI 字體與程式碼字體分開，間距和列高統一。

派工的初始設計尺寸為工具列 36–40px、樹列 24–28px、UI 字體 12–13px、狀態列約 22px；這些是本產品目標，不是官方設計 token 的精確數值。

## 主題系統(IntelliJ Islands)

2026-09-28 起,外觀對齊 IntelliJ New UI 的 **Islands Dark / Islands Light**。

- 色彩:`crates/desktop-native/src/theme.rs` 的 `Palette` 有 `DARK`、`LIGHT` 兩組,數值取自 JetBrains/intellij-community 的 `platform/platform-resources/src/themes/islands/ManyIslands{Dark,Light}.theme.json`(UI 語意色、`VersionControl.GitLog.*` ref 色、`VersionControl.Log.Graph` 飽和度／亮度)與 `IslandSchemeDark.xml`、`expUI/expUI_{dark,light}Scheme.xml`(editor 文字、FILESTATUS、行號、搜尋、語法色);diff 行底色取自其父配色 Darcula／Default。每個欄位旁註記來源 key。半透明 token(hover)預先疊在 island 底色上。
- 所有 render 程式碼只經由 `pal()` 取色,原始碼中不得再出現 `rgb(0x…)` 常值。切換 palette 後重繪即可整窗換色。
- 選擇:預設跟隨作業系統外觀(`window.appearance()`,並以 `observe_window_appearance` 監聽即時切換);環境變數 `SNIP_THEME=dark|light` 可強制指定。原生 app 目前沒有持久化設定存放區(`snip_core::settings::Settings` 是剪貼簿格式契約,不放 UI 偏好),所以沒有設定頁選項。
- 版面:frame(`main-window-bg`)墊在最底,左側工具視窗、editor、下方 Git Log 為各自的圓角 island(半徑 8px),彼此以 4px 的 frame 間隙分隔(splitter 即間隙,不再畫 1px 分隔線)。header、左側 stripe、狀態列直接畫在 frame 上;開啟中的 stripe 按鈕為實心 accent 圓角,editor 的作用中 tab 為實心圓角 pill。
- 字型:內嵌 Inter(UI,13px)與 JetBrains Mono(程式碼,13px),於啟動時 `add_fonts` 註冊,不依賴系統字型;中日韓字元由 GPUI 字型 fallback 取系統字型(CI 安裝 `fonts-noto-cjk`)。授權見 README「第三方授權」。內嵌字型使執行檔增加約 1.05 MiB。
- 測試:像素檢查(smoke 的錯誤橫幅、focus ring、警告字色,IME 的近白候選框偵測,bench／collaboration 截圖)一律以 `SNIP_THEME=dark` 啟動並對 `DARK` 數值校正;`native_light_theme_renders` 以 `SNIP_THEME=light` 輸出 `light_theme.png` artifact 並只檢查整窗偏亮。`theme.rs` 單元測試確保 `DARK`／`LIGHT` 除品牌藍系 token 外逐欄不同。

## 驗收門檻

1. 1080×720 及 900×600 真實視窗都能操作；長路徑、繁中及多 repo 名稱不能把動作按鈕擠出畫面。
2. 截圖必須包含真實 file tree、內容、分岔／合併 graph，以及有新增／覆寫／刪除的貼上預覽。用真資料，不能交靜態 mockup。
3. 主要控制項必須有真實行為；未完成的搜尋／選單不可畫成可用。
4. 真鍵鼠測試需完成檔案選取、系統剪貼簿複製、目的端預覽、取消、逐項覆寫選擇、套用及磁碟內容檢查。來源／目的地改變時阻止過期計畫。
5. 測試不能依賴假造的座標日誌；取實際控制項 bounds 或用穩定語意／鍵盤定位，保留真實滑鼠點擊覆寫按鈕的覆蓋。
6. 既有 clipboard contract、來源語意與全套 real-app E2E 不得因改版降級。Rust fmt/clippy/tests、完整 preflight、CI 仍是合併條件。
7. 截圖檢查與功能測試都通過才接受 UI；AGY 自述完成不代表通過。

## 實作交接

AGY brief：`/tmp/snip-agy-intellij-redesign.txt`。
2026-09-25 17:06 台灣時間重新派工：`implement-mugqllww-4fd0945f`。結果於 17:09 回收：`Individual quota reached`，未執行實作；服務預估 19:01:41 恢復，尚未驗證。
其他核心及效能工作以 `native-workbench-supervision.md` 的未解項目為準。
