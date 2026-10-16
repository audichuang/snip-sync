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
- **只給自己用**:不處理程式碼簽章、公證、自動更新。正式版會發到自己的 Homebrew tap。

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
- 複製上限統一為 32 MiB(core 常數 `transfer::CLIPBOARD_PAYLOAD_MAX`,兩個介面共用,與貼上預覽預算一致;GUI 複製上限已為 32 MiB(階段 2),CLI 於階段 3/4 採用);超過是明確錯誤(CLI exit 1),不截斷。
- `snip copy <路徑…>` 規則(CLI 於階段 4 採用):相對路徑以 cwd 解析;路徑不存在 exit 1;路徑在 `--repo` 外 exit 1;指向 root 外的 symlink、FIFO/裝置檔與 `.git`/巢狀 repo 略過修剪;結果為空時顯示「No files selected.」exit 1 且不改剪貼簿;`--repo` 含 `..` 時先以 cwd 詞法解析,標籤為 root 相對路徑、`clipcode-root` 為解析後 basename。

複製完成的通知:**與 IDE 套件相同**,顯示檔案數、字元數、行數、字數、token 數,以及跳過了幾個檔案。

### 3.2 貼上還原

1. 讀剪貼簿 → 解析 → 產生**還原計畫**:每個檔案一列,標示動作與原因。
   - 動作:新增 / 覆寫 / 刪除 / 跳過。
   - 跳過的原因例如:路徑不合法、超出 root、目標不是 UTF-8、目標讀不到或大於 8 MiB、內容是 placeholder。
2. **預覽**:使用者可逐檔勾選;覆寫的檔案可展開看 diff(目前的目標檔 ↔ 剪貼簿內容)。
3. 確認後執行。執行前**重新檢查**一次路徑與編碼(預覽期間檔案系統可能已經變了)。
4. 結果:成功 / 跳過 / 失敗各幾個,失敗的列出原因。

- 規劃時已略過的項目仍在預覽逐筆列出原路徑與原因，不能勾選成寫入；Git metadata 保護明確說明原因。結果的跳過總數包含規劃略過與執行時保留既有檔案，各計一次；使用者取消勾選的操作不列入跳過。全部項目略過時顯示「未套用任何檔案；略過 N 項」。

- 預設仍是覆蓋已存在的檔案(使用者確認後),但新增兩道防護(transfer / `plan_import_with`,GUI 與 CLI 於階段 6 皆已採用):
  (a) `TransferError::TargetCollision`:計畫中兩筆 entry 指向同一個實體檔(大小寫差異片段若位於已存在路徑部分,如檔案或目錄已存在,會由 realpath 解析偵測;偵測目的端是否不分大小寫,不分大小寫則新目標也摺疊大小寫(D10,階段 5b 已實作);以及 symlink 別名、同一路徑出現兩次)就整批拒絕;錯誤訊息格式呈現為單一目標路徑並列出衝突的操作名稱（如 `target collision: multiple operations target '<p>': previous was 'create a', current is 'create b'`，不重複輸出路徑亦不暴露內部大小寫摺疊字串）;
  (b) freshness:預覽後目標檔或 repo 的 HEAD/index 有變,套用時拒絕(`TransferError::StaleDestination`),需重新預覽。
  (c) 寫入鎖:檔案貼上與 commit 重放套用前,先取得目的地每個 repo 的寫入鎖(git dir 裡的 `snip-paste.lock`,OS 檔案鎖,跨行程有效,行程結束即釋放),拿到鎖之後才重新檢查 freshness 並寫入。另一個貼上(另一個工作區分頁、遠端 worker 或 CLI)正持有鎖時立刻拒絕、什麼都不寫,不排隊等待(`GitError::WorktreeBusy`;桌面顯示「這個儲存庫正在被另一個貼上作業使用，請稍後再試」,CLI exit 1)。不在任何 repo 裡的資料夾不加鎖:鎖只放在 git dir,不在一般資料夾留下檔案。無法建立或鎖住鎖檔(而非被占用)時整批以 `Io` 拒絕,不會略過鎖照寫。
  這與 IDE 套件(TS)不同,見 porting-notes「已知且接受的差異」。
- 安全規則照 porting-notes 第 3 節:路徑片段等於 `.git`(ASCII 不區分大小寫,含 Win32 結尾點或空白拼寫如 `.git.`)視為 unsafe/unresolved 拒絕(檔案模式的寫入與刪除、commit 模式的 `path`/`old_path` 皆阻擋)、路徑含控制字元或 `<>:"|?*` 拒絕、containment 以 realpath 判斷、寫入或刪除的目標把 symlink 解析到底後落在 Git 目錄(repo 的 `.git`、bare repo、獨立 git dir)裡的,規劃時列為略過(檔案模式 UNRESOLVED_PATH、commit 模式 UnsafePath)、寫入前再檢查一次(只拒絕那一筆,其餘照寫)、
  placeholder 永遠不寫到真實檔案、目標不是 UTF-8 不覆寫、所有寫入一律 UTF-8,而且一律以「同資料夾暫存檔 + rename」**取代目錄項目**:硬連結或 symlink 的另一個名字(或指向的檔案)永遠不會被寫穿,三個作業系統一致。
- CLI 補充(現行行為/CLI 於階段 6 已切換):絕對路徑在 sanitize 前先解析(root 內部、跨機器後綴 → 相對路徑,同 TS);絕對 [DELETED] 解析不到 root → 拒絕(視為 unsafe/unresolved 跳過,同 TS);寫入對不到 root 的 POSIX 絕對路徑(如 `/Users/bob/other/src/a.ts`)→ 去首斜線放主 root 下(同 TS);帶磁碟機代號的路徑(如 `D:\work\lib\b.ts`)因 `sanitize_relative_path` 的絕對路徑/磁碟機檢查(`is_absolute_path`/`has_drive_slash`)而跳過(`UNRESOLVED_PATH`),不再放進 `D/work/...`。CLI 只有單一 root。`paste` 的 `--repo` 必須已存在,否則 exit 1(見 porting-notes「已知且接受的差異」)。
- 目的端路徑為目錄、FIFO 等非一般檔案時整批以 `DestinationNotRegular` / `SpecialFile` 拒絕(含 `--dry-run`,exit 1;錯誤訊息明確提示貼上拒絕覆寫非一般檔案),見 porting-notes「已知且接受的差異」。父層片段為一般檔案或目的端為懸空 symlink 時亦以 `TransferError::Io` 整批拒絕(exit 1),同見該條目。

CLI:`snip paste --dry-run`(只列計畫)、`snip paste --apply [--overwrite | --skip-existing]`。

