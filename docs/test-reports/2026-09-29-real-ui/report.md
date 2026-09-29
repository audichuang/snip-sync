# snip-sync macOS 真實 UI 測試報告

測試日期：2026-09-29（Asia/Taipei）  
本輪資源取樣：2026-09-29 22:06:16 +0800 至 2026-09-29 22:40:25 +0800，共 2049.00 秒（約 34 分鐘）。  
目的：驗證多 repo 複製／貼上、commit 是否真正建立、檔案與 Git 狀態是否正確，以及操作後資源是否持續增長；保留問題供後續優化。

## 1. 結論與優先處理項目

**核心傳輸與重播能完成，這一版仍有阻擋常見操作的 UI 問題，不能判定全部通過。**

- 本輪從 App 實際建立 **6 個新 commit**：3 個連續重播、C4 首次重播、C4 重複重播的空 commit，以及反向傳送的一個空 commit。前輪另有 3 個，本輪再次以 Git 核對。fixture 建置時建立的 commits 不計入這個數量。
- 最優先是 **BUG-01：貼上清單中同相對路徑重複出現時，第二個控制項無效**。它同時影響「連續 commits 都修改同一檔」與「不同 repos 各有同名檔案」。前者會阻止整段 commit 重播。
- Commit 預覽有多處與實際行為不一致：binary SKIP 顯示為建立、rename 未顯示舊路徑刪除、空 commit 沒有任何 message/作者/時間預覽。另有二進位刪除與規格不一致。
- 15 repo、15,000 tracked paths、15,000 commits 的資料下，100 次真實切換完成，無崩潰。RSS 穩態中位數 **132.750→136.219 MiB（+3.469 MiB）**；切換區間程序樹取樣峰值 **141.438 MiB**。FD 沒有增加，閒置時 Git 子程序排空。這次沒有足夠證據判定持續洩漏，也不構成「沒有洩漏」的證明。
- **本輪未修改產品程式碼、未 commit/push/release。** 測試結束時來源 checkout 為乾淨的 `main`；測試 App 已用 Cmd+Q 正常關閉。

## 2. 環境與證據口徑

| 項目 | 實際值 |
|---|---|
| 作業系統 | macOS 27.0 / build 26A428，ARM64 |
| 實體 RAM | 24 GiB |
| App | snip-sync 0.3.2，本機測試 bundle |
| Source SHA | `167c10cd647fdadc1c0711f94b74940d8490f528` |

> 本報告描述測試當時 `main` 的 0.3.2 source SHA `167c10c`。此報告分支由較新的 `develop` SHA `068c467` 建立；後續程式變更可能已改變部分行為。本報告中的程式碼連結固定到實際測試的 source SHA，BUG 項目在修正前應先於目前 develop 重現。
| Binary SHA-256 | `500531d90324c69f0bb4a2fb4d470955692c5644c932f14d6d3f903dd5efbb81` |
| 程序 | 取樣開始前已運行約 44 分鐘，屬暖程序 |
| Binary 路徑 | `local test bundle (path omitted)` |
| 實際視窗 | 1080×752 screenshot；本輪未測 900×600 |
| 正式 release 比較 | 未進行；此 bundle 的建置 profile 未在本輪重新核實，不拿它宣告 release RAM gate 通過 |
| UI 操作 | 全部透過 Computer Use 的真實原生點擊、鍵盤、滾動、系統剪貼簿 |
| Shell 用途 | 建立可丟棄 fixtures、唯讀核對 Git/檔案、量測程序；沒有用 CLI/API 代替 App 執行複製或貼上 |
| UI 證據 | 本次對話中的 Computer Use 截圖；本報告另附重現步驟、payload、Git/file oracle 與原始量測。未另存獨立截圖檔 |

完整環境資訊已保留於本機測試附件；為避免包含本機路徑與程序識別資訊，未納入 repo。原生 AX 只提供視窗級資訊，因此主要操作依實際截圖定位，100 次切換每次都重新取得 AX 狀態，並在第 10/30/50/70/90/100 次檢查畫面。操作時間包含工具觀察開銷，**不是 App 回應延遲 benchmark**。

