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

## 貼上

**貼上目標**（paste target）:
貼上規劃與寫入的地方：本機的一個資料夾，或遠端工作區 worker 上的一個資料夾。CLI 與桌面 App 都只經它規劃、重驗與套用；本機目標套用保留下來的計畫，遠端目標由 worker 在自己的磁碟上重新規劃後寫入。
_Avoid_: 貼上目的地（單獨使用，易與目的資料夾混淆）、paste destination

## 錯誤

**原因**（Reason）:
一個失敗的型別化意義，是資料而不是文字：變體加上必要的資料（路徑、細節）。桌面 App 只經 `reason_msg` 把它譯成目前介面語言；CLI 與線上協定的文字來自 thiserror 的 Display。上線前由 `beside_code` 清掉絕對路徑與 git stderr。
_Avoid_: 錯誤字串、error message（當成資料讀取時）

**錯誤碼**（ErrorCode）:
線上拒絕的封閉分類，拼法固定。新的意義加在 `Reason`，不加在 `ErrorCode`。
_Avoid_: 用錯誤碼表達細節
