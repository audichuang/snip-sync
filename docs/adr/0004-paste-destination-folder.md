# 貼上的目的資料夾可以還不存在，落點用「錨點加尾段」表示

貼上的目的資料夾與導入路徑（`--map` 的資料夾、之後 GUI 的 prefix 資料夾）可以是還不存在的資料夾。引擎不再要求落點是 `CanonicalRootId`（必須能 canonicalize，也就是必須已存在），而是用 `PlacedRoot`：最近一個已存在的祖先（錨點，canonical）加上還要建立的名稱（尾段）。資料夾已存在時尾段是空的，行為與計畫、摘要、快照和線上 frame 都和以前逐位元相同。

## 決定

1. **尾段在路由裡一致套用，不放進 `primary_prefix`；接上尾段之前先驗證路由後的檔案路徑。** 錨點才是 `plan_import_parsed_with` 的 root（`destination_roots`、`op.root_path`），`op.relative_path` 是 `尾段/相對路徑`。命中某個 prefix 的 entry 直接拿 `rest`、絕對路徑 entry 也不會加上 `primary_prefix`，所以尾段若放進 `primary_prefix` 就只套得到其中一種路由。留給檔案本身的路徑要先通過 `sanitize_relative_path`（非空、有檔名）才接尾段，否則 `.` 或 `p/` 會變成目的資料夾本身。
2. **規劃只讀不寫。** `PlacedRoot::resolve` 只查 `symlink_metadata`；沒有東西在規劃時被建立，包含 `--dry-run`。大小寫偵測也只讀（向檔案系統查詢能力，查不出來才用現有項目或資料夾自己的名稱比對，不建探測檔；仍無法確定就當不分大小寫），測試以錨點目錄的 mtime 驗證。
3. **「指定新資料夾」只在 Apply 經同意後、在寫入鎖之下逐層建立。** 指定新資料夾是使用者指名的目的資料夾或導入路徑的尾段（`requested_dirs`，`missing_dirs` 的子集）；檔案路徑裡的中間資料夾（順帶新資料夾）照舊由寫檔時建立、不需同意。`RestoreSelection::confirmed_new_dirs` 必須涵蓋目前勾選且是新增的列實際需要的指定新資料夾，否則 `NewDirsNotConfirmed`；失敗或一個檔都沒寫成時，把這次建立的資料夾由深到淺刪回（有內容的留著）。worker 只建自己重新規劃算出的資料夾，master 送來的同意只用來放行。
   **Apply 時依 `PlacedRoot` 尾段強制同意，與規劃分類無關。** `requested_dirs` 是規劃時讀檔案系統算出的分類（最近既有祖先、`FolderId`、拼法），規劃與 Apply 之間檔案系統會變，分類就可能算錯（#241 第四輪：規劃中途出現的 `new` 讓 `NEW/SUB` 被當成順帶新資料夾）。所以計畫另外帶著每個 `PlacedRoot`（primary 與每個 prefix）resolve 時的尾段原始路徑（`DestinationFreshnessSnapshot::placed_tails`，逐層、排序、去重；沒有尾段時是 `None`，快照與線上 frame 逐位元不變，舊 peer 不帶這個欄位也照舊）。Apply 在寫入鎖之下、任何寫入之前：任何一層現在存在就 `StaleDestination`（`revalidate`，worker 對 master 的快照與自己重新規劃的快照各做一次；worker 重新 resolve 出的尾段必須與預覽記錄的相同）；不存在的層只要有勾選中的新增列的路徑經過它（同一路徑，或以 Apply 當下讀到的錨點物件身分加摺疊後名稱算出的同一個 `FolderId`），就必須在 `confirmed_new_dirs` 裡（同樣以路徑或 `FolderId` 比對，所以 `/private/x/new` 與 `/System/Volumes/Data/private/x/new` 對到同一筆同意），否則整批 `NewDirsNotConfirmed`、什麼都不寫。規劃的分類只決定 UI 與 CLI 列出、請使用者同意的清單，不能單獨放行。
   同一個資料夾的兩種拼法在同一次 Apply 只建一次：`unify` 把走另一個錨點拼法的 create 移到首個拼法所在的目的 root 底下（root 與路徑一起正規化）；仍留下兩種拼法時，mkdir 以物件身分記錄這次建立的資料夾，之後對同一個物件得到 `AlreadyExists` 視為自己建的，其他的 `AlreadyExists` 仍是過期。
