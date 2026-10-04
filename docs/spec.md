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

- 預設仍是覆蓋已存在的檔案(使用者確認後),但新增兩道防護(transfer / `plan_import_with`,GUI 與 CLI 於階段 6 皆已採用):
  (a) `TransferError::TargetCollision`:計畫中兩筆 entry 指向同一個實體檔(大小寫差異片段若位於已存在路徑部分,如檔案或目錄已存在,會由 realpath 解析偵測;偵測目的端是否不分大小寫,不分大小寫則新目標也摺疊大小寫(D10,階段 5b 已實作);以及 symlink 別名、同一路徑出現兩次)就整批拒絕;錯誤訊息格式呈現為單一目標路徑並列出衝突的操作名稱（如 `target collision: multiple operations target '<p>': previous was 'create a', current is 'create b'`，不重複輸出路徑亦不暴露內部大小寫摺疊字串）;
  (b) freshness:預覽後目標檔或 repo 的 HEAD/index 有變,套用時拒絕(`TransferError::StaleDestination`),需重新預覽。
  這與 IDE 套件(TS)不同,見 porting-notes「已知且接受的差異」。
- 安全規則照 porting-notes 第 3 節:路徑片段等於 `.git`(ASCII 不區分大小寫,含 Win32 結尾點或空白拼寫如 `.git.`)視為 unsafe/unresolved 拒絕(檔案模式的寫入與刪除、commit 模式的 `path`/`old_path` 皆阻擋)、路徑含控制字元或 `<>:"|?*` 拒絕、containment 以 realpath 判斷、
  placeholder 永遠不寫到真實檔案、目標不是 UTF-8 不覆寫、所有寫入一律 UTF-8。
- CLI 補充(現行行為/CLI 於階段 6 已切換):絕對路徑在 sanitize 前先解析(root 內部、跨機器後綴 → 相對路徑,同 TS);絕對 [DELETED] 解析不到 root → 拒絕(視為 unsafe/unresolved 跳過,同 TS);寫入對不到 root 的 POSIX 絕對路徑(如 `/Users/bob/other/src/a.ts`)→ 去首斜線放主 root 下(同 TS);帶磁碟機代號的路徑(如 `D:\work\lib\b.ts`)因 `sanitize_relative_path` 的絕對路徑/磁碟機檢查(`is_absolute_path`/`has_drive_slash`)而跳過(`UNRESOLVED_PATH`),不再放進 `D/work/...`。CLI 只有單一 root。`paste` 的 `--repo` 必須已存在,否則 exit 1(見 porting-notes「已知且接受的差異」)。
- 目的端路徑為目錄、FIFO 等非一般檔案時整批以 `DestinationNotRegular` / `SpecialFile` 拒絕(含 `--dry-run`,exit 1;錯誤訊息明確提示貼上拒絕覆寫非一般檔案),見 porting-notes「已知且接受的差異」。父層片段為一般檔案或目的端為懸空 symlink 時亦以 `TransferError::Io` 整批拒絕(exit 1),同見該條目。

CLI:`snip paste --dry-run`(只列計畫)、`snip paste --apply [--overwrite | --skip-existing]`。

## 4. commit 模式

### 4.1 選擇 commit

- GUI:在**歷史時間軸**(commit graph)上選一段:點起點,Shift + 點終點（在兩端點間沿著 first-parent 鏈選取，自動略過分支上的 side commit；若無法構成 first-parent 鏈則退回可見列選取並在複製時拒絕；沿鏈只走目前顯示的 commit，若搜尋、ref 或路徑篩選隱藏了鏈中間的 commit（例如搜尋 `多行中文|C3 merge` 會隱藏 C2），選取就退回可見列範圍，複製時拒絕；清除篩選，或在隱藏的 commit 顯示時再選取，才能複製整條鏈）。
- 顯示本機所有分支、遠端追蹤分支、標籤與 HEAD 的拓撲圖;可依 ref 篩選、搜尋 message / SHA,每頁 300 筆並能繼續載入。瀏覽不切換分支、不自動 fetch。
- 點 commit 顯示它的檔案樹、內容與 diff;包含 root 的選取固定從所選 tip 回溯,也支援尚未合併的其他分支。
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
2. 確認後,**依原本順序**對每個 commit:
   1. 寫入新增 / 修改的檔案、刪除被刪除的檔案;rename = 刪舊路徑 + 寫新路徑。
   2. 只 stage 這個 commit 涉及的路徑。
   3. 建立 commit,**只提交這些路徑**(使用者原本 stage 的其他東西不會被帶進去),
      作者與作者時間用剪貼簿帶來的值,committer 是本機使用者。
3. 疊在**目前分支的 HEAD** 上,不需要與來源有共同的起點,也不檢查是否 fast-forward。
4. 結果:建立了幾個 commit。

