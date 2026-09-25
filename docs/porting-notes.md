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
| `Command` 經過 shell 會被引號規則影響;Windows GUI 程序呼叫 git 會閃出主控台視窗 | 兩個套件的測試曾在 Windows 上因 `cmd.exe` 不認 `'` 而全部失敗 | 一律 `Command::new("git").args(..)`,不經 shell;Windows 上加 `CREATE_NO_WINDOW`。Job object 會改寫 `Command` 的 `creation_flags`,所以這個 flag 經 process-wrap 的 `CreationFlags` 設定(`gitrun.rs`) |

### 已知且接受的差異

- 桌面 App 在 monorepo 子資料夾選 Git 來源時,變更清單與複製範圍限制在該資料夾,並可逐檔勾選;CLI 與原本的 `collect_payload` 仍複製整個 Git 來源。這是桌面選取範圍的行為,不改剪貼簿格式。commit / 區間的 payload 路徑仍依 TS graphCopy 使用 repo 相對路徑。
- 過濾規則中「不含 `*` / `?` 的原始 regex pattern」直接交給 Rust `regex` 編譯:`.`、`\w`、`\d`、`\b` 是 Unicode 語意,
  少數 JS 視為字面字元的語法(如 `\pL`、巢狀字元類別)意義不同。glob 形式的 pattern 已經照 JS 語意轉換,不受影響。
  使用者寫原始 regex 時很少碰到;真的出現分歧再逐項轉譯。
- git 來源(graph:commit / 區間)被過濾規則排除的**刪除檔**不會進 payload。TS graphCopy 會先放刪除檔再過濾,
  會把被排除的檔案(例如 `secrets.env`)的舊內容帶出去;Rust 刻意不照做。
- commit 模式重播時,路徑逐一放在 `git add` / `git commit` 的參數上。Windows 命令列約 32K 字元上限,
  一個 commit 動到數千個檔案時會失敗;需要時改用 `--pathspec-from-file=- --pathspec-file-nul`。
