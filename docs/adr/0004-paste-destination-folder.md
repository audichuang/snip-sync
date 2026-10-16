# 貼上的目的資料夾可以還不存在，落點用「錨點加尾段」表示

貼上的目的資料夾與導入路徑（`--map` 的資料夾、之後 GUI 的 prefix 資料夾）可以是還不存在的資料夾。引擎不再要求落點是 `CanonicalRootId`（必須能 canonicalize，也就是必須已存在），而是用 `PlacedRoot`：最近一個已存在的祖先（錨點，canonical）加上還要建立的名稱（尾段）。資料夾已存在時尾段是空的，行為與計畫、摘要、快照和線上 frame 都和以前逐位元相同。

## 決定

1. **尾段在路由裡一致套用，不放進 `primary_prefix`；接上尾段之前先驗證路由後的檔案路徑。** 錨點才是 `plan_import_parsed_with` 的 root（`destination_roots`、`op.root_path`），`op.relative_path` 是 `尾段/相對路徑`。命中某個 prefix 的 entry 直接拿 `rest`、絕對路徑 entry 也不會加上 `primary_prefix`，所以尾段若放進 `primary_prefix` 就只套得到其中一種路由。留給檔案本身的路徑要先通過 `sanitize_relative_path`（非空、有檔名）才接尾段，否則 `.` 或 `p/` 會變成目的資料夾本身。
2. **規劃只讀不寫。** `PlacedRoot::resolve` 只查 `symlink_metadata`；沒有東西在規劃時被建立，包含 `--dry-run`。大小寫偵測也只讀（向檔案系統查詢能力，查不出來才用現有項目或資料夾自己的名稱比對，不建探測檔；仍無法確定就當不分大小寫），測試以錨點目錄的 mtime 驗證。
3. **「指定新資料夾」只在 Apply 經同意後、在寫入鎖之下逐層建立。** 指定新資料夾是使用者指名的目的資料夾或導入路徑的尾段；檔案路徑裡的中間資料夾（順帶新資料夾）不需同意。同意的單位是 `PlacedRoot`：計畫帶著每個有尾段的 `PlacedRoot`（primary，再依名稱排序的 prefix；錨點與尾段相同的只列一次）resolve 時逐層的原始路徑（`DestinationFreshnessSnapshot::placed`），`RestoreSelection::confirmed_placed` 以它在這份清單裡的位置同意，不是路徑字串或名稱鍵：使用者同意的是「這個目的地的尾段」。每個 create 屬於哪個 `PlacedRoot` 由規劃時的路由決定（`PlannedNewDirs::routes`），不比對名稱。Apply 在寫入鎖之下：尾段任何一層現在存在就 `StaleDestination`（`revalidate`，worker 對 master 的快照與自己重新規劃的快照各做一次，`placed` 必須與預覽相同）；勾選中的新增列路由到的 `PlacedRoot` 未同意就整批 `NewDirsNotConfirmed`；建立完所有資料夾與空檔之後，未同意的 `PlacedRoot` 任何一層存在（不論經哪個名稱建出），也整批 `NewDirsNotConfirmed`。失敗或一個檔都沒寫成時，以物件身分確認後把這次建立的空檔與資料夾由深到淺刪回（有內容的留著）。worker 只建自己重新規劃算出的資料夾，master 送來的同意只是位置。
   `requested_dirs`（規劃時以錨點物件身分加 `name_key` 算出的分類）只決定 UI 與 CLI 列出的清單，不放行任何東西。同一個資料夾的兩種拼法在同一次 Apply 只會真正建立一次：第二個 `create_dir` 得到 `AlreadyExists`，而那個物件是這次 Apply 自己剛建的，就不算過期。

4. **freshness 涵蓋寫入鎖鎖住的 repo（#237），指定新資料夾在預覽後出現就算過期。** 錨點一變，落點路徑就變了，所以不像順帶新資料夾那樣容許它後來變成一般資料夾；`revalidate` 對 `requested_dirs` 裡任何現在存在的東西回 `StaleChange::Created`。建立之後再確認同一個位置仍是剛建立的那個物件、且不是 symlink，擋住 revalidate 和 mkdir 之間被換掉。

## 設計原則：引擎不判斷名稱等價，在寫入鎖下問檔案系統

哪兩個名稱是同一個資料夾或檔案，是檔案系統自己的規則：大小寫、Unicode 正規化（APFS 與 HFS+ 對 NFC／NFD）、`ς`／`σ`、firmlink 與 bind mount 的兩個拼法，還有 HFS+ 直接忽略的字元（U+200C 等）。#241 曾連續七輪在「名稱等價」上找到新的同意繞過或靜默覆寫，每一輪都是程式在使用者空間預測的那張等價表少了一項；這張表無法窮盡。所以安全關卡不建立在預測上：

