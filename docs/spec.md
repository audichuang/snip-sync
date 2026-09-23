# snip-sync 功能規格

> 狀態:規格定稿(2026-09-23,經逐題確認)。實作細節與移植陷阱見 [plan.md](plan.md)、[porting-notes.md](porting-notes.md)。
> 本文件描述**做什麼**;與本文件衝突時,以本文件為準。

## 1. 產品定位

- 在一台電腦**複製**,在另一台電腦**貼上還原**。只負責「複製」與「貼上還原」兩件事。
- **完全雙向**:同一個程式在每台電腦上都能複製、也能貼上。沒有固定的來源機或目標機。
- 支援 **macOS、Windows、Linux**。
- **不管傳輸**:兩台電腦之間剪貼簿怎麼過去(RDP、VDI、手動…)不是本工具的範圍。
  不做分段、不做完整性雜湊。
- **不管衝突**:貼上一律覆蓋,狀態由使用者自己判斷與管理。工具只負責「讓使用者在確認前看清楚會發生什麼」。
- **只給自己用**:不處理程式碼簽章、公證、自動更新、Homebrew。

## 2. 兩種模式

| | 檔案模式 | commit 模式 |
|---|---|---|
| 用途 | 把檔案內容帶到另一台並覆蓋 | 讓另一台出現**同樣 message、同樣作者與時間、同樣檔案異動**的 commit |
| 貼上後 | working tree 的檔案被覆蓋,`git status` 看得到差異,commit 由使用者自己做 | 依序建立 N 個 commit |
| 剪貼簿格式 | 與 IDE 套件(ClipCode / Snipcode)**完全相同** | snip-sync 專用,IDE 套件不認得 |
| 與 IDE 套件互通 | ✅ 雙向 | ❌ 只有 snip ↔ snip |
| 目標需要是 git repo | 否 | 是 |

貼上時**自動判斷**剪貼簿內容是哪一種模式,使用者不需要選。

**明確不做:** 精確模式(逐位元組還原)、分段 / 雜湊、衝突偵測、三方合併、保留 commit hash、不連續 commit。

## 3. 檔案模式

### 3.1 複製

可以複製的來源:

| 來源 | CLI |
|---|---|
| 指定的檔案或資料夾(目前磁碟上的內容) | `snip copy <路徑…>` |
| working tree 的變更 | `snip copy --working` |
| staged(index)內容 | `snip copy --staged` |
| 單一 commit 的變更 | `snip copy --commit <sha>` |
| commit 區間(兩個端點之間的差異) | `snip copy --range <a>..<b>` |

規則(與兩個 IDE 套件一致,由共用 contract fixture 驗證):
- 剪貼簿格式與 IDE 套件**逐位元組相同**。已知限制照舊:不保留檔尾換行與前後空行,CRLF 變成 LF。
- 每個檔案帶變更標籤 `[NEW] [MODIFIED] [DELETED] [MOVED]`(git 來源時)。
- 刪除的檔案帶刪除前的內容。
- merge commit 取**與每一個 parent 的 diff 的聯集**(依路徑去重)。
- `a..b` 是**兩個端點**的比較,不是把區間內每個 commit 的變更相加。
- 非 UTF-8 檔案(含二進位、UTF-16)**跳過**,不進剪貼簿,只計入通知。
- 讀不到的檔案放 placeholder,不算已複製、不佔檔案數上限。
- 過濾規則、大小與數量上限:與 IDE 套件相同。

複製完成的通知:**與 IDE 套件相同**,顯示檔案數、字元數、行數、字數、token 數,以及跳過了幾個檔案。

### 3.2 貼上還原

1. 讀剪貼簿 → 解析 → 產生**還原計畫**:每個檔案一列,標示動作與原因。
   - 動作:新增 / 覆寫 / 刪除 / 跳過。
   - 跳過的原因例如:路徑不合法、超出 root、目標不是 UTF-8、目標讀不到或大於 8 MiB、內容是 placeholder。
2. **預覽**:使用者可逐檔勾選;覆寫的檔案可展開看 diff(目前的目標檔 ↔ 剪貼簿內容)。
3. 確認後執行。執行前**重新檢查**一次路徑與編碼(預覽期間檔案系統可能已經變了)。
4. 結果:成功 / 跳過 / 失敗各幾個,失敗的列出原因。

- **一律覆蓋**,不偵測目標是否被改過。
- 安全規則照 porting-notes 第 3 節:路徑含控制字元或 `<>:"|?*` 拒絕、containment 以 realpath 判斷、
  placeholder 永遠不寫到真實檔案、目標不是 UTF-8 不覆寫、所有寫入一律 UTF-8。

CLI:`snip paste --dry-run`(只列計畫)、`snip paste --apply [--overwrite | --skip-existing]`。

## 4. commit 模式

### 4.1 選擇 commit

- GUI:在**歷史時間軸**(commit graph)上選一段:點起點,Shift + 點終點。
- CLI:`snip copy --commits -n <N>`(從 HEAD 往回 N 個)、`snip copy --commits <a>..<b>`。
- **必須連續**:選取的 commit 必須能從起點沿著 **first parent** 一路走到終點。
  不連續就在複製時拒絕,並說明哪裡斷開。
- 範圍內有 merge commit:當成一般 commit,只帶它與 **first parent** 的差異
  (被合併進來那條分支上的個別 commit 不會出現)。

