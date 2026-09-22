# 移植筆記:TypeScript → Go

目標是讓 Go 版與兩個 IDE 套件**行為完全一致**。以下規則都來自兩個套件實際踩過的問題,
完整脈絡見 [ClipCodeVSCode/AGENTS.md](https://github.com/audichuang/ClipCodeVSCode/blob/main/AGENTS.md)
與 [ClipCode/AGENTS.md](https://github.com/audichuang/ClipCode/blob/main/AGENTS.md)。
**這些規則要靠共用 fixture 驗證,不要靠讀規格。**

## 1. Go 特有的陷阱

Go 的標準函式庫在幾個地方跟 Java、JavaScript 都不一樣,正是兩個套件曾經分歧過的地方:

| 陷阱 | 為什麼會錯 | 正確做法 |
|---|---|---|
| `regexp` 是 RE2,`.` 預設只排除 `\n` | Java 的 `.` 排除五個換行類字元,JS 排除四個,Go 又是另一套 | 需要「任意字元」時寫 `[\s\S]`,不用 `.` |
| `strings.TrimSpace` 會處理 Unicode 空白 | 兩個套件曾因 trim 的字元集不同,寫出**不同檔名** | 自己寫 ASCII trim,只處理 `[ \t\n\x0B\f\r]` |
| `len(s)` 是 byte 數 | 通知的字元數必須是 **UTF-16 code unit**(emoji 算 2) | 用 `len(utf16.Encode([]rune(s)))` |
| `strings.EqualFold` 做 Unicode folding | 例如 Kelvin 符號 `K` 會對到 `k`;header 的 `file:` 曾因 case folding 在一邊是 header、另一邊是內容 | 逐字元只比 ASCII:`[Ff][Ii][Ll][Ee]:` |
| `filepath` 依 OS 而異 | 分隔符號、大小寫、磁碟機代號 | 回傳原生路徑;containment 用真實路徑比對,不用字串比對 |
| `exec.Command` 經過 shell 會被引號規則影響 | 兩個套件的測試曾在 Windows 上因 `cmd.exe` 不認 `'` 而全部失敗 | 一律 `exec.Command("git", args...)`,不經 shell |

## 2. 線上格式的不變量(摘要)

- 每個檔案一個 header,由 `headerFormat` 中的 `$FILE_PATH` 產生。**替換是字面替換**
  (`strings.ReplaceAll` 沒問題),絕不用 regex 替換。
- 變更標籤 `[NEW] [MODIFIED] [DELETED] [MOVED]` 加在路徑前面。
- 內容裡若有一行本身會被解析成 header,複製時加上 `//clipcode-esc: ` 前綴,貼上時移除。
- 選用的開頭行 `// clipcode-root: <name>`:只在單一 root 的情境下輸出,
  名稱必須是 payload 裡路徑所相對的那個基準目錄。
- 有設定 post text 時,最後一個檔案之後輸出 `// clipcode-end`,標記內容結束的位置。
  若所設定的 header 格式會把這一行解析成 header,就不輸出。
- 解析時只用 `\r?\n` 切行,**單獨的 `\r` 不是換行**。
- header 與標籤的 regex 使用 ASCII 空白類別,不用 Unicode `\s`。
- 統計:`chars` 是 UTF-16 單位;`lines` 是 `\n` 數加 1(空字串為 0);`words` 是 ASCII 空白分隔的片段數;
  `tokens` 是 `words` 加上 `;{}()[],` 的出現次數。**從整份 payload 算**,不是從各檔案內容相加。

## 3. 還原的安全規則(摘要)

- **路徑片段含控制字元(0x00–0x1F)或 `<>:"|?*` 時拒絕**,所有平台一致。
  U+0085 / U+2028 / U+2029 在 Windows 合法,允許。
- 對不到任何 root 的絕對路徑:**寫入**時照原樣放在主 root 底下(拿掉磁碟機冒號、保留每一層目錄),
  **刪除**時一律拒絕。絕不依路徑尾端去猜測目標。
- containment 以**真實解析**(realpath)判斷:解析路徑或其最深的已存在祖先目錄。
  祖先的往上走不設上限,只限制 symlink 的跳轉次數。
- placeholder(`// File skipped: …` / `// Unable to read file content` / `// Error reading file content`)
  **只看內容的第一行**判斷,永遠不寫到真實檔案上。
- 目標檔案不是 UTF-8 時不覆寫。**讀不到或大於 8 MiB 視為無法驗證,也不覆寫**(fail closed)。
- 所有寫入一律 UTF-8。
- containment 與編碼的判斷在**真正寫入前重新檢查一次**:使用者在確認畫面之間,檔案系統可能已經變了。

## 4. 複製的規則(摘要)

- 非 UTF-8 檔案**不複製**(嚴格解碼,失敗就跳過並計入通知),UTF-16 含 BOM 也一樣。
- 嚴格 UTF-8 解碼**保留**開頭的 BOM。
- merge commit 的檔案集是**與每一個 parent 的 diff 的聯集**(依路徑去重)。
- 刪除的檔案帶**刪除前的內容**;只有當沒有任何 parent 有這個檔案時,才輸出
  `// This file has been deleted in this change`。
- 讀不到的 placeholder 仍放進 payload,但**不算已複製**,也**不佔檔案數上限**。
- 目錄 symlink 只在它本身就是被選取的輸入時才跟進,遞迴過程中不跟進(避免 pnpm / Bazel 的交叉連結爆量)。

## 5. Git plumbing 對照

TS 版透過 VS Code git 擴充取得 git 資料,Go 版改為直接呼叫以下指令(都加 `-z`,路徑不會被引號包住):

| 需求 | 指令 |
|---|---|
| commit 的變更清單 | 對每個 parent:`git diff-tree -r -z --no-commit-id --name-status -M <parent> <sha>`,再取聯集;root commit 對空樹 `4b825dc642cb6eb9a060e54bf8d69288fbee4904` |
| commit 區間 | `git diff -z --name-status -M <a> <b>` |
| staged 清單 | `git diff --cached -z --name-status -M` |
| 讀內容(批次) | `git cat-file --batch`,輸入 `<rev>:<path>`;staged 用 `:<path>`(index stage 0) |
| 是否為 shallow clone | `git rev-parse --is-shallow-repository`(shallow 的邊界 commit 看起來沒有 parent,不能當成 root commit 處理) |

讀到的位元組一律先經嚴格 UTF-8 檢查(`utf8.Valid`),再轉成字串。

## 6. 剪貼簿

| 平台 | 做法 | 注意 |
|---|---|---|
| Windows | Win32 API,`CF_UNICODETEXT` | 不要透過 `clip.exe` 或 PowerShell,編碼容易出錯;需驗證換行是否被轉換 |
| macOS | `NSPasteboard`(或 `pbcopy`/`pbpaste`) | |
| Linux | `xclip` / `wl-clipboard` | CI 需要 xvfb |

候選套件:`golang.design/x/clipboard`(macOS / Linux 需要 cgo)、`github.com/atotto/clipboard`(Windows 為純 Go)。
Wails v3 本身也提供剪貼簿 API,Phase 0 時一併比較。