- 此 commit 會寫入的目標檔在重播前已存在(`FilePlan.existed`),不論是否有未 commit 的修改,**預設不套用**,需明確允許覆寫(GUI:允許覆寫的開關,i18n `commit_overwrite_required`,paste.rs `execute_commit` 先 `preview.revalidate()` 再回此錯誤;CLI 判定與 GUI 相同[已決 D9],以 `FilePlan.existed` 判定,需 `--overwrite`,沒給則 exit 2)。覆寫門禁與 dry-run 提示皆以相異目標路徑（distinct paths）計算（多個 commit 變更同一既有路徑僅計為 1 個）。CLI commit 貼上在 `--dry-run`（未指定 `--apply` 與 `--overwrite`）且目標已有檔案存在時，會在 stderr 提示「N destination file(s) already exist; --apply will need --overwrite.」（退出碼維持 0）。CLI 貼 commit payload 時 `--skip-existing`、`--adjust-paths` 不支援,exit 2。這與 TS/原規格「直接覆蓋」不同,見 porting-notes「已知且接受的差異」。
- 中途某個 commit 建立失敗:**停下來**,回報已建立的前幾個、失敗的是哪一個與 git 的錯誤訊息。已建立的不回滾。
- 路徑安全規則與檔案模式相同。
- 預設值(規格階段未逐題確認,實作時照此,有意見再改):
  - 某個 commit 在本機寫入後沒有任何差異 → 仍然建立(空 commit),讓兩邊的 commit 數量與 message 一致。
  - 建立 commit 時**不執行** git hooks:重播的每個 git 呼叫都帶 `-c core.hooksPath=<空目錄>`
    (`--no-verify` 只跳過 pre-commit / commit-msg,擋不住 prepare-commit-msg、post-commit 等)。
    內容在來源端已經 commit 過,避免本機的 hook 修改、擋下或改寫重播的內容。
  - 重播前先檢查整個 commit 的目標:寫入或刪除的位置是目錄、父目錄被一般檔案佔住時,
    整個 commit 拒絕且不動任何檔案(預覽標為 `UNSAFE_PATH`)。來源 message 為空也照樣建立。

### 4.4 剪貼簿格式

snip-sync 自訂,只要求 snip ↔ snip 互通:
- 第一行是固定 marker(例如 `// snip-sync commits v1`),用來和檔案模式區分。
- 其後是 JSON,內容即 4.2 的欄位。marker 帶版本號,格式日後可擴充。

## 5. 介面

### 5.1 桌面 App