### 4.2 每個 commit 帶的內容

- commit message(完整,含多行)
- 作者名稱與 email
- 作者時間(含時區)
- 檔案異動清單:每個檔案的路徑、變更類型(新增 / 修改 / 刪除 / rename,rename 帶舊路徑)、
  以及**異動後的完整內容**(刪除則不帶內容)。

**不帶:** commit hash、parent、committer、簽章。

**非 UTF-8 / 二進位檔:不阻擋。** 該檔案仍列在清單中,標記為「未複製」與原因,**不帶內容**;
貼上時不寫入、不刪除這個檔案,其餘檔案照常建立 commit。預覽與通知都要顯示「第幾個 commit 少了哪些檔案」。

複製完成的通知:commit 數、檔案數、字元數,以及未複製的檔案數。

### 4.3 貼上

1. 讀剪貼簿 → 解析 → **預覽**:列出即將建立的 N 個 commit(message、作者、時間),
   每個 commit 可展開看檔案清單與 diff;「未複製」的檔案要標出來。
2. 確認後,**依原本順序**對每個 commit:
   1. 寫入新增 / 修改的檔案、刪除被刪除的檔案;rename = 刪舊路徑 + 寫新路徑。
   2. 只 stage 這個 commit 涉及的路徑。
   3. 建立 commit,**只提交這些路徑**(使用者原本 stage 的其他東西不會被帶進去),
      作者與作者時間用剪貼簿帶來的值,committer 是本機使用者。
3. 疊在**目前分支的 HEAD** 上,不需要與來源有共同的起點,也不檢查是否 fast-forward。
4. 結果:建立了幾個 commit。

- 涉及的路徑如果本機有尚未 commit 的修改,**直接覆蓋**(符合「蓋上去」的原則)。
- 中途某個 commit 建立失敗:**停下來**,回報已建立的前幾個、失敗的是哪一個與 git 的錯誤訊息。已建立的不回滾。
- 路徑安全規則與檔案模式相同。
- 預設值(規格階段未逐題確認,實作時照此,有意見再改):
  - 某個 commit 在本機寫入後沒有任何差異 → 仍然建立(空 commit),讓兩邊的 commit 數量與 message 一致。
  - 建立 commit 時**不執行** git hooks(`--no-verify`):內容在來源端已經 commit 過,
    避免本機的 pre-commit 修改或擋下重播的內容。

### 4.4 剪貼簿格式

snip-sync 自訂,只要求 snip ↔ snip 互通:
- 第一行是固定 marker(例如 `// snip-sync commits v1`),用來和檔案模式區分。
- 其後是 JSON,內容即 4.2 的欄位。marker 帶版本號,格式日後可擴充。

## 5. 介面

### 5.1 桌面 App

- **常駐系統匣**。選單:從剪貼簿貼上、複製上一次的選取、開啟主視窗、結束。
- **主視窗**(同一個視窗,可從系統匣的小尺寸展開):
  - 選 repo / 資料夾。
  - 檔案模式:選來源(檔案、working tree、staged、commit、區間)→ 複製。
  - commit 模式:歷史時間軸,選一段連續 commit → 複製。
  - 貼上:預覽(3.2 / 4.3)→ 確認 → 結果。
- 複製與貼上的通知內容見 3.1、4.2。
- 語言:繁體中文與英文。

### 5.2 CLI(`snip`)

```
snip copy <路徑…>
snip copy --working | --staged
snip copy --commit <sha> | --range <a>..<b>
snip copy --commits -n <N> | --commits <a>..<b>
snip paste --dry-run
snip paste --apply [--overwrite | --skip-existing]
```

CLI 與 App 呼叫同一組核心函式,行為完全相同。

## 6. 技術決策(摘要)

- Rust + Tauri 2,前端 React 19 + TypeScript + HeroUI v3 + Tailwind v4,結構比照 aghub(見 plan.md)。
- git 一律呼叫**系統的 git CLI**(假設兩台都有安裝 git;啟動時檢查,沒有就提示)。
  不用 git2 / gix,以確保 rename 偵測、`.gitattributes` 等行為與本機 git 一致。
- 前端元件:時間軸 `@tomplum/react-git-log`(HTML Grid 模式)、diff `@pierre/diffs`、
  可勾選檔案樹 `@rc-component/tree`。都需要在三個平台的 WebView 實測大 repo 與大檔。
- 不 fork 現成 git 客戶端;[GitDesktop](https://github.com/theBGuy/GitDesktop)(Apache-2.0,Tauri + React + 系統 git)
  可參考殼與畫面流程。GitButler 為 FSL 授權,不借用其程式碼。

## 7. 驗收條件

- 檔案模式:共用 contract fixture 全數通過;snip 產生的 payload 可被兩個 IDE 套件還原,反之亦然。
- commit 模式:在 fixture repo 上選 3 個連續 commit(含一個 rename、一個刪除、一個 merge、一個二進位檔)
  → 複製 → 貼到另一個 clone 的不同分支 → 產生 3 個 commit,message、作者、作者時間與檔案內容相同,
  二進位檔標示為未複製且其餘內容正確。
- 選不連續的 commit 時,複製被拒絕並說明原因。
- 三個平台都能完成一次「A 複製 → B 貼上」與「B 複製 → A 貼上」。