- **同意以 `PlacedRoot` 對應。** 列屬於哪個目的地，由路由在規劃時決定；尾段存不存在，由 `symlink_metadata` 在鎖下回答。Apply 先建好已同意的尾段與所有新檔的資料夾，再逐層看未同意的尾段：還不存在就沒事，存在就表示這次寫入（經任何一個名稱）建出了使用者沒同意的資料夾，整批拒絕。
- **碰撞以 `create_new` 偵測。** Apply 在寫入任何內容之前，對每個預期不存在的目標以 `OpenOptions::create_new` 建立空檔、路上的資料夾逐層 `create_dir`，並記下每個物件的身分（unix 的 dev／ino，Windows 的 volume serial 與 file index）。`AlreadyExists` 的物件若是本批建立的、或是本批要覆寫或刪除的檔，就是 `TargetCollision`；否則是預覽之後外部出現的，`StaleDestination`。全部建立成功後才寫入內容。新檔的內容經同一個物件的 handle 寫進空檔，不以 rename 換掉目標：寫入前以路徑與 handle 確認它仍是同一個物件、仍是空檔、修改時間未變；任何一項不符就視為外部已建立，依既有檔的規則處理（預設與 `skip_existing` 跳過並保留外部內容，允許覆寫時才寫，非 UTF-8 一律不覆寫）。剩下的時間窗是 handle 確認與寫入之間。要覆寫或刪除的既有檔也以物件身分去重（大小寫別名、硬連結）。
- **資料夾剛建立就被替換**，以物件身分判斷：`create_dir` 之後記下 `DirKey`，再確認同一個位置仍是同一個物件、且不是 symlink；HFS+ 把 NFC 名稱存成 NFD 不是替換。
- **`name_key` 只用在預覽。** 它決定同意清單怎麼分類、預覽要不要先擋下「可能碰撞」（寧可多擋），以及絕對路徑的 entry 對到哪個尚不存在的目的地（以錨點物件身分加尾段 `name_key` 比對 `PlacedRoot`；沒命中只會落入外部絕對路徑的既有規則）。測試有一個開關（`test_hooks::distinct_name_keys`）讓 `name_key` 對每次呼叫都回不同的鍵，所有先前的繞過與覆寫重現在這個開關下仍必須在 Apply 被擋下。
- 代價：Apply 中途被中斷（行程被殺）時，還沒寫到的新檔會留下空檔，正在寫入的那一個新檔可能只寫了一部分（內容直接寫進空檔，不經暫存檔 rename）；寫入前就拒絕的批次會收回所有空檔與資料夾。

## 為什麼用 opt-in 旗標

桌面 App 的行為不變：它永遠不建資料夾。`PasteRoute.allow_new_dirs`（線上是 `PasteMapping.allow_new_dirs`）預設 false；false 時本機規劃走原本的 `CanonicalRootId::new`、worker 照舊拒絕不存在的資料夾。CLI 一律打開它；之後的 GUI 單（確認 modal、導入路徑編輯）在同一個 PR 打開它並把 `PasteTarget::Local.bound` 設成工作區根目錄。

## 其他

- **`PREPARED_IMPORT_VERSION` 不升。** prepared 快取是 worker 行程內的記憶體，線上只多了加法欄位；升版會讓協定 7–8 的 worker 失去 prepared import，是退步。只升 `PROTOCOL_MAX` 到 9，新增 `CREATE_DIRS_VERSION = 9`。協商低於 9 的 worker 照舊拒絕不存在的資料夾；master 只在被拒絕之後確認那個資料夾真的不存在，才回 `WorkerTooOld`，一般的貼上不多一次往返。
- **master 不解析 worker 的路徑。** 計畫要建立哪些資料夾、每一列需要哪幾個，由規劃的那一台依它自己的路徑規則算好，放在 `DestinationFreshnessSnapshot::new_dirs`（`dirs` 由淺到深、各一次；`needs` 依 create 的索引列出它需要的 `dirs` 位置；`routes` 依 create 的索引列出它路由經過的 `PlacedRoot` 位置），master 只依勾選的列取出、照順序列給使用者並原樣送回同意，不對 worker 的路徑做 `parent()` 之類的運算：Unix 上 `C:\ws\new\a.txt` 的反斜線是檔名字元，曾讓 Windows worker 的計畫在 Unix master 上列不出資料夾、`--create-dirs` 送出空的同意。只在有尾段（`placed`）時記錄，沒有尾段的快照與 frame 逐位元不變；不進 `matches_preview`（由 digest 涵蓋的計畫與 `missing_dirs` 推得，worker 的 Apply 也以自己重新規劃的計畫重算）。沒有這個欄位時（沒有尾段，或此欄位之前的協定 9 開發版 worker）master 退回依路徑計算，只在兩台的路徑規則相同時正確。
- **工作區外一律不允許**：GUI（`bound`）與 worker（以工作區為界）。CLI 不設界。
- **名稱規則在所有平台一致**：尾段拒絕 `..`、`.git`、`sanitize_relative_path` 擋的字元、結尾是點或空白的名稱與 Windows 保留名稱，讓同一個指令在哪台機器上都得到同一個結果。已存在的資料夾不受影響，因為規則只檢查要建立的名稱。
- 新資料夾不能建進 Git 目錄（`PlacedRoot::resolve` 與 Apply 各檢查一次）。
- 它取代 `docs/unified-engine-plan.md` D4 的「GUI 第一步不改，另開單決定」中關於落點的部分：落點差異改由 `PlacedRoot` 在引擎裡解決。
- commit 貼上不變：`plan_commits` 與 worker 的 `replay_root` 仍要求目的資料夾已存在。