- **常駐系統匣**。選單:從剪貼簿貼上、複製上一次的選取、開啟主視窗、結束。
- **主視窗**(同一個視窗,可從系統匣的小尺寸展開):
  - 選 repo / 資料夾。
  - 檔案模式:專案工具視窗照實顯示工作區資料夾(含非 Git 檔案,儲存庫在其所在位置標出分支與變更數),逐層載入目錄,單擊任一列即單獨選取(檔案在右側預覽,資料夾同時展開/收合;箭頭只展開不選取),選資料夾只選它本身,複製時才走訪;以 Ctrl/Cmd 多選、Shift 範圍選取,右鍵複製所選檔案與資料夾。選來源(檔案、working tree、staged、commit、區間);Git 來源(Changes)顯示真正的目錄階層、變更狀態與 diff;沒有勾選框也沒有選取籃,對任一節點右鍵「複製」就只複製那個節點:檔案列是該檔(帶它的來源 staged／unstaged／untracked),資料夾列是該 repo、該群組底下的檔案,repo 列是該 repo 在該群組的檔案,群組列是所有 repo 在該群組的檔案;名稱不是有效 UTF-8 的列不納入,沒有可複製內容時「複製」停用。commit 樹瀏覽的檔案列右鍵「複製」該檔在那個 commit 的內容。Cmd/Ctrl+C 複製左側工具視窗游標所在的節點(專案工具視窗是選取的列;閱讀器有選取文字時複製文字)。選 monorepo 子資料夾時只列出並複製該資料夾內的變更。一般文字預覽上限 1 MiB,二進位不顯示文字內容。
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
snip paste --apply [--overwrite | --skip-existing] [--adjust-paths]
```

- 貼上 commit payload 時,`--skip-existing` 與 `--adjust-paths` 不支援,指定時以 exit 2 退出(見 4.3)。
- 貼上 commit payload 時,若目標檔案已存在需明確指定 `--overwrite`,未指定時以 exit 2 退出;`--dry-run` 且有目標檔案存在時於 stderr 輸出警告提示,exit 0(見 4.3)。

CLI 與 App 共用同一組核心函式,各自只多一層 UI 用的前端:
- 複製檔案／資料夾:兩邊都用 `transfer::expand_folder_items` 展開資料夾(過濾 `.git`、巢狀 repo、特殊檔、指出 root 的 symlink),再由 `transfer::plan_export_with` 產出 payload。CLI 另以 `selection_from_paths` 把命令列路徑轉成選取項目,並以 `plan_export_expanding` 分批展開(結果與一次展開相同,只是不必走完整棵樹);GUI 的選取來自檔案樹。
- 複製 Git 變更:兩邊都以 `SourceKind::{Working,Unstaged,Staged,Commit,Range}` 交給 `plan_export_with`。CLI 以 `transfer::changed_items` 列出變更,GUI 的項目來自 Changes 面板。
- 複製 commits:兩邊都經 `commits::copy_commits_with`。CLI 以範圍或 `-n` 選取(`transfer::plan_commit_export_with`),GUI 以時間軸選取的精確 chain(`plan_commit_export_exact_with`)。
- 貼上檔案:兩邊都以 `transfer::plan_import_with` 規劃(含碰撞與新鮮度檢查),再以 `TransferImportPlan::apply` 套用,底層執行器是 `restore::execute_restore_plan`。
- 貼上 commits:兩邊都經 `transfer::CommitReplayPreview`(`capture` / `revalidate` / `apply`)。
- 遠端連線:兩邊都經 `snip_remote::Client`(`RemoteHost::ssh`,主機來自 `ssh::config_hosts`)。
- 遠端 Git 檢視：兩邊都經 `snip_core::gitview::RepoView`（`snip_remote::RemoteRepo`）；worker 以 served `LocalRepo` 回答。
- 大小上限:兩邊的複製都以 `transfer::CLIPBOARD_PAYLOAD_MAX`(32 MiB)為上限,GUI 貼上預覽也用同一個值。

兩邊行為的差異只在 UI(CLI 是旗標與文字輸出,App 是右鍵複製、貼上預覽的逐檔勾選與時間軸),以及下列例外:
1. 路徑重定位:CLI 偵測單一 restore-base 建議並以 `--adjust-paths` 套用全部檔案(經 `ImportMapping::from_restore_base`);GUI 維持逐 prefix 選擇(D4)。兩者最後都是同一個 `ImportMapping`。
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

> 狀態:第三刀(2026-10-04)。改走 SSH,拿掉配對;支援瀏覽、預覽與唯讀 Git 檢視。複製與貼上在後續切片加入(見下方「明確不做」)。

這是**操作另一台電腦上檔案的控制通道**,不是剪貼簿的傳輸方式:第 1 節「不管傳輸」指的是複製／貼上之間的剪貼簿,照舊不變。

- **角色**:
  - **master**:桌面 App,或 CLI `snip remote hosts|ls|stat|cat|repos|changes|log|show|diff <host> <資料夾> …`。兩者共用 `snip_remote::Client`。
  - **worker**:被操作的那台,只要裝了 `snip`。master 每條連線都透過 ssh 在那台啟動一個 `snip serve --stdio`,連線結束就退出;不需要常駐服務,也不需要桌面 App。
- **主機與連線**:
  - 主機清單就是 `~/.ssh/config` 的 `Host` 項目(跟著 `Include` 走,略過萬用字元樣式)。
  - master 執行 `ssh -T -o BatchMode=yes <host> snip serve --stdio`;遠端非互動 shell 的 PATH 找不到 `snip` 時,依序試 `~/.local/bin`、`/opt/homebrew/bin`、`/home/linuxbrew/.linuxbrew/bin`、`/usr/local/bin`。
  - 身分驗證與加密完全交給 SSH,所以必須先設好金鑰登入;BatchMode 不會問密碼,失敗時顯示 ssh 自己的錯誤訊息。遠端沒有 `snip` 或版本太舊時,顯示「那台機器沒有安裝 snip,或版本太舊」。
  - worker 先印一行 `snip-serve-stdio/1`,之後才是協定的 frame;登入 shell 在這之前印的歡迎訊息會被略過。
  - `SNIP_REMOTE_EXEC` 環境變數可以整個取代啟動指令(以空白分隔),供測試與驗收使用。
- **權限**:SSH 登入的使用者讀得到的資料夾,都能開成遠端工作區。工作區開啟後,每個請求仍以 realpath 解析,必須留在該工作區之內,所以 `..`、絕對路徑、指向工作區外的 symlink 一律拒絕。
- **開啟遠端工作區**:工作區選單的「遠端主機（SSH）」列出主機。點一台主機會先連線並列出它家目錄底下的資料夾,也列出這台主機最近開過的資料夾,另有路徑欄可輸入任何資料夾(絕對路徑或 `~/…`)。開過的資料夾記在設定資料夾的 `remote-recent.json`,每台主機合計最多 10 筆。
- **master 開啟遠端工作區後**:
  - 專案樹逐層列出 worker 上的目錄,和本機一樣不列 `.git`(指名路徑仍可讀)。單一目錄最多列 1000 筆,超過就顯示截斷。
  - 指向工作區內資料夾的 symlink 列成資料夾,可以展開;指向工作區外或 `.git` 的 symlink 列成一般項目,打開時拒絕。規則與複製時展開資料夾相同(`transfer::is_safe_dir_symlink`)。
  - 點檔案就預覽,規則與本機相同:上限 1 MiB,二進位與非 UTF-8 不顯示文字。重新整理會重讀樹和開著的預覽,檔案在 worker 上已刪除就顯示錯誤。
  - 複製（右鍵「複製」與 Cmd/Ctrl+C）、貼上、加入儲存庫路徑（`add_repo_path`）、專案列的 Ctrl/Cmd 點擊選取在遠端工作區都會拒絕,狀態列顯示「遠端工作區只支援瀏覽、預覽與唯讀的 Git 檢視」（`remote_unsupported`）。右鍵選單的儲存庫與檔案列只提供複製 worker 上的路徑（`copy-worker-path`）,不提供本機 reveal（在 Finder／檔案總管顯示）。

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
  - job 管理：每個請求為一個 job，同時最多 2 個 job（其中掃描最多 1 個），等待佇列上限 16，超過或等候逾時（10 秒）立即回報 `Busy`。整體期限 View 60 秒、Scan 75 秒，逾時取消 job 並回報 `Timeout`。worker 每秒發送 `Pending` heartbeat 幀；master 斷線或取消請求時立即中止 worker 背景 job。
- **已知限制**:
  - tag 與參照極多（refs 輸出超過 `SERVED_MAX_STDOUT = 4 MiB`）的 repo，在遠端會顯示「遠端參照資料過大」（`remote_refs_too_large`）錯誤（本機可看）。
  - Refresh 時，開著的 Changes 列預覽若不屬於正在重新載入的 repo（或尚未讀完），預覽會清掉，回到該 repo 的預設畫面，不保留也不重讀；本機與遠端相同。專案樹開著的檔案照常重讀。
  - Refresh 時 worker 掃描失敗（舊版 worker、連線中斷），repo 清單清空，已開的預覽與展開的資料夾不保留；當時正在載入的資料夾可能停在載入中，需重新開啟工作區。
  - 每條連線是一個 ssh 程序;master 每台主機最多留 2 條閒置連線。worker 的 job 與 `Served` 名額限制以單一程序計。
- **版本相容**:
  - 協定透過 `hello` 握手以 `max_version` 協商（目前最高版本 2,`GIT_VIEWS_VERSION = 2`）。worker 協商出的版本低於請求所需時,Changes 與 Log 明確顯示「{0} 上的 snip-sync 版本太舊（協定 {1}），不支援 Git 檢視；請在那台機器更新」（`remote_worker_too_old`）。
  - 0.6.x 以前的 TLS worker(`snip worker`)不支援 `serve --stdio`,連不上時顯示「沒有安裝 snip,或版本太舊」。
- **寫入不變式**:
  - 將來若開放 `Request::Write`/`Rename`，必須拒絕任何 `.git` 目錄底下、以及 git dir／common dir 目標底下的路徑；否則 Git 檢視會把「可寫檔案」變成「在 worker 上執行程式」（hooks、filter、fsmonitor 設定）。
- **明確不做(本切片)**:
  - 遠端寫入類 Git 操作（stage、unstage、commit、checkout、discard、rename、write）：(1) 能寫入 worker 檔案就能寫入 `.git/config`、hooks 或 filter，等於讓 master 在 worker 執行任意程式（違反寫入不變式）；(2) 本機寫入依賴 HeavyGuard、新鮮度與碰撞檢查（§3.2、§4.3），跨機器版本尚未設計；(3) 讀取先做正確，避免因誤判狀態做出錯誤決策。
  - 遠端的加入 repo 路徑（`add_repo_path`）與貼上：維持拒絕（回報 `remote_unsupported`）。複製（變更列、群組、資料夾、專案列選取、commit 檔案、commit）和本機一樣：worker 用同一個複製引擎產生 payload（協定 3 的 `Export` / `ExportCommits`），App 寫進剪貼簿。
  - 掃描逾時（`TimedOut`）或達上限（`LimitReached`）在遠端不支援游標續掃（僅 depth-limited 資料夾可續），需重新整理重掃。
  - 自動 fetch、遠端分支操作：本機亦無此功能。
  - Windows 作為 worker:遠端啟動指令用 POSIX `sh`。
