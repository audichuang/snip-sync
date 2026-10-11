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
