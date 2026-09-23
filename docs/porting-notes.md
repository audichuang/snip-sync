# 移植筆記:TypeScript → Rust

目標是讓 Rust 版與兩個 IDE 套件**行為完全一致**。以下規則都來自兩個套件實際踩過的問題,
完整脈絡見 [ClipCodeVSCode/AGENTS.md](https://github.com/audichuang/ClipCodeVSCode/blob/main/AGENTS.md)
與 [ClipCode/AGENTS.md](https://github.com/audichuang/ClipCode/blob/main/AGENTS.md)。
**這些規則要靠共用 fixture 驗證,不要靠讀規格。**

## 1. Rust 特有的陷阱

Rust 的標準函式庫與 `regex` crate 在幾個地方跟 Java、JavaScript 都不一樣,正是兩個套件曾經分歧過的地方:

| 陷阱 | 為什麼會錯 | 正確做法 |
|---|---|---|
| `regex` crate 的 `.` 預設只排除 `\n` | Java 的 `.` 排除五個換行類字元,JS 排除四個(`\n` `\r` U+2028 U+2029),Rust 又是另一套 | 需要「任意字元」時寫 `[\s\S]`;移植 JS 的 `.` 時寫 `[^\n\r\x{2028}\x{2029}]` |
| `regex` crate 的 `\s`、`\w` 預設是 **Unicode** | header 與標籤只能認 ASCII 空白 | 明確寫出字元類別 `[ \t\n\x0B\x0C\r]`,不依賴 `\s` 或 `(?-u:\s)` 的定義 |
| `str::trim()` 會處理 Unicode 空白;`trim_ascii()` / `is_ascii_whitespace()` **不含** `\x0B` | 兩個套件曾因 trim 的字元集不同,寫出**不同檔名** | 自己寫:`s.trim_matches(\|c\| matches!(c, ' ' \| '\t' \| '\n' \| '\x0B' \| '\x0C' \| '\r'))` |
| `str::len()` 是 byte 數 | 通知的字元數必須是 **UTF-16 code unit**(emoji 算 2) | 用 `s.encode_utf16().count()` |
| `to_lowercase()` 做 Unicode 轉換 | 例如 Kelvin 符號 `K` 會對到 `k`;header 的 `file:` 曾因 case folding 在一邊是 header、另一邊是內容 | 只用 `eq_ignore_ascii_case`,或 regex 寫 `[Ff][Ii][Ll][Ee]:` |
| `str::lines()` 會吃掉 `\r\n` 的 `\r`、也不回傳結尾的空行 | 切行規則必須跟 TS 的 `split(/\r?\n/)` 逐項相同 | 用 `split('\n')`,**除了最後一段以外**每段 `strip_suffix('\r')`(最後一段後面沒有 `\n`,結尾的 `\r` 要保留) |
| `String::from_utf8_lossy` 會把錯誤位元組換成 U+FFFD | 非 UTF-8 檔案必須被跳過,不能帶著替換字元寫出去 | 一律 `String::from_utf8` / `std::str::from_utf8`,失敗就跳過並計數(會保留開頭的 BOM,符合規則) |
| `std::fs::canonicalize` 在 Windows 回傳 `\\?\C:\...` | 與使用者路徑做 containment 比對時永遠對不上 | 用 `dunce::canonicalize`;比對用解析後的 `Path` 元件,不用字串 |
| `std::path` 依 OS 而異 | 分隔符號、大小寫、磁碟機代號 | 回傳原生路徑;containment 用真實路徑比對,不用字串比對 |
| `Command` 經過 shell 會被引號規則影響;Windows GUI 程序呼叫 git 會閃出主控台視窗 | 兩個套件的測試曾在 Windows 上因 `cmd.exe` 不認 `'` 而全部失敗 | 一律 `Command::new("git").args(..)`,不經 shell;Windows 上加 `creation_flags(CREATE_NO_WINDOW)`(aghub 的 `src-tauri/src/lib.rs` 已有這個常數) |

### 已知且接受的差異

- 過濾規則中「不含 `*` / `?` 的原始 regex pattern」直接交給 Rust `regex` 編譯:`.`、`\w`、`\d`、`\b` 是 Unicode 語意,
  少數 JS 視為字面字元的語法(如 `\pL`、巢狀字元類別)意義不同。glob 形式的 pattern 已經照 JS 語意轉換,不受影響。
  使用者寫原始 regex 時很少碰到;真的出現分歧再逐項轉譯。
- `paths` 在 Windows 對超過 MAX_PATH 的路徑,`dunce::canonicalize` 會保留 `\\?\` 形式,containment 可能誤判為逃出 root 而拒絕(fail closed)。

## 2. 線上格式的不變量(摘要)

- 每個檔案一個 header,由 `headerFormat` 中的 `$FILE_PATH` 產生。**替換是字面替換**
  (`str::replace` 沒問題),絕不用 regex 替換。
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

TS 版透過 VS Code git 擴充取得 git 資料,Rust 版改為直接呼叫以下指令(都加 `-z`,路徑不會被引號包住)。
**用 `--raw --no-abbrev`,不用 `--name-status`**:`--raw` 才會給出舊版本的完整 blob OID,刪除前的內容要靠它讀。

| 需求 | 指令 |
|---|---|
| commit 的變更清單 | 對每個 parent:`git diff-tree -r -z --raw --no-abbrev --no-commit-id -M <parent> <sha>`,再依路徑取聯集;root commit 對空樹 `4b825dc642cb6eb9a060e54bf8d69288fbee4904`。聯集時要保留「哪個 parent、哪個舊 OID」:同一路徑在不同 parent 可能有不同的刪除前內容。**不要**用 `-c` / `--cc`(那是 combined diff,不是聯集) |
| commit 區間 | `git diff -z --raw --no-abbrev -M <a> <b>`(兩端點比較,不是逐 commit 相加) |
| staged 清單 | `git diff --cached -z --raw --no-abbrev -M` |
| working tree 清單 | `git diff -z --raw --no-abbrev -M`;未追蹤檔另用 `git ls-files --others --exclude-standard -z` |
| 讀內容(批次) | 長駐一個 `git cat-file --batch`,輸入 OID(或 `<rev>:<path>`;staged 用 `:<path>`),依 header 的 size 讀精確位元組數 |
| 是否為 shallow clone | `git rev-parse --is-shallow-repository`(shallow 的邊界 commit 看起來沒有 parent,不能當成 root commit 處理) |
| commit 模式:連續性 | `git rev-list --first-parent <tip>`,確認起點在其中,並取 `<base>..<tip>` 的 first-parent 序列 |
| commit 模式:metadata | `git log -1 -z --format=%an%x00%ae%x00%aI%x00%B <sha>` |
| commit 模式:重播 | `git add -A -- <paths>`,再 `git commit --no-verify --allow-empty --author="<name> <email>" --date=<iso> -F - -- <paths>`(message 從 stdin 餵,避免引號與換行問題) |

`--raw -z` 的輸出要**以位元組解析**(路徑可能不是 UTF-8)。讀到的內容一律先經嚴格 UTF-8 檢查(`String::from_utf8`),再轉成字串。

## 6. 剪貼簿

CLI 與 App 共用 `snip-core` 的 `clip` 模組,底層用 [`arboard`](https://crates.io/crates/arboard)
(`tauri-plugin-clipboard-manager` 內部也是它)。剪貼簿讀寫留在 Rust 端,不經過前端。

| 平台 | arboard 的做法 | 注意 |
|---|---|---|
| Windows | Win32 API,`CF_UNICODETEXT` | 不要透過 `clip.exe` 或 PowerShell,編碼容易出錯;需驗證換行是否被轉換 |
| macOS | `NSPasteboard` | |
| Linux | X11 / Wayland(`wayland-data-control` feature) | CI 需要 xvfb;X11 上擁有剪貼簿的程序一結束內容就消失,CLI 的 `copy` 要用 arboard 的 `SetExtLinux::wait()` 留在背景直到內容被取走 |
