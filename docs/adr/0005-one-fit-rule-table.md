# Log 工具視窗的文字寬度由一張規則表決定，由一個求解器分配

Log 工具視窗裡一段文字和別的文字搶寬度時（commit 列的 subject、ref 標籤、作者、日期、hash，篩選 chip，分支窗格與分支選單的名稱和 repo 後綴，標頭的 ref 提示），誰先讓、讓到哪裡，只由 `ui/fit.rs` 的 `FitRole::rule()` 規則表和 `fit_row` 求解器決定。畫面上的每一段文字由一個 `FitCell` 畫出，它依求解器給的寬度加省略號，並向 `TEXT_FIT` 探針報告畫出來的結果。

原因：此前一列的寬度由四層各自決定，互不知道對方：清單一次算 gutter、作者、日期三欄（`list_cols`），每列再算一次標籤與 subject（`row_widths`），標籤容器裡的 flex 用 `flex_shrink = 1000` 的權重再分一次，最後列的元素樹畫出來沒有任何程式檢查它和前面是否一致。每張 issue 只在某一層加一個常數（`LABEL_FLOOR_W`、`MIN_SUBJECT_BESIDE_LABELS`、`MIN_DATE_W`、`flex_shrink = 1000`），規則不是全序而是循環：1080 寬有標籤時標籤勝過 subject（#195）；日期的需求勝過標籤（#217）；900 寬沒有標籤時 subject 的 160 勝過日期。輸入還是上一幀的量測，只有篩選 chip 列在寬度變了時排重畫，所以清單與側邊窗格在視窗放寬、縮窄後停在舊版面。

## 規則表

`FitRole::rule()`，窮舉 match，字數一律以 grapheme 計：

| 角色 | 規則 | 出現 |
|---|---|---|
| `Subject` | `Readable(6)` | 一定有 |
| `BranchLabel`、`TagLabel` | `Readable(6)` | 整組出現或整組消失；≥1080 一定出現 |
| `RefMore`（`+N`） | `Whole` | 隨標籤組 |
| `Collapsed`（「已收合 N」） | `Whole` | 只在收合的 merge 列；放不下時整段消失（`Optional`），先於標籤組 |
| `Author` | `Readable(3)` | 整欄可以隱藏 |
| `Date` | `Whole` | 一定有 |
| `Hash` | `Whole` | 只在 hash 開時 |
| `ChipRepo`、`ChipBranch`、`ChipUser`、`ChipDate` | `KeyValue(4)` | 放不下就收進「更多」 |
| `ChipMore` | `Whole` | 只在有 chip 被收起時 |
| `RefHint`（標頭「記錄: <ref>」） | `KeyValue(6)` | 讓位給右側按鈕 |
| `PaneGroup`、`PaneRefName`、`MenuRefName` | `Readable(6)` | 一定有 |
| `PaneRepo`、`MenuRepo`（repo 後綴） | `Whole` | 整段或沒有 |

- `Readable(n)` 的下限寬是 chrome（標籤的圖示與間隔、chip 的內距與圖示）加「前 n 個 grapheme 加 `…`」的量測寬再加 1px；文字不超過 n 個 grapheme 時，下限就是整段。`KeyValue(n)` 的鍵一定完整，值至少 n 個 grapheme。`Whole` 的下限等於自然寬。
- 那 1px（`FIT_MARGIN`）對齊 gpui 的省略號：它逐字元加總寬度，加總後捨去小數仍大於盒寬才截（`LineWrapper::truncate_line`），所以量出的整形寬度剛好放進去的盒子，仍可能被截。每個自然寬與下限寬都含這 1px。

## 全序

一句話：日期一定完整；subject 先拿到可辨識下限；標籤組排在作者欄之前，所以作者欄先隱藏，標籤再整組捨棄；收合 chip 排在標籤組與作者欄之後，放不下時它先消失，不讓一列為了它丟掉標籤、也不讓整份清單為了一列的說明文字失去作者欄；之後短的識別字先完整，再給 subject 舒適寬（160），再給作者（120）、日期（118）、gutter。

清單層與列層各算一次：

- **清單層**（`fit_list`）只用「最壞的一列」：標籤組取下限總和最大的那組，收合 chip 取最寬的，作者下限取所有不重複作者名的最大值，subject 的下限取「漢」×6 與「W」×6 加省略號兩者較寬的上界。輸出 gutter、作者、日期三欄的寬度，作者與日期可以是 `None`（整欄不出現）。
- **列層**（`fit_log_row`）把清單層的欄寬釘住，每列再算自己的 subject、收合 chip、標籤組。清單層放得下的最壞組，每一列都放得下。

求解器的語意：`Atomic` 的一組全部以下限顯示，或全部 `dropped`、之後任何 claim 都不給；`Grow` 長到目標，可以只拿到一部分；`ShortestFirst` 讓短的先長到自然寬；`Rest` 拿剩下的全部。寬度永遠不超過自然寬，只有顯示中的 part 才付它的間隔。