- `paths` 在 Windows 對超過 MAX_PATH 的路徑,`dunce::canonicalize` 會保留 `\\?\` 形式,containment 可能誤判為逃出 root 而拒絕(fail closed)。
- 所有 git 程序都經 `gitrun`:全域同時最多 2 個、最多 64 個呼叫排隊(一般並行碰不到,爆量才拒絕)(再多直接 `QueueFull`)、排隊可逾時或取消、每次呼叫有期限、stdout 有上限,
  結束時殺掉整棵程序樹、reap root、關閉管線後才釋放名額;清理無法確認時回報 `Cleanup` 並永久保留該名額(`leaked_slots`),不假裝已乾淨。TS 版沒有這些限制。
  - 上限:`RunOptions::default()`(舊的 CLI / 桌面路徑)256 MiB、300 秒;新流程用 `RunOptions::interactive`(64 MiB、30 秒、排隊 10 秒,嚴格)
    或 `RunOptions::preview`(1 MiB、15 秒,明確截斷)。超過上限預設是錯誤(`OutputLimit`),不是截斷。
  - Unix:root 自成 process group,以 `killpg` 殺整組。root 在殺 group 之前**不會被 reap**:結束狀態用 `waitid(WNOWAIT)`(Linux/FreeBSD)
    或 kqueue `NOTE_EXIT`(macOS)觀察,因此 group id 不可能被別的程序重用。刻意 `setsid` 脫離 group 的後代殺不到;
    它若握住管線,呼叫在期限到時回報 `OutputHeldOpen` 失敗,不會卡住也不會回報成功。
    在 macOS / Darwin 上，XNU 核心的 `killpg1` (`bsd/kern/kern_sig.c`) 走訪 process group 時會過濾掉 zombie (`p->p_stat != SZOMB`)，
    當群組內所有程序皆已結束(或未 reap 的 root 是唯一程序且已退出)，可發送信號之程序數為 0，在 POSIX 模式下會回傳 `EPERM` (`os error 1`)
    而非 `ESRCH`。但 `EPERM` 亦可能因 MAC policy 或特權限制導致無法對存活後代發信號，因此 `gitrun` 不無條件忽略 `EPERM`，
    而是在遇上 `EPERM` 時以 `/bin/ps -ax -o stat=,pgid=` 檢查該 PGID。此檢查有嚴格的生命週期時限與空間邊界：
    500 ms 總 deadline（涵蓋 stdout 管線讀取與 EOF 後的程序退出等待，防止 helper 關閉 stdout 後卡死）、256 KiB 輸出上限、
    以及 200 ms 終止寬限（kill 後輪詢 try_wait 確保 reap，並將 kill/reap 失敗向外傳播，絕不在 Drop 內無窮等待或遺留 stray/zombie helper）。
    其代價為 $O(\text{processes})$ 的系統程序表掃描，且僅在 macOS / Darwin 遇上 `EPERM` 時才觸發。
    解析器採零記憶體分配驗證（非 UTF-8、欄位數量不符、無效 primary stat、非數字 PGID、缺少換行截斷皆回報錯誤），
    僅在確定該 PGID 僅剩 zombie (`'Z'`) 或無成員時才判定清理成功；任何非 `'Z'` 的存活成員、非零 exit code、超時、溢位或解析失敗一律嚴格 fail closed
    回傳錯誤並保留名額（leak slot），確保活體後代絕不被假裝乾淨。
  - Windows:process-wrap 10 以 Job object 管理(先 suspended spawn、放進 job、再 resume;job 建立或 assign 失敗時它會終止那個 suspended 子程序,
    resume 失敗時終止整個 job)。取代了 command-group 5.0.1,後者在 assign 失敗時會留下 suspended 子程序與 job handle。
    process-wrap 的 std `JobObject` **不是** kill-on-close:`TerminateJobObject` 失敗時只能再殺 root,並回報 `Cleanup`、保留名額。
    job 沒有 `JOB_OBJECT_LIMIT_BREAKAWAY_OK`,後代無法以一般方式脫離;若有程序把管線 handle 複製給 job 外的程序,
    reader thread 最多等 5 秒,之後回報錯誤,該 thread 留著(無法強制結束 thread)。
  - 明確的 `finish` / `CatFile::close` 會回報清理失敗;`Drop` 無法回傳錯誤,會再試一次,仍失敗就保留名額。
  - 同一個 thread 在持有 git 名額時(例如開著 `cat-file --batch`)再啟動 git 會直接失敗(`NestedProcess`),避免兩個這樣的 thread 把名額卡死。
  - 修改 index / ref 的重操作(`commits::replay`)在同一個 worktree(以 git dir 區分)一次只跑一個,最多 4 個等待者(`workspace::lock_heavy`)。
- `browser::directory` 最多讀 10,002 個項目,超過就回報錯誤,不回傳部分清單;非 UTF-8 的檔名沒有能指到它的字串路徑,所以不列出
  (不會把它有損轉成另一個檔案的路徑)。可續讀、保留 OS 原始檔名的分頁是 `workspace::DirectoryScan`:每次呼叫的工作量有上限,
  頁內依「目錄優先、名稱位元組」排序、跨頁是 OS 列舉順序;目錄在掃描中變動(時間戳只精確到檔案系統的解析度)就回報 `Changed`。
- repo 探索(`workspace::Discovery`)只保留每層一個開著的目錄(最多 `max_depth + 1`,上限 32 層),不累積待走路徑;
  超過深度或無法讀取的目錄會逐頁列出,走完時狀態是 `Incomplete` 而非 `Complete`;repo 數到上限時 `LimitReached`,可 `raise_repo_limit` 後續走。
  沒有 checkout 的 submodule 沒有 `.git`,探索看不到,要用 `declared_submodules` 從 `.gitmodules` 列出。
- 桌面預覽(`browser::git_preview`)的內容與 patch 都是嚴格的:超過 1 MiB 就報錯,因為現有 UI 無法表達「已截斷」。
  可截斷的版本是 `git_preview_with` + `RunOptions::preview`:只保留完整的 hunk,每個 hunk header 的行數與內容一致;新檔案合成的 patch 同樣只保留完整的行。
- transfer 的刪除來源分開:`Working` 讀 `HEAD:<path>`(與 gitsrc / TS 的 SCM 行為一致)、`Unstaged` 讀 index(`:<path>`)、`Staged` 讀 `HEAD:<path>`。
  工作區的刪除會把「不存在」記入 freshness,寫剪貼簿前若路徑又出現就視為過期。
- 瀏覽 commit 目錄(`browser::commit_directory`)、blob(`browser::commit_blob`)與作者歷史(`browser::history_by_author`):
  - 嚴格路徑身份:非 UTF-8 檔名在 `commit_directory` 中直接回報 `GitError::Malformed("unsupported non-UTF-8 path in commit directory")`，絕不靜默略過、絕不捏造檔名、不使用有損替換字元，確保有效的 `U+FFFD` 檔名（例如 `a\u{FFFD}`）與字面包含 `[unsupported non-UTF-8]` 的合法檔名皆能完全正常讀取與操作；`BlobText::NotUtf8` 僅保留給檔案內容位元組非 UTF-8 的情況。
  - 嚴格 NUL 終止紀錄:`ls-tree -z` 僅以結尾包含 NUL 的完整 record 建立項目；截斷時拋棄不完整的尾端並忠實標記 `truncated = true`，絕不憑中斷片段捏造檔名。若截斷導致連一筆完整紀錄都無法解析且 limit > 0，回傳 `GitError::OutputLimit`。
  - 嚴格預算傳遞:所有 `_with` 變體將取消權杖(`CancelToken`)、逾時與輸出上限傳遞至包含 `resolve_commit_with`、`head_with`、`cat-file -s` 在內的每個子程序。
  - 有界輸出捕捉與大小限制:目錄列表(`ls-tree`)stdout 上限 8 MiB、項目數上限 2,000;歷史紀錄(`log`)stdout 上限 16 MiB、筆數上限 10,000;blob 預覽上限 1 MiB。
  - 誠實截斷與防範假成功:嚴格 blob 與歷史查詢在輸出遭截斷時回傳 `GitError::OutputLimit`，絕不截斷後回傳殘缺成功(`Text`)或假完結(`has_more: false`)。`resolve_commit_with` 嚴格要求完整 OID (40/64 hex)，截斷時拒絕輸出。


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
- **git 來源**讀不到的檔案放 placeholder(`// Unable to read file content`)進 payload,但**不算已複製**,也**不佔檔案數上限**。
  **磁碟來源**(檔案模式)讀不到或非 UTF-8 的檔案不放 placeholder,只計數;超過大小上限的放 skipped marker。
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