4. **freshness 涵蓋寫入鎖鎖住的 repo（#237），指定新資料夾在預覽後出現就算過期。** 錨點一變，落點路徑就變了，所以不像順帶新資料夾那樣容許它後來變成一般資料夾；`revalidate` 對 `requested_dirs` 裡任何現在存在的東西回 `StaleChange::Created`。建立之後再確認它仍是自己 canonical 的位置，擋住 revalidate 和 mkdir 之間被換成 symlink。

## 為什麼用 opt-in 旗標

桌面 App 的行為不變：它永遠不建資料夾。`PasteRoute.allow_new_dirs`（線上是 `PasteMapping.allow_new_dirs`）預設 false；false 時本機規劃走原本的 `CanonicalRootId::new`、worker 照舊拒絕不存在的資料夾。CLI 一律打開它；之後的 GUI 單（確認 modal、導入路徑編輯）在同一個 PR 打開它並把 `PasteTarget::Local.bound` 設成工作區根目錄。

## 其他

- **`PREPARED_IMPORT_VERSION` 不升。** prepared 快取是 worker 行程內的記憶體，線上只多了加法欄位；升版會讓協定 7–8 的 worker 失去 prepared import，是退步。只升 `PROTOCOL_MAX` 到 9，新增 `CREATE_DIRS_VERSION = 9`。協商低於 9 的 worker 照舊拒絕不存在的資料夾；master 只在被拒絕之後確認那個資料夾真的不存在，才回 `WorkerTooOld`，一般的貼上不多一次往返。
- **master 不解析 worker 的路徑。** 計畫要建立哪些資料夾、每一列需要哪幾個，由規劃的那一台依它自己的路徑規則算好，放在 `DestinationFreshnessSnapshot::new_dirs`（`dirs` 由淺到深、各一次；`needs` 依 create 的索引列出它需要的 `dirs` 位置），master 只依勾選的列取出、照順序列給使用者並原樣送回同意，不對 worker 的路徑做 `parent()` 之類的運算：Unix 上 `C:\ws\new\a.txt` 的反斜線是檔名字元，曾讓 Windows worker 的計畫在 Unix master 上列不出資料夾、`--create-dirs` 送出空的同意。只在有尾段（`placed_tails`）時記錄，沒有尾段的快照與 frame 逐位元不變；不進 `matches_preview`（由 digest 涵蓋的計畫與 `missing_dirs` 推得，worker 的 Apply 也以自己重新規劃的計畫重算）。沒有這個欄位時（沒有尾段，或此欄位之前的協定 9 開發版 worker）master 退回依路徑計算，只在兩台的路徑規則相同時正確。
- **工作區外一律不允許**：GUI（`bound`）與 worker（以工作區為界）。CLI 不設界。
- **名稱規則在所有平台一致**：尾段拒絕 `..`、`.git`、`sanitize_relative_path` 擋的字元、結尾是點或空白的名稱與 Windows 保留名稱，讓同一個指令在哪台機器上都得到同一個結果。已存在的資料夾不受影響，因為規則只檢查要建立的名稱。
- 新資料夾不能建進 Git 目錄（`PlacedRoot::resolve` 與 Apply 各檢查一次）。
- 它取代 `docs/unified-engine-plan.md` D4 的「GUI 第一步不改，另開單決定」中關於落點的部分：落點差異改由 `PlacedRoot` 在引擎裡解決。
- commit 貼上不變：`plan_commits` 與 worker 的 `replay_root` 仍要求目的資料夾已存在。