## 4. commit 模式

### 4.1 選擇 commit

- GUI:在**歷史時間軸**(commit graph)上選一段:點起點,Shift + 點終點（在兩端點間沿著 first-parent 鏈選取，自動略過分支上的 side commit；若無法構成 first-parent 鏈則退回可見列選取並在複製時拒絕；沿鏈只走目前顯示的 commit，若搜尋、ref 或路徑篩選隱藏了鏈中間的 commit（例如搜尋 `多行中文|C3 merge` 會隱藏 C2），選取就退回可見列範圍，複製時拒絕；清除篩選，或在隱藏的 commit 顯示時再選取，才能複製整條鏈）。
- 顯示本機所有分支、遠端追蹤分支、標籤與 HEAD 的拓撲圖;可依 ref 篩選、搜尋 message / SHA,每頁 50 筆並能繼續載入（單一 repo 與多 repo 合併的 log 相同）。瀏覽不切換分支、不自動 fetch。
- Log 的 Repository 篩選獨立於 Project 選取；單一 repo 的分支清單與 graph 都以該 Log repo 的 HEAD 標示目前位置。detached HEAD 不標任何分支為目前分支；多 repo 合併清單不顯示單一目前分支圖示，graph 仍依各列所屬 repo 判斷。
- 點 commit 顯示它的檔案樹、內容與 diff;包含 root 的選取固定從所選 tip 回溯,也支援尚未合併的其他分支。
- merge commit 的檔案清單保留 all-parent union，預覽 diff 比較 first parent；只相對其他 parent 改動、相對 first parent 不變的檔案顯示原文，不合成新增 diff。
- merge commit 的檔案清單中，相對 first parent 不變、只因其他 parent 才列出的檔案以 muted 文字標示，tooltip 說明「與 first parent 相同（只在其他 parent 有變更）」。標記由 first parent 與 merge 的一次額外比較算出，不需改協定。
- 多 repo 合併 log 的分支清單，每個 ref 後方以 muted 文字標出所屬 repo；同名 ref 出現在多個 repo 時顯示 repo 數（「N 個 repo」）。分支 chip 的 popup 同樣標示。
- 標題列的分支 chip 永遠顯示 Log repo 的 HEAD，不顯示 ref 篩選；ref 篩選只在 Log 的分支 chip 與 Log 面板標題顯示。
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
貼上時不寫入、不刪除這個檔案,其餘檔案照常建立 commit。刪除二進位 / 非 UTF-8 檔的 commit 也一樣:依刪除前的 blob 判斷,標為「未複製」,貼上不刪除目標端的同名檔案(文字檔不論大小仍照常重播刪除)。預覽與通知都要顯示「第幾個 commit 少了哪些檔案」。

複製完成的通知:commit 數、檔案數、字元數,以及未複製的檔案數。

### 4.3 貼上

1. 讀剪貼簿 → 解析 → **預覽**:列出即將建立的 N 個 commit(message、作者、時間),
   每個 commit 可展開看檔案清單與 diff;「未複製」的檔案要標出來。
   diff 以目的端及前序重播後的虛擬內容為基準：例如原檔 `already`、第一筆寫 `c1`、第二筆寫 `c2`，依序顯示 `already → c1`、`c1 → c2`。新增、刪除與 rename 同樣更新後續基準，略過項目不更新；預覽不修改目的端。二進位、未複製、連結或無法安全讀取的基準須顯示原因，不能冒充空檔 diff。本機與遠端採同一套核心規劃。
2. 確認後,**依原本順序**對每個 commit:
   1. 寫入新增 / 修改的檔案、刪除被刪除的檔案;rename = 刪舊路徑 + 寫新路徑。
   2. 只 stage 這個 commit 涉及的路徑。
   3. 建立 commit,**只提交這些路徑**(使用者原本 stage 的其他東西不會被帶進去),
      作者與作者時間用剪貼簿帶來的值,committer 是本機使用者。
3. 疊在**目前分支的 HEAD** 上,不需要與來源有共同的起點,也不檢查是否 fast-forward。
4. 結果:建立了幾個 commit。

