# UI 探索軸：dogfood 與 real-UI 回合固定要走的維度

目的：Log 一類「文字搶寬度」的錯誤，過去都是在某個沒人看的組合才出現（1080 寬 hash 開、放寬沒有輸入、兩顆 chip 同時啟用）。隨便看看會漏，所以 dogfood 與 real-UI 回合依下面五條固定的軸走，每輪交一張覆蓋表，說明每一格看了沒有、結果是什麼。

同一份軸也是 gpui 矩陣測試（`crates/desktop-native/src/main.rs` 的 `tests::in_process::fit_matrix`，#229）的格子。矩陣在每次 CI 都跑，dogfood 專門負責矩陣碰不到的部分（見最後一節）。

判定一律依 `FitRole::rule()`（`docs/adr/0005-one-fit-rule-table.md`）與 `[APP:TEXT_FIT: ...]` 行，不寫任何像素數字：

- `python3 scripts/real_ui_round.py text-fit <日誌> --check` 通過：沒有 `TEXT_FIT_DRIFT`、沒有 `clipped=1`、每段文字畫出來的字數符合它的規則。
- 日期（`:date`）的 `cut=0`。
- 視窗寬 ≥1080 時，有 ref 的列都有標籤（`:branch` 或 `:tag` 的 `fit>0`）。
- 900 寬時作者欄與整組標籤依設計可以被捨棄（`fit=0`）；收合的 merge 列上的「已收合 N」讓位給標籤。這兩種不判失敗。

## A 軸：視窗

| 尺寸 | 用途 |
|---|---|
| 900×600 | 驗收規格的最小寬度，作者欄與標籤的取捨在這裡發生 |
| 1080×720 | 預設的筆電寬度，「≥1080 一定有標籤」的下界 |
| 1440×900 | 寬螢幕，分支窗格與詳情窗格都開著 |

用 `python3 scripts/real_ui_round.py resize W H --run "$RUN"` 調整，等到 `[APP:VIEWPORT: ...]` 相符。

另有兩條縮放路徑，每一步之後不碰滑鼠、不按鍵，先截圖，再跑 `text-fit --check`：

1. 1440×900 → 1080×720 → 900×600 → 1080×720 → 1440×900。
2. 1080×720 → 1600×900（跨過 `log_branches_w` 的 1200 門檻）。

縮放後的畫面必須和直接開在那個寬度的畫面一致（同一段文字有沒有出現、畫出幾個字）。這是矩陣的 I6。

## B 軸：語系

每種尺寸在繁中與英文下各看一次。切換用 Option+L，日誌看 `[APP:LOCALE: ...]` 確認真的換了。日期格式與按鈕文字的寬度因語系而不同，同一個寬度下標籤有沒有出現可以不同（ADR 0005 的 Consequences），所以不要求兩種語系的結果相同，只要求各自符合規則。

## C 軸：Log 欄位

經 `btn-log-more` 開關，每個組合都看：

- hash 欄：關（預設）／開（`log-more:hash`）。
- 詳情窗格：開（預設）／關（`log-more:details`）。
- 分支窗格：開（預設）／關（`log-more:branches`）。

至少走這四組：預設、hash 開、兩個窗格都關（列表最寬）、hash 開加兩個窗格都關。

## D 軸：資料形狀

fixture 用 `docs/real-ui-operator-fixture.sh` 的 gate-b、`perf15`（多 repo），遠端工作區照 `docs/remote-real-ui-protocol.md`。資料裡一定要包含下列形狀，缺的形狀在一個拋棄式 repo 上用 `git` 補出來（作者與日期用 `git commit --author` 與 `GIT_AUTHOR_DATE`／`GIT_COMMITTER_DATE`）：

- 中文 subject、中文 tag、中文作者。
- 長分支名（例如 `feature/a-rather-long-branch-name-for-labels`）和長 tag（20 個字以上，窄寬度時會被截）。
- 5 個以上 tag 掛在同一個 commit（會出現 `+N`）。
- 收合的 merge（merge 列的「已收合 N」）。
- 只屬於其中一個 repo 的分支（分支窗格與選單的 repo 後綴）。
- 日期含 `10/24`、`04:44` 這類寬數字的 commit（比例寬度字型的數字寬度不同）。

## E 軸：狀態

- 啟用中的篩選 chip：0、1、2、3 個。要包含「長 repo 名加短分支名」這一對（短的值先完整）。
- 選取 commit（詳情窗格有內容）。
- 浮層開著：篩選選單、分支選單、「更多」選單。
- hover（滑鼠停在列上）。

## 主題

只在 1080×720 用 light 走一次，其餘用預設主題。

## 覆蓋表

每輪回合交一張表，列是 A 軸的尺寸與縮放路徑，欄是 B、C、D、E 的組合，每格填 `pass`、`fail`、`not-run`，`fail` 與 `not-run` 附原因與日誌路徑。issue 草稿多兩個欄位：

- **軸座標**：A 到 E 各是什麼值。
- **矩陣缺口**：這一格矩陣有沒有；有的話為什麼沒抓到。

矩陣應該抓到卻漏掉的，修正的 PR 一定要補上矩陣的格子或 fixture 的列，而且那一格在修正前必須失敗。

## 矩陣已涵蓋的格子

測試用真實字寬與內嵌字型（macOS 的 CoreText、Linux 的 cosmic-text；Windows 沒有，跳過）。fixture 是 Log 的單一 repo（含中文 subject／作者／tag、長分支、5 個 tag、收合的 merge、長 tag）與兩個 repo 加兩顆啟用中 chip 的組合。

| 測試 | 涵蓋 |
|---|---|
| `fit_rows_w{0900,1080,1440}_hash_{off,on}_panes_{default,listonly}`（12 格） | A 軸三種尺寸 × B 軸兩種語系 × C 軸 hash 與窗格，檢查規則、不裁切、不重疊、出現規則；預設窗格再點分支窗格的 ref 看標頭提示 |
| `fit_chips_w{0900,1080,1440}` | A、B 軸 × E 軸兩顆啟用中的 chip、分支窗格與分支選單的 repo 後綴 |
| `fit_rows_follow_a_resize_walk_hash_{off,on}` | A 軸第一條縮放路徑，並檢查寬度單調 |
| `fit_rows_follow_a_widening_1080_to_1600_hash_{off,on}` | A 軸第二條縮放路徑 |
| `fit_matrix_cells_cover_every_axis_combination`、`every_fit_role_names_the_cell_that_draws_it` | 格子等於三軸的笛卡兒積；每個 `FitRole` 都指到一格，那一格真的畫出它 |

dogfood 專門負責矩陣碰不到的部分：

- Retina 縮放（`scale != 1`）。
- 真實的 hover 與 tooltip（浮層開著時 tooltip 不顯示）。
- PingFang 與 Noto 的 CJK 字寬差異。
- 連續拖曳縮放（不是一步到位）。
- 遠端工作區的資料。
- 選單與浮層的位置（貼邊、翻轉）。
- 主題。
