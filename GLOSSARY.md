# snip-sync

在電腦之間以剪貼簿搬運檔案與 Git commit 的工具；桌面 App 與 CLI 是同一套引擎的兩個前端。

## 工作台

**工作區**（workspace）:
開啟中的一個資料夾，可能在本機，也可能是某台 SSH 主機上的資料夾。本機以 canonical path 識別；遠端以 SSH Host 別名加 worker 解析後的路徑識別，指向同一台機器的兩個別名算兩個工作區。
_Avoid_: 專案、project

**工作區分頁**（workspace tab）:
主視窗頂端的一個分頁，恰好顯示一個工作區；同一個工作區最多只有一個分頁。
_Avoid_: 分頁（單獨使用，易與工具視窗混淆）、視窗

**工具視窗**（tool window）:
工作區分頁內的 Project、Changes、Log 等面板。
_Avoid_: tab、分頁

**浮層**（floating layer）:
錨定在窗格之上的面板：工作區選單、Log 下拉選單、repo／ref 選擇器、右鍵選單。同時最多開一個，開新的會取代舊的，分頁切走時它關掉。開著時，所有 tooltip 不顯示。
_Avoid_: popover、popup、dropdown（泛指時）、menu（單獨使用，易與右鍵選單混淆）

**內嵌層**（inline layer）:
屬於某個窗格的一塊介面：閱讀器的搜尋／跳行列、Project 窗格的加入 repo 輸入框、貼上預覽面板的目的資料夾輸入框。開浮層不會關它，分頁切走時它留著，它開著時 tooltip 照常顯示。
_Avoid_: 工具列、bar（泛指時）

**可辨識下限**（readable floor）:
一段文字至少要看得到的 grapheme 數，由 `FitRole::rule()` 決定（subject 與 ref 標籤 6、作者 3、chip 的鍵加值 4 個字；日期整段）。空間不夠時，文字被截到這個下限為止，再不夠就整個 chip、整欄或整組標籤退出，不再截。
_Avoid_: 最小寬度、幾 px

**探索軸**（exploration axis）:
dogfood 與 gpui 矩陣測試共用的固定維度：視窗寬度（含縮放路徑）、語系、Log 欄位（hash 與窗格）、資料形狀、狀態。每輪 dogfood 依軸走並交一張覆蓋表，定義見 `docs/ui-exploration-axes.md`。
_Avoid_: 隨便看看、也看大視窗

## 貼上

**貼上目標**（paste target）:
貼上規劃與寫入的地方：本機的一個資料夾，或遠端工作區 worker 上的一個資料夾。CLI 與桌面 App 都只經它規劃、重驗與套用；本機目標套用保留下來的計畫，遠端目標由 worker 在自己的磁碟上重新規劃後寫入。
_Avoid_: 貼上目的地（單獨使用，易與目的資料夾混淆）、paste destination

**目的資料夾**（destination folder）:
這次貼上的落點資料夾，是貼上目標的資料夾部分：CLI 的 `--repo`（遠端是 `--in`），桌面 App 在預覽面板的目的資料夾列選擇。CLI 的目的資料夾可以還不存在；不存在時規劃只讀不寫，Apply 經使用者同意才建立。
_Avoid_: 目標資料夾、貼上目的地（單獨使用）、還原目的目錄

**導入路徑**（import path）:
把剪貼簿裡某個 prefix 底下的 entry 送到別的資料夾的對應（CLI 的 `--map PREFIX=PATH`）。和目的資料夾一樣可以還不存在。
_Avoid_: 對應資料夾、mapping folder

**錨點與尾段**（anchor, tail）:
落點（`PlacedRoot`）的兩部分：錨點是最近一個已存在的祖先（canonical），尾段是它底下還要建立的名稱。資料夾已存在時尾段是空的。尾段在每一種路由裡都接在錨點後面、檔案相對路徑前面。
_Avoid_: base、missing parts

**指定新資料夾**（requested folder）與**順帶新資料夾**（incidental folder）:
還不存在、要在 Apply 建立的資料夾分兩種。指定新資料夾是使用者指名的目的資料夾或導入路徑的尾段（`requested_dirs`），只在 Apply 經同意後、在寫入鎖之下建立；同意的單位是它所屬的 `PlacedRoot`（`confirmed_placed`，計畫裡的位置），不是路徑或名稱。順帶新資料夾是檔案路徑隱含的中間資料夾，在 Apply 寫入內容前逐層建立、不需同意；它若以另一個名稱建出了未同意的指定新資料夾，整批拒絕。
_Avoid_: 新資料夾（單獨使用，分不出需不需要同意）

## 錯誤

**原因**（Reason）:
一個失敗的型別化意義，是資料而不是文字：變體加上必要的資料（路徑、細節）。桌面 App 只經 `reason_msg` 把它譯成目前介面語言；CLI 與線上協定的文字來自 thiserror 的 Display。上線前由 `beside_code` 清掉絕對路徑與 git stderr。自由文字欄位是 `Datum`：只能由說出來源的建構子產生（OS、git、ssh、舊 peer 的文字或 bug 回報標籤），我們自己的句子進不去。
_Avoid_: 錯誤字串、error message（當成資料讀取時）

**錯誤碼**（ErrorCode）:
線上拒絕的封閉分類，拼法固定。新的意義加在 `Reason`，不加在 `ErrorCode`。
_Avoid_: 用錯誤碼表達細節

## 驗證

**不變式**（Invariant）:
桌面模型裡兩個檢視對同一件事必須一致的條件，登記在 `crates/desktop-native/src/invariants.rs`，由同一個 `eval` 判定。任何時刻都成立的叫 A，等載入全部結束才成立的叫 Q。有 probes 的視窗每畫一幀就印一行 `[APP:INVARIANT_BROKEN]`，隨機動作 lane 在每個手勢後檢查它。已知違反由某個 issue 負責的，登記為 `Pending(issue)`，不靠動作序列豁免。
_Avoid_: 斷言、檢查（泛指時）

**故障規則**（FaultRule）:
測試給 worker 的一條指令：哪一種請求（可以限定工作區）、略過幾個、作用幾次、做什麼（拒絕、斷線、截半回覆、節流、先沉默……）。只有測試能啟動，`snip serve --stdio` 沒有任何入口。heartbeat 永遠不受影響，故障只作用在答覆。
_Avoid_: mock、stub