- 此 commit 會寫入的目標檔在重播前已存在(`FilePlan.existed`),不論是否有未 commit 的修改,**預設不套用**,需明確允許覆寫(GUI:允許覆寫的開關,i18n `commit_overwrite_required`,paste.rs `execute_commit` 先 `PasteTarget::check_commits`(本機為 `preview.revalidate()`)再回此錯誤;CLI 判定與 GUI 相同[已決 D9],以 `FilePlan::overwrites_existing`(目標存在、動作為寫入、無 layout conflict)判定,需 `--overwrite`,沒給則 exit 2)。覆寫門禁與 dry-run 提示皆以相異目標路徑（distinct paths）計算（多個 commit 變更同一既有路徑僅計為 1 個）。CLI commit 貼上在 `--dry-run`（未指定 `--apply` 與 `--overwrite`）且目標已有檔案存在時，會在 stderr 提示「N destination file(s) already exist; --apply will need --overwrite.」（退出碼維持 0）。CLI 貼 commit payload 時 `--skip-existing`、`--adjust-paths` 不支援,exit 2。這與 TS/原規格「直接覆蓋」不同,見 porting-notes「已知且接受的差異」。
- 中途某個 commit 建立失敗:**停下來**,回報已建立的前幾個、失敗的是哪一個與 git 的錯誤訊息。已建立的不回滾。
- 路徑安全規則與檔案模式相同。
- 預設值(規格階段未逐題確認,實作時照此,有意見再改):
  - 某個 commit 在本機寫入後沒有任何差異 → 仍然建立(空 commit),讓兩邊的 commit 數量與 message 一致。
  - 建立 commit 時**不執行** git hooks:重播的每個 git 呼叫都帶 `-c core.hooksPath=<空目錄>`
    (`--no-verify` 只跳過 pre-commit / commit-msg,擋不住 prepare-commit-msg、post-commit 等)。
    內容在來源端已經 commit 過,避免本機的 hook 修改、擋下或改寫重播的內容。
  - 重播前先檢查整個 commit 的目標:寫入或刪除的位置是目錄、父目錄被一般檔案佔住時,
    整個 commit 拒絕且不動任何檔案(預覽標為 `UNSAFE_PATH`)。來源 message 為空也照樣建立。
  - 在不分大小寫的檔案系統(macOS、Windows 預設)上,路徑或其中任何一層資料夾和目的地磁碟上、
    index 裡的既有項目(以較早的 commit 處理後為準)或同一個 commit 的另一個路徑只差大小寫時,
    整個 commit 同樣在預覽與 Apply 都拒絕(`CaseAlias`)。磁碟會把它當成既有的項目,`git commit --only`
    卻區分大小寫比對 index,寫入後才失敗(#140)。只改大小寫的改名(`Foo.txt` → `foo.txt`)也算。
    拒絕訊息提示改用檔案模式貼上(寫進磁碟上既有的拼法,不建立 commit),或先把名稱改成一致。
    index 拼法要完整讀到才能排除別名:讀取失敗、逾時、取消或超過輸出上限時,預覽與 Apply 都回報錯誤,
    不當成空 index 繼續。
    分大小寫的檔案系統(Linux)兩種拼法照常各自寫入。已知限制:較早的 commit 刪除的若是只在 index
    (未進 HEAD)的檔案,預覽當它已離開 index,Apply 時它仍在 index,所以較晚的 commit 寫入它的
    大小寫別名時,預覽說會寫、Apply 拒絕;這個方向不會留下半套狀態。

### 4.4 剪貼簿格式

snip-sync 自訂,只要求 snip ↔ snip 互通:
- 第一行是固定 marker(例如 `// snip-sync commits v1`),用來和檔案模式區分。
- 其後是 JSON,內容即 4.2 的欄位。marker 帶版本號,格式日後可擴充。

## 5. 介面

### 5.1 桌面 App

- **常駐系統匣**(尚未實作)。選單:從剪貼簿貼上、複製上一次的選取、開啟主視窗、結束。
- **主視窗**:只有一個。原生標題列下方一直顯示 App 自己畫的工作區分頁列(用語見 `GLOSSARY.md`,理由見 `docs/adr/0001-in-app-workspace-tabs.md`):
  - 一個工作區分頁恰好顯示一個工作區(本機資料夾,或 SSH 主機上的資料夾,見第 8 節)。分頁列最右邊是「+」,開一個空分頁並打開工作區選單。
  - 開啟工作區一律開新分頁;那個工作區已經開著就切到它的分頁。本機以 canonical path 判斷(經 symlink 拼寫的路徑算同一個),遠端以 SSH Host 別名加 worker 解析後的路徑判斷(指向同一台機器的兩個別名算兩個工作區)。巢狀資料夾各自是一個工作區。從空分頁開啟時就開在那個分頁;空分頁要開的工作區已在別的分頁開著時,切過去並關掉這個空分頁。
  - 分頁標籤:本機顯示資料夾名,遠端顯示「主機 ▸ 名稱」;標籤相同時加上父資料夾,直到不同為止;過長時以「…」截斷。遠端分頁顯示連線狀態圖示(連線中、已連線、失敗),正在寫入貼上的分頁顯示貼上圖示;滑鼠停在標籤上顯示完整路徑。視窗標題是「目前分頁的標籤 — snip-sync」,沒有工作區時是「snip-sync」。分頁多到放不下時分頁列可以橫向捲動,「+」一直看得到。
  - 快捷鍵:Cmd/Ctrl+W 與 Cmd/Ctrl+Shift+W 關閉目前的分頁;Cmd/Ctrl+Shift+[ ] 切到上一個／下一個分頁(頭尾相接);Cmd/Ctrl+1–9 直接跳到第 1–9 個分頁,沒有那個分頁時不動;Ctrl+Tab 維持在控制項之間移動焦點。分頁上的「×」關閉該分頁。
  - 背景分頁的狀態全部保留(選取、捲動位置、面板寬度、貼上預覽),切回來不重新整理;切回時鍵盤回到那個分頁上次有焦點的面板。面板寬度每個分頁各一份,不存檔。分頁數量不設上限,每個遠端分頁保有自己的 Client 與連線；握手排程與閒置連線上限見第 8 節。
  - 關閉分頁時先收尾那個分頁自己的背景工作(只等自己的工作,不等別的分頁),再移除分頁;正在連線的遠端開啟也算,關閉或結束會取消它(中止還在連線的 ssh),不等連線逾時;那個分頁正在寫入貼上時拒絕關閉。關掉最後一個分頁時畫面只剩「+」,App 不結束。
  - 只有 Cmd/Ctrl+Q 或關閉視窗才結束 App:任何分頁正在寫入貼上就拒絕結束並切到那個分頁;否則等所有分頁都收尾完才結束。某個分頁收尾逾時就取消結束,切到那個分頁,其他分頁重新載入收尾時取消的內容。
  - **還原分頁**:設定資料夾的 `open-tabs.json` 記著開著的分頁,格式 `{"tabs":[{"kind":"local","path":<canonical path>}|{"kind":"remote","host":<ssh Host 別名>,"path":<worker 解析後的路徑>}],"active":<索引或 null>}`。分頁開啟、關閉、切換時才寫入(內容沒變就不寫);沒有分頁時不存,顯示中的分頁是空的就把 `active` 記成 null;結束 App 不重寫它(留的是收尾前的分頁)。
    - 一般啟動依序重開存下的分頁,再處理 `--workspace X`:X 已在某個還原的分頁開著就切過去(以 canonical path 判斷,symlink 拼寫算同一個),否則開在新分頁,並顯示它。沒帶 `--workspace` 就顯示存下的作用中分頁。存的是空清單就開一個空分頁。從來沒存過 `open-tabs.json` 時沿用舊的順序:`--workspace`,否則最近開過的本機資料夾,否則啟動時的資料夾(不是 `/` 或家目錄),否則一個空分頁。
    - `--restore-dir` 只是這次啟動自己那個分頁(`--workspace` 的分頁,否則顯示中的分頁)的貼上目的地;之後才開的分頁貼進各自的 repo。`--mode idle/overview/preview` 只開一個分頁,不還原也不存檔。
    - 存下的本機資料夾已不存在就略過;顯示中分頁的狀態列與一張失敗通知(12 秒)顯示「已略過 N 個找不到的工作區」(英文 "Skipped N workspaces that no longer exist")。
    - 遠端分頁立刻以「主機 ▸ 名稱」顯示並存檔,在背景連線,狀態列顯示「正在重新連線 …」。連不上時分頁留著,狀態是失敗(警告圖示),分頁提示文字最後加「連線失敗:<原因>」,狀態列顯示「無法開啟遠端工作區:<原因>」;暫時性失敗會在背景重試,間隔 5、10、20、40 秒,之後每 60 秒一次:連線逾時或中斷、ssh 啟動失敗(含 ssh 訊息無法歸類的,例如網路尚未就緒)、遠端資料夾暫時不存在(NotFound)、worker 忙碌或逾時。已知永久拒絕停止自動重試,保留錯誤供使用者處理:權限拒絕(Forbidden)、缺少 worker、協定錯誤、版本過舊,以及其他拒絕碼。重試成功就在同一個分頁連上。按 Refresh 會立即重試失敗的還原分頁,不必等退避計時器。
    - 還原先依保存順序建立分頁，再優先啟動保存的作用中遠端分頁；同主機的背景分頁等它第一個 OpenWorkspace probe 完成、失敗或取消後才開始，最多等 30 秒，其他主機不等待。這是有界的啟動優先，不保證任意網路延遲下的回應順序；背景連線完成不能改變作用中 identity。成功開啟工作區的 Client 交給該分頁繼續使用，初次掃描不另建一個 Client。
    - e2e 日誌:還原時印 `[APP:REMOTE_REOPEN: host=.. path=..]`,每次 open 重試印 `[APP:REMOTE_RETRY: attempt=N]`；scan 失敗仍印 `REMOTE_SCAN_FAILED`，暫時失敗排程時印 `[APP:REMOTE_SCAN_RETRY: attempt=N delay_ms=..]`，恢復時印 `[APP:REMOTE_SCAN_RECOVERED: retries=N repos=N]`;啟動時若讀到存下的清單,印一次 `[APP:WS_TABS_RESTORED: count=<還原後的分頁數> skipped=<N>]`。每個還原的分頁各印一行 `[APP:WS_TAB_OPENED: id=.. count=..]`,id 依存下的順序指派,`--workspace` 的分頁(若是新開的)排最後。測試用的探針:`ws-tab-state:<索引>:failed`。
  - 選 repo / 資料夾。
  - 檔案模式:專案工具視窗照實顯示工作區資料夾(含非 Git 檔案,儲存庫在其所在位置標出分支與變更數),逐層載入目錄,單擊任一列即單獨選取(檔案在右側預覽,資料夾同時展開/收合;箭頭只展開不選取),選資料夾只選它本身,複製時才走訪;以 Ctrl/Cmd 多選、Shift 範圍選取,右鍵複製所選檔案與資料夾。選來源(檔案、working tree、staged、commit、區間);Git 來源(Changes)顯示真正的目錄階層、變更狀態與 diff;沒有勾選框也沒有選取籃,對任一節點右鍵「複製」就只複製那個節點:檔案列是該檔(帶它的來源 staged／unstaged／untracked),資料夾列是該 repo、該群組底下的檔案,repo 列是該 repo 在該群組的檔案,群組列是所有 repo 在該群組的檔案;名稱不是有效 UTF-8 的列不納入,沒有可複製內容時「複製」停用。commit 樹瀏覽的檔案列右鍵「複製」該檔在那個 commit 的內容。Cmd/Ctrl+C 複製左側工具視窗游標所在的節點(專案工具視窗是選取的列;閱讀器有選取文字時複製文字;Git Log 有焦點時複製選取的 commit)。選 monorepo 子資料夾時只列出並複製該資料夾內的變更。一般文字預覽上限 1 MiB,二進位不顯示文字內容。
  - commit 模式:歷史時間軸,選一段連續 commit → 複製(亦可於 Git Log 有焦點時按 Cmd/Ctrl+C 複製)。
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
snip paste --apply [--overwrite | --skip-existing] [--adjust-paths]
```

- 貼上 commit payload 時,`--skip-existing` 與 `--adjust-paths` 不支援,指定時以 exit 2 退出(見 4.3)。
- 貼上 commit payload 時,若目標檔案已存在需明確指定 `--overwrite`,未指定時以 exit 2 退出;`--dry-run` 且有目標檔案存在時於 stderr 輸出警告提示,exit 0(見 4.3)。

CLI 與 App 共用同一組核心函式,各自只多一層 UI 用的前端:
- 複製檔案／資料夾:兩邊都用 `transfer::expand_folder_items` 展開資料夾(過濾 `.git`、巢狀 repo、特殊檔、指出 root 的 symlink),再由 `transfer::plan_export_with` 產出 payload。CLI 另以 `selection_from_paths` 把命令列路徑轉成選取項目,並以 `plan_export_expanding` 分批展開(結果與一次展開相同,只是不必走完整棵樹);GUI 的選取來自檔案樹。
- 複製 Git 變更:兩邊都以 `SourceKind::{Working,Unstaged,Staged,Commit,Range}` 交給 `plan_export_with`。CLI 以 `transfer::changed_items` 列出變更,GUI 的項目來自 Changes 面板。
- 複製 commits:兩邊都經 `commits::copy_commits_with`。CLI 以範圍或 `-n` 選取(`transfer::plan_commit_export_with`),GUI 以時間軸選取的精確 chain(`plan_commit_export_exact_with`)。
- 貼上檔案:兩邊都經 `snip_remote::PasteTarget`(`plan_files` / `apply_files`);本機目標以 `transfer::plan_import_with` 規劃(含碰撞與新鮮度檢查；worker 以 `plan_import_parsed_with` 重用已解析的 entries，仍走同一引擎),再以 `TransferImportPlan::apply` 套用,底層執行器是 `restore::execute_restore_plan`。路由(目的地與各 prefix 資料夾、`--adjust-paths`)只由 `pastetarget::import_mapping` 建成 `ImportMapping`,本機目標與 worker 共用。
- 貼上 commits:兩邊都經 `PasteTarget`(`plan_commits` / `check_commits` / `apply_commits`);本機目標用 `transfer::CommitReplayPreview`(`capture_in` / `revalidate` / `apply`)。
- 遠端連線:兩邊都經 `snip_remote::Client`(`RemoteHost::ssh`,主機來自 `ssh::config_hosts`)。
- 遠端 Git 檢視：兩邊都經 `snip_core::gitview::RepoView`（`snip_remote::RemoteRepo`）；worker 以 served `LocalRepo` 回答。
- 貼上目標:兩邊都經 `snip_remote::PasteTarget`(`Local` 在本機跑上面的函式,`Remote` 經 `Client` 交給 worker);錯誤統一為 `PasteError`(`reason()`、`is_stale`、`is_repo_busy`、`is_collision`,遠端 Apply 失聯為 `OutcomeUnknown`)。新鮮度各自檢查:本機 Apply 套用保留的計畫並在寫入鎖內重驗;worker 可短期保留原始檔案 payload，但 Apply 仍重新規劃並比對 digest。覆寫門禁的判定在核心:`FilePlan::overwrites_existing`、`CommitReplayPlan::existing_writes`、`RestorePlan::existing_targets`。
- 遠端複製:兩邊都經 `snip_remote::Client`(`export_files`／`export_commits`);遠端貼上經 `PasteTarget::Remote`,它依協商版本送出檔案預覽／套用或 `replay_plan`／`replay_apply`,worker 跑上面同一組函式(§8「貼上」)。
- 大小上限:兩邊的複製都以 `transfer::CLIPBOARD_PAYLOAD_MAX`(32 MiB)為上限,GUI 貼上預覽也用同一個值。

兩邊行為的差異只在 UI(CLI 是旗標與文字輸出,App 是右鍵複製、貼上預覽的逐檔勾選與時間軸),以及下列例外:
1. 路徑重定位:CLI 偵測單一 restore-base 建議並以 `--adjust-paths` 套用全部檔案(經 `ImportMapping::from_restore_base`);GUI 維持逐 prefix 選擇(D4)。兩者最後都是同一個 `ImportMapping`,都由 `pastetarget::import_mapping` 建成。
2. 沒有選到任何檔案:CLI 顯示 `No files selected.` 並以 exit 1 結束、不碰剪貼簿;GUI 顯示提示、同樣不寫剪貼簿。

舊的整體引擎 `copy::collect_copy_files` 與 `gitsrc::collect_payload` 已降級為測試 oracle,產品程式碼不再呼叫。`restore::plan_restore` 仍是 `plan_import_with` 內部每筆 entry 的規劃器(contract fixture 測的就是它),`restore::execute_restore_plan` 仍是檔案貼上的執行器。

## 6. 技術決策(摘要)

- 桌面 App 自 v0.3.0 起是 GPUI 原生版(`crates/desktop-native`)。原本的 Rust + Tauri 2 + React 版已從 repo 移除,
  下面提到前端元件與 WebView 的條目是當時的紀錄。
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

## 8. 遠端工作區(SSH)

> 狀態:第三刀(2026-10-04)改走 SSH,拿掉配對;支援瀏覽、預覽與唯讀 Git 檢視。之後加入複製(協定 3)與貼上(協定 4),和本機資料夾一樣用。

這是**操作另一台電腦上檔案的控制通道**,不是剪貼簿的傳輸方式:第 1 節「不管傳輸」指的是複製／貼上之間的剪貼簿,照舊不變。

- **角色**:
  - **master**:桌面 App,或 CLI `snip remote hosts|ls|stat|cat|repos|changes|log|show|diff|copy|copy-commits|paste <host> <資料夾> …`。兩者共用 `snip_remote::Client`。
  - **worker**:被操作的那台,只要裝了 `snip`。master 每條連線都透過 ssh 在那台啟動一個 `snip serve --stdio`,連線結束就退出;不需要常駐服務,也不需要桌面 App。
- **主機與連線**:
  - 主機清單就是 `~/.ssh/config` 的 `Host` 項目(跟著 `Include` 走,略過萬用字元樣式)。
  - master 執行 `ssh -T -o BatchMode=yes <host> snip serve --stdio`;遠端非互動 shell 的 PATH 找不到 `snip` 時,依序試 `~/.local/bin`、`/opt/homebrew/bin`、`/home/linuxbrew/.linuxbrew/bin`、`/usr/local/bin`。
  - 身分驗證與加密完全交給 SSH,所以必須先設好金鑰登入;BatchMode 不會問密碼,失敗時顯示 ssh 自己的錯誤訊息（`Permission denied` 時另提示先 `ssh-copy-id`）。BatchMode 也不會詢問未知的 host key:第一次連的主機要先在終端機 `ssh <host>` 接受一次,否則顯示 ssh 的 `Host key verification failed`。遠端沒有 `snip` 或版本太舊時,顯示「那台機器沒有安裝 snip,或版本太舊」。
  - worker 先印一行 `snip-serve-stdio/1`,之後才是協定的 frame;登入 shell 在這之前印的歡迎訊息會被略過。
  - `SNIP_REMOTE_EXEC` 環境變數可以整個取代啟動指令(以空白分隔),供測試與驗收使用。
  - `SNIP_E2E_PASTE_HOLD` 是貼上中斷驗收用的暫停點：只有設成**絕對路徑**且該檔案存在時，worker 才在第一個檔案寫入後暫停，等檔案移除再繼續（最多 60 秒）；未設定、相對路徑或檔案不存在都不暫停。
- **權限**:SSH 登入的使用者讀得到的資料夾,都能開成遠端工作區。工作區開啟後,每個請求仍以 realpath 解析,必須留在該工作區之內,所以 `..`、絕對路徑、指向工作區外的 symlink 一律拒絕。
- **開啟遠端工作區**:工作區選單的「遠端主機（SSH）」列出主機。點一台主機會先連線並列出它家目錄底下的資料夾(不列 `.` 開頭的)。點資料夾是進入它、列出它的子資料夾,不會開啟;上方顯示目前的資料夾和「開啟這個資料夾」按鈕,另有「上一層」(在根目錄時不顯示)。晚到的列表若不是目前顯示的資料夾就丟掉。路徑欄跟著填入目前的資料夾,也可輸入任何資料夾(絕對路徑或 `~/…`)再按「開啟」。路徑欄緊接在目前資料夾下方;資料夾很多時清單在自己的區塊內捲動,整個工作區選單也不超出視窗高度,所以路徑欄與兩個開啟按鈕一直點得到。
  - 工作區選單的主機瀏覽每個分頁只用一條連線,選單關閉或離開該分頁時結束(連線與 ssh 一併關掉);從選單開啟正在瀏覽的資料夾時沿用那條連線,不再另開 ssh。主機瀏覽只要求 `ListDir` 的 `folders_only`(協定加法欄位,無版本門檻:舊 worker 忽略它並送回全部項目,master 仍自行過濾)。路徑欄永遠顯示目前瀏覽主機的資料夾,展開的主機不顯示別的主機的輸入欄;一選定主機或資料夾,路徑欄立即改成它,列表回來時只有使用者還沒改過才覆寫。
- **遠端的最近開啟與重新連線**:開過的資料夾記在設定資料夾的 `remote-recent.json`,所有主機合計最多 10 筆,列在工作區選單上方的「最近開啟」、排在本機的之後,標成「主機 ▸ 資料夾名」,點一下就重開。啟動時的重新連線由 `open-tabs.json` 負責(見 5.1「還原分頁」):遠端分頁記成主機別名加 worker 路徑,啟動後在背景重連；暫時失敗保留分頁並 backoff 重試，永久拒絕則保留錯誤。它取代了舊的 `remote-last.json`,後者不再寫入;啟動時找不到 `open-tabs.json` 但有 `remote-last.json`,就轉成一個遠端分頁、存成 `open-tabs.json` 並刪掉 `remote-last.json`。
- **master 開啟遠端工作區後**:
  - 專案樹逐層列出 worker 上的目錄,和本機一樣不列 `.git`(指名路徑仍可讀)。`ListDir` 一次回整份排序好的清單(資料夾在前),上限 20,000 筆,超過時回 `truncated` 並顯示與本機相同的截斷標記,沒有游標續讀;「顯示更多」把清單餵進本機樹同一個 held 續讀路徑。`snip remote ls` 一次印出整份,超過上限時附截斷提示。
  - 指向工作區內資料夾的 symlink 列成資料夾,可以展開;指向工作區外或 `.git` 的 symlink 列成一般項目,打開時拒絕。規則與複製時展開資料夾相同(`transfer::is_safe_dir_symlink`)。
  - 點檔案就預覽,規則與本機相同:上限 1 MiB,二進位與非 UTF-8 不顯示文字。重新整理會重讀樹和開著的預覽,檔案在 worker 上已刪除就顯示錯誤。
  - 複製與貼上和本機一樣(見下方「貼上」)。加入儲存庫路徑（`add_repo_path`）會拒絕,狀態列顯示「遠端工作區不支援這個操作」（`remote_unsupported`）。右鍵選單的儲存庫與檔案列只提供複製 worker 上的路徑（`copy-worker-path`）,不提供本機 reveal（在 Finder／檔案總管顯示）。
- **貼上**(協定 4):
  - 檔案模式與 commit 模式都和本機同一個預覽面板與流程:同樣的列、勾選、覆寫預設關閉、前綴對應選擇(候選是遠端工作區的 repo 與資料夾,顯示成 `主機:路徑`)、新鮮度與碰撞檢查,結果也相同。目的地是目前的 repo,沒有 repo 時是工作區資料夾。
  - worker 在自己的磁碟上跑本機貼上同一套引擎(`transfer::plan_import_with`、`CommitReplayPreview`),不是逐檔寫入的 RPC。`ImportPlan`／`ReplayPlan` 只規劃、不寫入,回傳計畫與新鮮度快照(路徑是 worker 的)。
  - Apply 不信任保留的舊計畫:`ImportApply` 先用預覽時的快照重新驗證(和本機 Apply 一樣的「已在外部建立／修改／刪除」),再重新規劃並比對計畫摘要,有任何變動就以 `Stale` 拒絕、什麼都不寫,然後依使用者的勾選寫入。`ReplayApply` 把預覽和 payload 接回去,取得寫入鎖(見 3.2 (c),被占用時以 `Busy` 拒絕)後重新驗證再重放;`ImportApply` 同樣在寫入鎖底下重新驗證;「先允許覆寫」的提示之前,同樣先檢查是否過期。
  - 協定 7 的檔案預覽以 `PrepareImport` 上傳 payload，回覆路徑、動作、略過原因、新鮮度、摘要與來源 entry 序號，不回傳檔案全文。master 從同一份 payload 還原內容，核對完整計畫 digest；目的端 diff 仍按需求讀取。
  - 協定 8 的 commit 重放同樣只上傳一次 payload：`PrepareReplay` 上傳 payload，worker 把自己產生的 `CommitReplayPreview` 以單次使用的 token 保留在同一條連線租約中（與檔案貼上共用每個 Client 最多 2 份的上限）；回覆仍帶計畫與新鮮度供顯示。`ApplyPreparedReplay` 只送 token、digest 與 check_only；覆寫閘門與 CLI 的檢查同樣經由 token，檢查通過後 token 保留給之後的套用。寫入前的 `Busy`／`Timeout` 保留 token。`TOKEN_GONE` 時在同一條連線上只送一次 `ReplayApply`（全文），由 worker 重新規劃並比對。協定 4–7 的 worker 沿用 `ReplayPlan`／`ReplayApply`。
  - worker 可保留原始 payload、mapping、canonical workspace／目的地與 digest，發出單次使用的 token；不快取可直接執行的計畫。每個 worker 的保留上限為 128 MiB、8 筆、5 分鐘；記憶體計入字串／路由配置容量與正在套用的 payload。容量不足或 master 未要求保留時，仍回傳精簡預覽但不給 token，套用沿用 `ImportApply` 上傳全文。
  - 有 token 的 `FilePastePlan` 固定使用產生它的連線，每個 Client 最多保留 2 份連線租約。`ApplyPrepared` 只送 token、scope、選取與預覽快照；worker 取出原始 payload 後，仍走 `ImportApply` 的快照檢查、重新規劃、digest／選取驗證與寫入鎖內 freshness 重驗。worker 在寫入前的拒絕不會用掉 token：寫入鎖被占用（`Busy`）或期限逾時（`Timeout`）只來自取得寫入鎖、新鮮度重驗與重新規劃，`apply_observed` 在第一筆寫入前就回傳，所以 worker 把原始 payload 以同一個 token 放回（仍算原本的 TTL），client 把租約還原為可再套用，使用者按 Apply 即可用同一份預覽重試，不需要重新預覽。`ApplyPrepared` 拿不到 token（過期、被逐出、worker 重啟，或已用掉）時，worker 沒有寫入，回覆 `Stale` 與固定文字 `TOKEN_GONE`；client 在同一條連線上只送一次 `ImportApply`（全文），由 worker 重新規劃並比對 digest 與新鮮度。目的地沒變就照常寫入；目的地已被外部改動則以真正的原因（已在外部建立／修改／刪除）拒絕，不顯示成 token 過期。其餘 `Stale`（digest 或新鮮度）、`Forbidden`、`Io`、`OutcomeUnknown` 都不會改走全文，`Stale` 依舊要求重新預覽。Apply 可能已送出後的失聯維持 `OutcomeUnknown`，不重連重送或改送全文。原連線失效時不會改用另一個 worker 的連線。
  - CLI `snip remote paste <host> <資料夾> [--in 資料夾] --dry-run|--apply [--overwrite|--skip-existing] [--adjust-paths] [--stdin]` 的旗標、輸出與結束碼和 `snip paste` 相同;路徑調整建議由 worker 依它的資料夾提出。
  - payload 可達剪貼簿上限(32 MiB),大於一個 frame:請求的貼上文字以 `Chunk` frame 先送(worker 端上限為剪貼簿上限),回覆的計畫以 `Chunk` + `Joined` 分段;整個請求(含 Apply 的新鮮度快照)超過一個 chunk 時,以 `FrameChunk` + `FrameJoin` 有上限地分段(協定 5)。寫入中的 Apply 不能中途取消,所以期限較長(30 分鐘),master 也不會重送寫入請求;超過 worker 期限時,寫入工作自己的回應(成功或它自己的錯誤)不會被丟掉換成 `Timeout`,worker 以 heartbeat 撐住並送出實際結果,master 的期限(31 分鐘)較長,若 master 先放棄則結果未知:App 顯示「連線中斷，無法確認貼上是否完成；請重新整理確認」(`paste_outcome_unknown`),CLI 在 stderr 說明並以非零結束。

### Git 檢視

- **儲存庫掃描與路由**:
  - master 開啟遠端工作區後在背景發送掃描請求（`ScanRepos`），掃描工作區資料夾內的 repo。掃描規則同本機探索：搜尋深度 8、最多 256 個 repo、單一 75 秒期限（`SCAN_DEADLINE`）。
  - 若逾時或達上限，狀態列與提示顯示「找到 N 個儲存庫，遠端掃描未完成；重新整理可重掃」（`remote_scan_incomplete`）以及未到達的資料夾清單；狀態 `More` 亦對映至未完成。遠端續掃僅支援 depth-limited 資料夾，逾時或達到數量上限時不支援游標續掃，需以重新整理（Refresh）重新掃描。
  - 每個 repo 的 Changes、diff、歷史（graph、commit 檔案與 diff、commit 樹）、分支／ref 篩選皆透過型別化 RPC（`GitQuery`）在 worker 端執行 git，不跨網路傳遞命令列字串。
  - 遠端 repo 的樹 IO 與預覽路徑自動帶上相對於工作區根目錄的路徑前綴。工作區本身就是 repo 時直接復用單一樹，不建立多餘的虛擬工作區樹。
- **三種情況的畫面**:
  - 多 repo：列出所有掃描到的儲存庫。Changes 面板依 repo 分組顯示變更；Log 面板顯示跨 repo 合併歷史，可透過 Repository 下拉選單篩選單一 repo。
  - 單 repo（工作區資料夾本身即 repo）：直接開啟為該 repo，專案樹即 repo 樹。
  - 非 repo（工作區資料夾內無任何 `.git`）：專案樹維持一般檔案瀏覽；Changes 面板顯示「這個資料夾裡沒有 Git 儲存庫」（`changes_no_repository`），Log 面板顯示「這個資料夾裡沒有 Git 儲存庫」（`log_no_repository`），絕不顯示為乾淨或空 log。
- **「沒讀到不顯示成乾淨」規則**:
  - Changes 空狀態（`ChangesEmpty`）：依序判定「掃描中（Scanning）→ 讀取中（Loading）→ 讀取失敗（ScanFailed）→ 沒有 repo（NoRepository）→ 無符合（NoMatch）→ 乾淨（Clean）」。任一 repo 讀取失敗時顯示 Note 錯誤列，未完成讀取前絕不顯示為乾淨（`clean_working_copy`）。多 repo 工作區中沒有變更列的讀取失敗 repo 達 2 個以上時，錯誤列收進清單末端一個「無法讀取的儲存庫 (N)」節點，預設收合（`change-unreadable`）；只有 1 個時錯誤列留在最上方。
  - Log 空狀態（`LogEmpty`）：依序判定「掃描中（Scanning）→ 讀取中（Loading）→ 沒有 repo（NoRepository）→ 失敗（Failed）→ 空（Empty）」。多 repo 合併 log 中若有部分 repo 讀取失敗，log 上方顯示「N 個儲存庫無法讀取：a, b, …」提示（`log_failed_feeds`），只列前 2 個名稱，完整清單在提示的 tooltip。
- **和本機一樣開 repo**:
  - worker 以 `LocalRepo::open_served` 開 repo:探索與身分判定和本機工作區相同(`identify_repos` 不帶 boundary),所以主 repo 在工作區外的 linked worktree、獨立 git dir、借用外部物件庫(alternates／`--shared`)都照常顯示,和本機一致。
  - 探索到的 `.git` 必須解析回它自己所在的資料夾:空的 `.git` 讓 git 往上找到外層 repo 時,這個資料夾不算 repo、直接略過(和 git 一致),也不把外層 repo 當成它的。本機與遠端都套用。
  - 不改 worker 端的 index:served `git diff` 需要讀 index 時,以 `GIT_INDEX_FILE` 指向私有暫存副本,不寫入真正的 `.git/index`、不搶 `index.lock`,因為那台機器的使用者可能同時在用 git。
  - 執行環境強化照舊:固定 argv 前綴 `-c core.fsmonitor= -c protocol.allow=never -c core.hooksPath=/dev/null`(Windows 為 `NUL`),注入 `GIT_OPTIONAL_LOCKS=0`、`GIT_NO_LAZY_FETCH=1`,清除 `GIT_DIR`、`GIT_WORK_TREE` 等變數。
- **資源規則**:
  - 獨立名額池：遠端讀取使用 `gitrun` 獨立的 `GitPool::Served`（並行 1、佇列 4），不計入 worker 主人的 `Local` 名額池，遠端 master 的請求絕不排擠 worker 主人自己的 UI 操作，也不會卡住桌面版的工作區切換或結束（drain）。master 端亦對遠端 Git 呼叫限制最多 4 個並行請求。
  - job 管理：每個請求為一個 job，同時最多 2 個 job（其中掃描最多 1 個），等待佇列上限 16，超過或等候逾時（10 秒）立即回報 `Busy`。整體期限 View 60 秒、Scan 75 秒，逾時取消 job 並回報 `Timeout`。協定 ≥2 時，**每個請求**（含 `OpenWorkspace`／`ListDir`／`Stat`／`Read` 這類 worker 自己回答的請求）在 worker 計算期間每秒發送一個 `Pending` heartbeat 幀；自己回答的請求也有 View 的期限（60 秒），不經 job 名額（不會收到 `Busy`）。協定 1 的 master 不認得 `Pending`，只收到回覆。master 斷線或取消請求時立即中止 worker 背景 job。
  - master 的沉默上限：回覆的第一個位元組之前 5 秒（讓休眠或 NAT 逾時而斷掉的池中連線快速失敗），回覆開始之後（frame 中途、同一個回覆的兩個 frame 之間、heartbeat 之後）30 秒；整個呼叫另受絕對期限限制，收尾靠取消而不是靠上限。逾時訊息帶出已收到的位元組數與經過時間，並分出「沉默」（no data for）與「期限」（time limit passed）兩種結束方式；`GitError` 保留這段文字，不再一律寫成 90 秒。
- **已知限制**:
  - tag 與參照極多（refs 輸出超過 `SERVED_MAX_STDOUT = 4 MiB`）的 repo，在遠端會顯示「遠端參照資料過大」（`remote_refs_too_large`）錯誤（本機可看）。
  - Refresh 時，開著的 Changes 列預覽若不屬於正在重新載入的 repo（或尚未讀完），預覽會清掉，回到該 repo 的預設畫面，不保留也不重讀；本機與遠端相同。專案樹開著的檔案照常重讀。
  - 初次或 Refresh 的唯讀掃描遇到暫時性斷線、timeout 或 busy 時，在同一分頁／session 以 1、2、4 秒 backoff 最多重試 3 次；作用中與背景分頁相同。權限、版本、路徑或協定等永久拒絕不自動重試。成功後恢復 repo 清單並清除掃描錯誤；耗盡次數保留可見錯誤供手動 Refresh。Refresh 取代舊 scan/timer，關分頁或退出會取消；取消退出後會恢復尚未完成的初次掃描，即使尚無 repo。
  - 每條連線是一個 ssh 程序；每個 Client 最多留 2 條閒置連線。同一 RemoteHost 連線設定的握手（connect＋Hello）跨 Client 最多同時 2 個，排隊有期限且可取消；不同主機互不阻塞。這是新握手上限，不是整個 App 的存活 SSH 連線上限。worker 的 job 與 `Served` 名額限制仍以單一程序計。
- **版本相容**:
  - 協定透過 `hello` 握手以 `max_version` 協商（目前最高版本 8:`GIT_VIEWS_VERSION = 2`、`TRANSFER_VERSION = 3` 複製、`PASTE_VERSION = 4` 貼上、`EXPORT_CHANGES_VERSION = 5` 由 worker 解析變更選取與整個資料夾匯出、`REQUEST_CHUNKS_VERSION = 5` 分段請求、`REPLAY_PREVIEW_VERSION = 6` commit 貼上目的端的有界文字／檔案種類讀取、`PREPARED_IMPORT_VERSION = 7` 精簡檔案預覽與單次 payload token、`PREPARED_REPLAY_VERSION = 8` commit 重放的單次 payload token）。worker 協商出的版本低於請求所需時,Changes 與 Log 明確顯示「{0} 上的 snip-sync 版本太舊（協定 {1}），不支援 Git 檢視；請在那台機器更新」（`remote_worker_too_old`）;複製與貼上回報「那台的 snip-sync 太舊」。舊 worker 缺少 commit 預覽基準資訊時，詳情明示無法比較，不能把每筆都與初始磁碟相比或冒充新增；檔案貼上對協定 4–6 沿用 `ImportPlan`／`ImportApply`，協定低於 4 明確拒絕；既有安全門禁維持相容。
  - 錯誤原因（`Reason`）是五個加法欄位（皆無版本門檻、寬鬆讀取，沒有原因或為空時不輸出該鍵，與舊 worker 的 frame 逐位元組相同）：`Response::Error.reason`、`ScannedRepo.reason`、`RepoScan.error_reasons`（與 `errors` 依索引對齊）、`ReplayFailure.reason`、`Response::Imported` 的 restore 失敗 `failures[].reason`（沒有 `ErrorCode` 的框，變體一律保留，路徑與 OS 文字清空；由 `codeless` 模組統一產生）；另有兩個加法的子欄位：`Reason::Internal.condition`（使用者造成的固定條件，舊 peer 不送也不讀，讀成沒有條件）與 `StartReason::NoCommand`／`CannotRun`（啟動失敗的細分；舊 peer 不認得的值讀成沒有啟動原因）；未知的變體讀成沒有原因，不使整個 frame 失敗。舊 worker 不送 `reason`，新 master 不再從英文訊息猜原因，所以回退為通用措辭：桌面 App 對 Stale、Collision、Busy 拒絕顯示該拒絕碼的通用字串，而非 worker 的英文原文；沒有原因的 replay 失敗顯示通用的遠端拒絕（`remote_refused_io`）。同理，對舊 worker 的掃描錯誤列與摘要失敗的 repo，因 `ScannedRepo.reason`／`RepoScan.error_reasons` 缺席，一律顯示通用的 `remote_refused_io` 措辭，而非 worker 的原文（例如 git status 失敗的說明）（已接受的相容成本）。CLI 對舊 worker 的 repo-busy 拒絕只印 worker 的訊息，少了「; try again when the other paste finishes」後綴（已接受的相容成本）；新 worker 搭配舊 master 則多送的欄位被忽略。`worker_closed`（worker 在回覆之間結束了串流）只在 master 端產生，不會上線。#225 之前的 worker 只送還原失敗的英文文字（例如 `sub/x: unsafe path`）；master 原樣接在通用標題之後顯示（`Datum::old_peer_text`），這是已接受的相容成本：路徑無法從英文安全切出來，拿掉就看不出是哪個檔案失敗。
  - 0.6.x 以前的 TLS worker(`snip worker`)不支援 `serve --stdio`,連不上時顯示「沒有安裝 snip,或版本太舊」。
- **寫入不變式**:
  - 能寫進 `.git` 就能寫 `.git/config`、hooks 或 filter,等於讓 master 在 worker 執行程式。這條守門在 snip-core 的貼上引擎裡(§3.2 安全規則),本機與遠端是同一段程式:payload 路徑含 `.git` 片段,或目標經 symlink 解析後落在 Git 目錄裡的,都是略過列、不寫入,其餘照寫。worker 另外只要求目的地與每個前綴對應的資料夾在工作區內(遠端的存取範圍);commit 重放和本機一樣進到目的地所在的 repo,即使它的頂層在工作區之上。
  - `Request::Write`/`Rename` 不提供(回 `Unsupported`):貼上整份在 worker 規劃與寫入,沒有逐檔寫入的 RPC。
- **明確不做(本切片)**:
  - 遠端寫入類 Git 操作（stage、unstage、commit、checkout、discard、rename、write）：(1) 能寫入 worker 檔案就能寫入 `.git/config`、hooks 或 filter，等於讓 master 在 worker 執行任意程式（違反寫入不變式）；(2) 本機寫入依賴 HeavyGuard、新鮮度與碰撞檢查（§3.2、§4.3），跨機器版本尚未設計；(3) 讀取先做正確，避免因誤判狀態做出錯誤決策。
  - 遠端的加入 repo 路徑（`add_repo_path`）：維持拒絕（回報 `remote_unsupported`）。複製（變更列、群組、資料夾、專案列選取、commit 檔案、commit）和本機一樣：worker 用同一個複製引擎產生 payload（協定 3 的 `Export` / `ExportCommits`），App 寫進剪貼簿。貼上見上方「貼上」。
  - 掃描逾時（`TimedOut`）或達上限（`LimitReached`）在遠端不支援游標續掃（僅 depth-limited 資料夾可續），需重新整理重掃。
  - 自動 fetch、遠端分支操作：本機亦無此功能。
  - Windows 作為 worker:遠端啟動指令用 POSIX `sh`。