### 測試資料

| 資料集 | 用途 | 路徑與規模 |
|---|---|---|
| 前輪 A/B | 基本多 repo、staged、三 commit、歷史檔案 | `fixture A`、`B`，各 15 repos |
| 本輪 source15/target15 | merge、binary、hooks、staged 保留、邊界、反向、多 repo 同名 | `fixture source15`、`target15`，各 15 repos |
| perf15 | 多 repo 切換／log／記憶體 | 15 repos × 1,000 paths × 1,000 commits，30 refs/repo，共 450 refs |
| empty | 非 Git 目的資料夾 | `fixture empty` |

測試資料 manifest 留存在本機測試附件，內含暫存目錄路徑，未納入 repo。效能資料是 generator 的 medium 加自訂數量，**不是**規範的 15×10,000 paths×20,000 commits standard 負載。資料中的 mock secret 字串由 workload generator 產生，不是真實憑證。

## 3. 實測情境矩陣

以下 40 列是明確定義的檢查項目，不是全產品測試覆蓋率。`PASS-WITH-UI-DEFECT` 表示磁碟／Git 結果正確但預覽有問題；`FAIL-CONTRACT` 表示實作與書面契約衝突；`OBSERVATION` 不代表驗收通過。

| ID | 情境 | 判定 | 結果 |
|---|---|---|---|
| T01 | 工作區：載入 15 repo，切換來源／目的工作區 | PASS | source15、target15、perf15 都顯示 15 個儲存庫。（本輪） |
| T02 | 多 repo：來源前綴未對應時禁止套用 | PASS | 未選 repo01 對應時 Apply 停用；選定後目的絕對路徑正確。（本輪） |
| T03 | 多 repo：不同 repo 同相對路徑各自覆寫 | FAIL | 兩個 unrelated.txt 的第二個覆寫無法勾選；只套用第一份，第二份保留。BUG-01。（本輪） |
| T04 | 多 repo：跨 repo 選 4 檔／資料夾展開後貼到另一工作區 | PASS | 建立 2、覆寫 2，alpha/beta 各自落在正確 repo；其餘 13 repo 未受影響。（前輪 UI、本輪重核磁碟） |
| T05 | Git 檔案：staged 新增、修改、刪除、rename 一起貼上 | PASS | 5 路徑：建立 2、覆寫 1、刪除 2；rename 舊路徑消失。（前輪 UI、本輪重核磁碟） |
| T06 | Git 檔案：同檔 staged 與 working 內容不同 | PASS | staged 複製取 index；Project 資料夾複製取 WORKTREE VERSION。（兩輪） |
| T07 | 檔案：資料夾遞迴複製及通知 | PASS | batch 資料夾：已複製 5 檔、略過 3 檔。（本輪） |
| T08 | 檔案：二進位與 UTF-16 檔案略過 | PASS | 未進可還原內容；目的原有 binary/UTF-16 位元組不變。（本輪） |
| T09 | 檔案：1,360,000-byte 大檔與 placeholder | PASS | 產生 size-exceeds-limit placeholder，貼上不建立 large.txt。（本輪） |
| T10 | 檔案：零位元組檔案 | PASS | empty.txt 確實建立且 size=0。（本輪） |
| T11 | 檔案：中文、空白路徑、CRLF 正規化 | PASS | 中文路徑與內容正確；CRLF→LF、前後空行／尾端 LF 移除符合既有契約。（本輪） |
| T12 | 檔案：逐檔取消及預設不覆寫 | PASS | 取消 new.txt；overwrite.txt 保留 DESTINATION KEEP；其餘 3 檔建立。（本輪） |
| T13 | 檔案：預覽後目的內容被外部修改 | PASS | 拒絕 Apply，保留新內容，沒有任何部分建立。（本輪） |
| T14 | 貼上：取消預覽不寫入／不建立 commit | PASS | Escape 取消；前輪 Git HEAD 不變，本輪可重新建立計畫。（兩輪） |
| T15 | 剪貼簿：一般文字／無效 payload | PASS | 使用原生輸入框 Cmd+C 複製測試文字，Paste 顯示格式無效，沒有寫入。（本輪） |
| T16 | 檔案：非 Git 資料夾作為目的地 | PASS | 選擇保留 repo01 前綴，建立 unrelated.txt 與 repo01/unrelated.txt，未建立 .git。（本輪） |
| T17 | Commit：包含 SIDE 分支的非 first-parent 連續選取 | PASS | 4 個 commit 選取被拒絕，顯示 commits are not contiguous。（本輪） |
| T18 | Commit：3 commits 重複修改已存在的同一路徑 | FAIL | common.txt 的第二個覆寫控件無效，整段無法重播。BUG-01。（本輪） |
| T19 | Commit：3 commits 貼入 common.txt 尚不存在的另一 repo | PASS | 真的建立 3 個新 SHA，git rev-list before..HEAD=3。（本輪） |
| T20 | Commit：完整 message、作者、email、時間與時區 | PASS | 本輪 3 個逐一比對相等；多行中文訊息與空白行保留。（兩輪） |
| T21 | Commit：merge 以 first-parent 差異重播 | PASS | 來源 C3 為 merge，目的 commit 一個 parent，僅寫入 side.txt，沒有額外 SIDE commit。（本輪） |
| T22 | Commit：rename、文字刪除、Unicode/emoji 內容 | PASS | 舊檔消失、新檔及文字位元組正確；但 rename 預覽不完整，見 BUG-02。（兩輪） |
| T23 | Commit：目的地原有 staged／untracked 檔案保留 | PASS | unrelated.txt index blob 不變，沒有混入重播 commits；local-only.txt 保留。（本輪） |
| T24 | Commit：略過四種 hooks | PASS | pre-commit、prepare-commit-msg、commit-msg、post-commit 均設成 marker+exit1；重播成功且 marker 未出現。（本輪） |
| T25 | Commit：來源二進位刪除 | FAIL-CONTRACT | payload notCopied=null，實際刪除 binary.dat；與 spec 4.2 不一致。BUG-03。（本輪） |
| T26 | Commit：來源二進位新增 | PASS-WITH-UI-DEFECT | payload BINARY、實際略過，文字正常提交；預覽卻算成 CREATE。BUG-02。（本輪） |
| T27 | Commit：同一 C4 重複貼上 | PASS | 產生兩個不同 SHA；第二個 tree 與前一個相同，確實為空 commit。（本輪） |
| T28 | Commit：反向複製空 commit 到來源工作區 | PASS-WITH-UI-DEFECT | source15/repo05 多出 1 個空 commit；預覽缺 message/author/date。BUG-02。（本輪） |
| T29 | Commit：取消部分檔案後拒絕整段重播 | PASS | 取消 side.txt 後拒絕 Apply，HEAD 與磁碟未變。（本輪） |
| T30 | Commit：未確認覆寫時拒絕重播 | PASS | 缺任一覆寫確認即拒絕，沒有建立部分 commit。（本輪） |
| T31 | Commit：目的父路徑被一般檔案擋住 | PASS-WITH-UI-DEFECT | 安全拒絕，HEAD/status/阻擋檔內容不變；錯誤露出翻譯 key。BUG-05。（本輪） |
| T32 | 歷史檔案：Git 歷史刪除檔的單檔複製 | LIMITATION | Copy Files 停用；新增檔可複製。屬明確實作限制，見 LIMIT-01。（前輪 UI、本輪源碼確認） |
| T33 | 歷史：repo filter 與 regex message filter | PASS | ^C[123] 正確留下 3 個 first-parent commits。（本輪） |
| T34 | 歷史：大歷史清單分頁 | PASS | 實際滾動觸發 50→100→150 筆載入。（本輪） |
| T35 | 歷史：搜尋未載入的第 500 個 commit | PASS | progressive enhancement 500 找到 d523d45，檔案預覽指出 commit 500。（本輪） |
| T36 | 歷史：15 repo 全部顯示時的訊息欄 | FAIL | 1080×752 視窗下訊息欄消失；單 repo filter 後恢復。BUG-04。（本輪） |
| T37 | 資源：100 次真實 repo 切換 | PASS | 涵蓋 15 repo，100 次完成，最後回 repo01；無 crash/hang。（本輪） |
| T38 | 資源：連續 RSS／footprint／FD／thread 量測 | OBSERVATION | 100 次前後 RSS 中位數 +3.469 MiB；不是 release gate 或無洩漏證明。（本輪） |
| T39 | 生命週期：關閉工作區、重新開啟、剪貼簿保留 | PASS | 關閉為 0 repo；重開單 repo成功；前後 clipboard SHA256 相同。（本輪） |
| T40 | 生命週期：正常 Cmd+Q 退出 | PASS | App quit、PID 消失；384 個曾取樣到的子程序 PID 均不再存活。退出碼未取得。（本輪） |