篩選 chip 用同一個求解器：全部放得下就全部整段；否則先給「更多」，有值的 chip 依自然寬由短到長各自拿下限，再由短到長長到自然寬，沒有值的 chip 最後，也最先進「更多」。分支窗格與分支選單的列先給名稱下限，再給名稱整段，之後後綴才整段出現或不出現；分支選單的面板寬度在 render 時由各列自然寬決定（200–360）。

## TEXT_FIT 是執行期的對帳

`[APP:TEXT_FIT: id=text-fit:<scope>:<role> rule= natural= floor= fit= drawn= visible= clipped= cut= shown=/]` 在內容改變時印一行。`visible` 是盒子與 `window.content_mask()`（所有祖先裁切框的交集）的橫向交集，所以 `drift`（`TEXT_FIT_DRIFT`：畫出來的盒子與求解器給的寬度差超過 1px，代表有一層繞過了求解器）與 `clipped`（祖先裁掉了它）是兩件事。`#[gpui::test]` 與 native-e2e smoke 斷言兩者都沒有，`scripts/real_ui_round.py text-fit --check` 在真實 UI 的日誌上做同一件事。

## 量測與繪製用同一個文字樣式

字型家族、字級、字重只在一處定義：`FitRole::style_for(own)`（`FitStyle`）。`FitCell::render` 把它的三個值都釘在盒子上（所以祖先設的字重、字級不會漏進文字），下限（`floor_px`）、自然寬、清單層的作者欄下限與 `TEXT_FIT` 的 `shown` 都用同一個樣式量。唯一不是 `Regular` 的是自己的作者名稱（`Author` 且 `own`，semibold）：`FitCell::own(true)` 帶著它，清單層算作者欄下限時每個作者名也依它是不是自己的來量（快取鍵含這一點，`git_user_email` 在第一批列之後才到）。畫完後探針再從 `window.text_style()` 讀 gpui 實際用的樣式，不同就印 `[APP:TEXT_FIT_STYLE_DRIFT: id= measured= drawn=]`（`style_drift`；`shown` 以實際繪製的樣式計，不被量測樣式掩蓋），與寬度的 drift 一樣在 `#[gpui::test]`、native-e2e 與 `real_ui_round.py text-fit --check` 都算失敗。真實字型矩陣的 fixture 有一列是使用者本人、名稱是 56 個 W，並用獨立的 semibold 整形數字檢查作者欄在 1080 寬、預設窗格時仍留得下 3 個 grapheme。

重畫只有一個機制（`refresh_if_stale`）：這一幀依據的寬和量到的寬不同時排一次重畫，每個新寬度最多 3 次（`Window::refresh` 在繪製中是 no-op，所以排在這一幀結束之後）。面板寬、清單寬、篩選 chip 列都走它。

## Consequences

- 900 寬時作者欄常常隱藏；作者欄的出現不是寬度的單調函數（標籤組比作者先拿）。
- 設計取捨：使用者定案「≥1080 寬一定要有標籤」。收合的 merge 列在 1080、hash 開時，subject 的 6 字下限、完整日期、整段的「已收合 N」與標籤組的下限空間上不可能同時成立（清單 574，英文「+3 collapsed nodes」自然寬約 130），所以讓「已收合 N」讓位：標籤組排在它前面（清單層與列層都是），清單層的作者欄下限也排在它前面（它只是 gutter 裡 `+` 的說明，不該比一整欄還重要，也不該讓一列合併把每一列的作者欄都擠掉）。收合與展開的開關 `collapse:<key>` 在 gutter 裡，不受影響，消失的只是旁邊的說明文字。代價是 1080 hash 開時，帶 ref 的收合 merge 列沒有「已收合 N」；沒有 ref 的收合列照常顯示。
- 繁中與英文的標籤有無仍可能不同，因為日期的寬度不同。這是刻意的，所以沒有「標籤出現與否和語系無關」這條規則。
- 規程只判規則，不判像素：T36、B43 的通過線是 `text-fit --check` 與規則，不再寫 574、160、64 這類實作常數。
- `scripts/check_text_fit.sh` 擋下重新加權重的做法（`flex_shrink = Some(..)`、已退役的常數與函式名稱）、Log 檔案裡沒有 `FIT-EXEMPT: <理由>` 的 `clip_text`／`fill_text`／`.text_ellipsis()`／`.line_clamp(`，以及 `FIT-ROW-BEGIN`／`FIT-ROW-END` 之間的 `.flex_1()`、`.flex_grow()`、`.min_w_0()`、`.flex_shrink(`，和 `.font_weight(`、`.font_family(`、`.text_size(`、`.font_style(`、`.italic(`、`.text_xs/sm/lg/xl(`（樣式只來自 `FitRole::style_for`，在 render 之後再加就是量一個樣式、畫另一個）；四個區段標記（`log_row`、`branch_pane_row`、`branch_menu_row`、`log_filter_chips`）各要恰好出現一次，刪掉標記也躲不掉。哪些行算測試碼只有一份判定（`scripts/lib/test_code.sh`）。
- 範圍外：詳情窗格的檔名（以 `FIT-EXEMPT` 標記）、其他下拉選單、垂直方向「半列可見」的裁切（#197）。