可供排序或後續追蹤：[test-cases.csv](evidence/test-cases.csv)、[test-cases.json](evidence/test-cases.json)。

## 4. 可重現問題與優化方向

### BUG-01 — P1：重複相對路徑的貼上控制項失效

**重現 A（commit 模式）**：開 `source15/repo01`，Git log 篩 repo01，regex `^C[123]`，選 C1→C3，Copy Commits；到 `target15/repo01` 的 `qa-replay` 貼上。C1/C2 都修改 `common.txt`。第一筆覆寫可勾，第二筆點 checkbox 或文字都無效，點第二列也保留第一列詳細內容。重開乾淨預覽可再次重現；因缺第二筆確認，整段 Apply 被拒絕。

**重現 B（多 repo 檔案模式）**：複製 target15 的 repo01/repo03 staged `unrelated.txt`，主要 repo=repo03；到 source15/repo03 貼上，把 repo01 前綴映射至 source15/repo01。清單有兩個 root、各一個 `unrelated.txt`；第二個覆寫同樣無效。只允許第一個並 Apply，結果覆寫1／跳過1，磁碟沒有寫錯 repo，但無法完成預期的兩檔覆寫。

高可信度原因：[crates/desktop-native/src/ui/paste.rs](https://github.com/audichuang/snip-sync/blob/167c10cd647fdadc1c0711f94b74940d8490f528/crates/desktop-native/src/ui/paste.rs#L323) 的 row/include/overwrite UI ID 都只包含 `path`，沒有 destination root 或 commit/item 身分。報告未修改程式碼，根因需在修正時用回歸測試確認。

建議：以目的 root + 穩定 operation/item ID 建立唯一控件 ID，commit 模式再包含 commit index；probe ID 也要同步。加入兩個不同 repo 同相對路徑、兩個 commits 同路徑的 `#[gpui::test]`，並以真實 UI 確認兩列都能選取與覆寫。

證據：[commits-payload.txt](evidence/commits-payload.txt)、[reverse-multirepo-payload.txt](evidence/reverse-multirepo-payload.txt)、[reverse-multirepo-oracle.json](evidence/reverse-multirepo-oracle.json)。原始 target15/repo01 尚未成功重播該範圍，可直接作為重現目的地。

### BUG-02 — P2：commit 預覽未忠實呈現實際執行計畫

1. **Binary SKIP 被算成 CREATE**：C4 新增 `new-binary.bin` 與 `newdir/content.txt`。payload 的 binary 帶 `notCopied=BINARY`，詳細文字顯示 `action: SKIP`，但清單是綠色、原因說將建立新檔、總計「建立2／跳過0」。實際只寫文字檔。
2. **Rename 舊路徑未揭露**：顯示新路徑 CREATE，未顯示 old.txt 被刪除，刪除總數也沒有含這個動作；實際重播確實刪舊、寫新。
3. **空 commit 沒有可審查資訊**：貼一個 `files=[]` 的 commit，預覽0項、建立0，畫面沒有 message／作者／時間，但 Apply 真的建立1個 commit。
4. 多個 commits 被攤平成依路徑排序的檔案清單，沒有清楚的 N 個 commit 順序及每個 commit 的完整分組，不易理解同路徑反覆修改。

程式線索：[paste_op](https://github.com/audichuang/snip-sync/blob/167c10cd647fdadc1c0711f94b74940d8490f528/crates/desktop-native/src/ui/mod.rs#L476) 依 selected/is_delete/dest_exists 決定顏色與原因，沒有優先處理 `action_label=SKIP`；[build_commit](https://github.com/audichuang/snip-sync/blob/167c10cd647fdadc1c0711f94b74940d8490f528/crates/desktop-native/src/paste.rs#L912) 建立以檔案為主的預覽。規格要求見 [docs/spec.md](https://github.com/audichuang/snip-sync/blob/167c10cd647fdadc1c0711f94b74940d8490f528/docs/spec.md#L101)。

建議：直接以 replay plan 呈現 commit 分組與順序，明示 skip reason、rename old→new、空 commit；將「commit 數」與「檔案動作數」分開。統計、每列原因與最終動作必須同源。

證據：[c4-payload.txt](evidence/c4-payload.txt)、[c4-repeat-oracle.json](evidence/c4-repeat-oracle.json)、[empty-commit-payload.txt](evidence/empty-commit-payload.txt)、[reverse-empty-commit-oracle.json](evidence/reverse-empty-commit-oracle.json)。

### BUG-03 — P2／契約待決：二進位刪除真的被重播

C2 刪除含 NUL／非 UTF-8 的 `binary.dat`。產出的 commit payload 是 `DELETED`、`notCopied=null`；預覽是 DELETE；目的端此檔確實被刪除。

[docs/spec.md](https://github.com/audichuang/snip-sync/blob/167c10cd647fdadc1c0711f94b74940d8490f528/docs/spec.md#L94) 明確要求非 UTF-8／二進位標記為未複製，貼上不寫入也不刪除。[crates/core/src/commits.rs](https://github.com/audichuang/snip-sync/blob/167c10cd647fdadc1c0711f94b74940d8490f528/crates/core/src/commits.rs#L620) 對 Deleted 在讀 blob 編碼前直接回傳；本輪未找到 porting-notes 的接受差異。

這是**已確認的行為與規格衝突**；應先決定產品是否允許傳播 binary deletion。若維持規格，需檢查刪除前 blob 並產生 notCopied；若刻意允許，需明確修改契約與接受差異，並以測試鎖定。不要在未決定語意前只修改 UI 文案。

證據：[commits-payload.txt](evidence/commits-payload.txt) 中 binary.dat、[commit-replay-oracle.json](evidence/commit-replay-oracle.json) 的 `binaryPreserved=false`。

### BUG-04 — P2：15 repo 的 Git log 訊息欄被擠掉

在 1080×752 視窗、perf15、全部 repos、首50列時，graph、refs、作者與時間仍顯示，commit message 區卻無文字。點選列後右側 details 有真正的 message。篩成 repo-01-core 後訊息欄立即恢復，資料未遺失。

疑似原因：[crates/desktop-native/src/ui/log_view.rs](https://github.com/audichuang/snip-sync/blob/167c10cd647fdadc1c0711f94b74940d8490f528/crates/desktop-native/src/ui/log_view.rs#L615) 的 subject 可用寬度由固定欄與 gutter 相減，未保留 minimum subject width。建議限制多 repo 圖形／ref 寬度、讓作者日期或側面板有可折疊優先序，驗收 1080 與900寬下至少能看到可辨識的訊息。

### BUG-05 — P3：不安全父路徑顯示翻譯 key

target15/repo04 的 `newdir` 是一般檔案。貼 C4 時安全拒絕，但狀態顯示 `paste_err_destination (Not a directory (os error 20))`，沒有清楚指出阻擋路徑。HEAD、status、原檔內容都不變。

建議補齊 i18n key，顯示具體目標路徑與可理解原因；見 [crates/desktop-native/src/paste.rs](https://github.com/audichuang/snip-sync/blob/167c10cd647fdadc1c0711f94b74940d8490f528/crates/desktop-native/src/paste.rs#L886)。證據：[c4-repeat-oracle.json](evidence/c4-repeat-oracle.json)。

### BUG-06 — P3：commit 複製通知缺少略過資訊

C4 包含一個 BINARY 未複製檔，通知只說「已複製1個 commit」。未顯示檔案數、字元數、未複製數與所屬 commit，與 [docs/spec.md](https://github.com/audichuang/snip-sync/blob/167c10cd647fdadc1c0711f94b74940d8490f528/docs/spec.md#L94)、[docs/spec.md](https://github.com/audichuang/snip-sync/blob/167c10cd647fdadc1c0711f94b74940d8490f528/docs/spec.md#L97) 不符。現有文案見 [crates/desktop-native/src/i18n.rs](https://github.com/audichuang/snip-sync/blob/167c10cd647fdadc1c0711f94b74940d8490f528/crates/desktop-native/src/i18n.rs#L266)。

建議通知摘要至少含 commit 數、檔案數、未複製數，並能在預覽看到具體檔名與原因。

### LIMIT-01 — 歷史刪除檔不能從 changed-files 選單單獨複製

前輪實測：歷史新增檔的 Copy Files 可用；歷史刪除檔則停用。[crates/desktop-native/src/menu.rs](https://github.com/audichuang/snip-sync/blob/167c10cd647fdadc1c0711f94b74940d8490f528/crates/desktop-native/src/menu.rs#L74) 明確將 deleted 檔排除，這是現有實作的選擇，不標成偶發故障。staged 刪除與整個 commit 刪除的流程可以使用。

若產品希望「從 Git 歷史複製刪除動作、到另一處套用」，應提供帶 `[DELETED]` 的變更複製入口，並與「複製該版本的檔案快照」清楚區分。

## 5. 有沒有真的建立 commit：Git 證據

本輪的 App 重播目標是獨立初始化的 repo，沒有依賴相同 commit hash 或共同祖先。重播疊在目的地目前分支。以下 commit 不是 fixture setup 建立的。

| 來源／操作 | App 建立的 commit | 核對 |
|---|---|---|
| C1 多行中文訊息 | `0facbf366bff29fa50446c72525a67f71672c308` | message、author name/email、author date 完全相等 |
| C2 rename/delete | `f547ad9a35e38954646b5936907575130349152e` | metadata 相等；common修改、文字與binary刪除、rename 實際存在 |
| C3 merge | `fea37e31ee2108cde2ab3fe3ed04ff9bdbaefb96` | metadata 相等；一個 parent，side.txt 正確 |
| C4 首次 | `ada59fa1c2e3bc5f570a6b063d4a940d08878938` | 文字提交；binary不寫入 |
| C4 重複貼上 | `fb61985f671c1fe06ceead5a0b909450006330f0` | 新 SHA，tree 與上一個相同；確實是空 commit |
| 反向貼空 commit 到 source15/repo05 | `465deadc5a2c49af46403f90861f8c6c99b7ccda` | before..HEAD=1，tree 不變、working tree clean |

本輪 target15/repo03 的 before=`98975631ccfe7d5a82d6d4a5c76ec02ab8557a6c`，分支 `qa-replay`。不拿整個 source tree 與 target tree 相等作為本輪判準，因兩邊獨立 baseline、README 與 unrelated 檔本來就不同；核對的是每個提交的 metadata、涉及的 paths／位元組、parent 順序，以及非涉及 staged blob 保留。

[commit-replay-oracle.json](evidence/commit-replay-oracle.json)；[c4-repeat-oracle.json](evidence/c4-repeat-oracle.json)；[reverse-empty-commit-oracle.json](evidence/reverse-empty-commit-oracle.json)。前輪另外3個重播 SHA=`cf549aa…`、`e5c2d01…`、`2f20cc3…`，本輪重核每筆 metadata **與 tree** 都相等：[previous-round-oracle.json](evidence/previous-round-oracle.json)。

## 6. 記憶體與資源結果

### 量測方法

- 以 macOS `ps -axo pid,ppid,rss,%cpu,comm` 每名義500ms取樣，按當時 PPID 遞迴加總 App 程序樹 RSS。實際間隔 median=0.5051秒、p95=0.5053秒、最大=0.5103秒。
- 共 4060 筆，其中 4059 筆為活程序；最後一筆是正常退出事件。退出後的0值**不納入活程序記憶體統計**。
- RSS 單位由 KiB /1024 轉為 MiB。`vmmap -summary` 的 physical footprint 另列，保留工具的 M 單位，不能把它與 RSS/PSS 混用。
- idle 窗均超過30秒。操作 phase 含 UI操作、工具與思考間隔，duration 不能當工作完成時間或效率 benchmark。100 次 selection 第一至最後觀察約205秒，完整量測 phase 較長。
- 這是暖程序順序測試；未清除 filesystem cache、未量 process-cold launch、未量 Linux PSS，也沒有分別量 GPU／WindowServer。500ms 取樣會漏掉更短的 Git 子程序或瞬時峰值。

### 程序樹 RSS

| 場景 | 秒／有效樣本 | RSS 中位數 MiB | p95 MiB | 取樣峰值 MiB | 最多程序數 |
|---|---:|---:|---:|---:|---:|
| 既有程序／前輪後閒置 | 34.35 / 69 | 129.719 | 129.719 | 129.719 | 1 |
| 本輪 commit 操作，含操作間等待 | 606.25 / 1202 | 131.203 | 132.109 | 137.047 | 5 |
| 檔案與反向操作，含等待 | 480.47 / 953 | 132.062 | 134.250 | 137.906 | 5 |
| 15 repo 切換前穩態 | 63.12 / 126 | 132.750 | 133.016 | 133.016 | 1 |
| 100 次切換所在操作區間 | 306.80 / 609 | 136.219 | 137.484 | 141.438 | 3 |
| 100 次後穩態 | 52.02 / 104 | 136.219 | 136.219 | 136.219 | 1 |
| 分頁／搜尋操作區間 | 121.19 / 241 | 138.328 | 138.391 | 139.672 | 3 |
| 關閉工作區後穩態 | 78.27 / 156 | 138.297 | 138.359 | 138.359 | 1 |
| 重新開單 repo 暖程序穩態 | 54.55 / 109 | 138.047 | 138.250 | 138.297 | 1 |


100次前後：中位數 **+3.469 MiB（約2.6%）**；結尾52秒窗固定在136.219 MiB。切換後樹列保留了較多展開狀態，雖回到同一 repo／Changes／預覽內容，畫面不是完全相同的展開狀態。觀察到 retained 增量，無法單靠此數值區分快取、allocator保留或洩漏。

後續分頁／搜尋後 RSS 約138.3 MiB；關閉工作區78秒仍約138.3 MiB，重開單repo約138.0 MiB。**工作區關閉未立即讓 RSS 回到起點129.7 MiB**，適合列為後續 profiler 調查項目；目前增量規模約8.6 MiB，沒有取得 heap allocation stack，不能斷言是洩漏。

### Physical footprint 與其他資源

| 時點 | footprint（vmmap 原值） | App 生命週期 footprint peak（原值） | 數字 FD | thread |
|---|---:|---:|---:|---:|
| 本輪開始 | 74.5M | 100.8M | 本輪初始未取可靠數字FD基線 | — |
| perf15 切換前 | 76.4M | 104.0M | 5 | 7 |
| 100次後及再閒置 | 79.8M | 106.9M | 5 | 5 |
| 關閉工作區 | 81.5M | 109.0M | 5 | 6 |
| 重開單 repo | 81.5M | 109.0M | 5 | 5 |
| 退出前 | 81.8M | 109.0M | 5 | 5 |

footprint peak 是這個已運行很久的 App 的生命週期峰值，包含本輪開始前活動，不能說成某一操作的峰值。`ps %cpu` 取樣中位數在 idle約0.3%、切換約0.4%，僅為系統平滑估計，不是精準CPU工時。

取樣到的閒置窗均只剩主程序；100次切換區間最多3個並行程序，commit操作區間最多5個。整輪曾捕捉384個子程序PID，正常Cmd+Q後皆不再存活。主PID也消失。因為是 attach，未取得 App exit code，**不宣稱 exit0**。

此結果不等同 `native-resources-short/long` 或 D4 release gate。現階段更應先修正 BUG-01／02，再用固定 release binary、固定 warmup、相同展開狀態、至少多輪及更長 soak 重測 retained heap。

資料：[memory-summary.json](evidence/memory-summary.json)、原始 process sample 含程序與本機路徑資訊，留存在本機測試附件，未納入 repo；[memory-timeline.csv](evidence/memory-timeline.csv)、[quit-oracle-summary.json](evidence/quit-oracle-summary.json)。

## 7. 建議的修正與回歸順序

1. **先修 BUG-01**：讓每個 row/include/overwrite/probe ID 唯一。回歸兩個repo同路徑、兩個commits同路徑、rename前後重用路徑；確認兩列詳細內容與各自覆寫都能操作。
2. **重做 commit 預覽資料呈現**：直接以 commit/replay action 分組，顯示 commit 數、順序、rename舊路徑、binary skip reason、空 commit metadata；統計與執行一致。
3. **決定 binary deletion 契約**，再修 core 或明文紀錄接受差異；同時補 binary新增／修改／刪除／rename 的整套情境。
4. **修 multi-repo log 最小訊息寬度**，再補通知與翻譯。以1080與900寬、15repo多lane圖形驗收。
5. **記憶體優化依 profiler 證據進行**：先量 workspace close 前後的 owned model/cache、preview/tab/graph retained bytes，再量 allocator resident retention；不要僅以 RSS 未下降就刪快取或調整上限。

依 repo 的 AGENTS.md，UI state 回歸放 in-process `#[gpui::test]`；OS剪貼簿／真實輸入放 native-e2e；跨repo提交語意放 collaboration case。這次是測試與報告，沒有程式碼變更，因此未代替修正執行 preflight／push。

## 8. 尚未覆蓋，不能由本報告推論通過

- Windows/Linux 真實輸入、IME、跨機剪貼簿/RDP傳輸。A/B 是同一台 Mac 的獨立資料夾。
- 與 ClipCode/ClipCodeVSCode 的實際 UI 互貼；本輪只有 snip-sync UI，未重新跑 byte contract suite。
- standard 的15×10,000 paths×20,000 commits、500次切換／600秒long gate、多輪冷程序、受控 filesystem cold cache。
- 真實 Git replay 第N個 commit 失敗後的部分成功保留、磁碟滿／权限變化／缺 user identity、預覽後 HEAD/index 被改的完整矩陣。
- symlink traversal、惡意路徑字元、非UTF-8檔名、submodule/gitlink、sparse checkout、linked worktree、network filesystem 的完整安全矩陣。
- 視窗900×600、light theme、tray/hide、長時背景待機、無障礙操作的完整驗收。
- 長時間heap leak定位、GPU與WindowServer獨立量測、精確使用者操作延遲。

## 9. 留存資料與重現方式

完整原始產物與暫存 fixtures 保留在本機測試附件；repo 只保存去識別化報告、摘要與可分享的測試輸入／結果。

- 功能輸出：[file-matrix-oracle.json](evidence/file-matrix-oracle.json)、[stale-file-oracle.json](evidence/stale-file-oracle.json)、[non-git-oracle.json](evidence/non-git-oracle.json)、各 commit／多repo oracle。
- 可檢視 payload：[commits-payload.txt](evidence/commits-payload.txt)、[c4-payload.txt](evidence/c4-payload.txt)、[file-matrix-payload.txt](evidence/file-matrix-payload.txt)、[reverse-multirepo-payload.txt](evidence/reverse-multirepo-payload.txt)。
- 本次 UI 操作皆由 Computer Use 執行；Shell 僅用於建立可丟棄 fixtures、唯讀核對 Git／檔案結果及量測。
- 記憶體資料包含每 500 ms 的去識別化時間序列與彙總；原始程序取樣留在本機測試附件。
- perf fixture 原始命令：`python3 scripts/workload_generator.py <新的空目錄> --repos 15 --files 1000 --commits 1000 --refs 30 --quiet`。

報告中的 source links 指向此次測試 checkout 的行號。修正程式後行號可能改變；原始判斷以本報告記錄的 source SHA 為準。測試用剪貼簿最後內容是 `QA invalid clipboard payload`；測試 App 已關閉，fixture 與報告未清除。
