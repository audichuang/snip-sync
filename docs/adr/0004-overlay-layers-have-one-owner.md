# 浮層與內嵌層只有一個擁有者

桌面 App 的浮層（工作區選單、Log 下拉選單、repo／ref 選擇器、右鍵選單）與內嵌層（搜尋／跳行列、加入 repo 輸入框）的開關狀態，只存在 `overlay::OverlayStack`；它的欄位是私有的，唯一的入口是 `open_overlay`、`toggle_overlay`、`close_overlay`、`drop_overlays`。共用的規則寫在那一個模組，不再由每個開啟點、關閉點各自重複。

## 規則

- **互斥。** 浮層同時最多一個，型別保證（一個 `Option`）。開浮層取代舊的；開內嵌層也關掉浮層。內嵌層不受浮層影響。
- **Escape 只有一條根規則。** 沒有任何窗格或輸入框綁不帶修飾鍵的 Escape（`K1` 鎖住 keymap）。鍵盤攔截器對每一次不帶修飾鍵的 Escape 呼叫 `WorkbenchModel::on_escape`：
  1. 焦點所在的輸入框正在用輸入法組字，讓給輸入法（`marked_range()`）。
  2. 有浮層，就關它。
  3. 否則依 `FocusTarget` 窮舉決定：搜尋列與其輸入框關搜尋列、加入 repo 欄位關欄位、Log 搜尋框清空、閱讀器先清選取再關搜尋列、Project／Changes／Log 先結束 speed search 再回閱讀器、貼上面板取消貼上。
- **再按一次觸發鈕要關。** 每個浮層記得是哪個控制項開的（`TriggerId`）；面板的外部點擊遇到自己的觸發鈕就放過，留給觸發鈕的 click 去 toggle。三種舊作法（bounds cell、400 ms 實際時鐘、什麼都不做）合成這一種。
- **關閉後焦點歸位只有一條規則。** 每個浮層在開啟時決定 `return_to`：右鍵選單回來源窗格，Log 下拉回 Log，搜尋列回閱讀器；選擇器、工作區選單與加入 repo 欄位回到開啟它的地方（沒有視窗的開啟路徑回根）。關閉只排一個待歸位，下一幀在套用 `pending_focus` 之前處理：焦點在該層的 handle 上、在根、或沒有焦點時才歸位；外部點擊已經把焦點交給別的窗格時不搶回。新層取代舊層時，若焦點在舊層裡，新層繼承舊層的歸位目標，並撤銷舊層排好的歸位。
- **切分頁關掉浮層，內嵌層留著。** 浮層靠指標位置與整幀版面運作，不屬於背景分頁；它擁有的主機瀏覽（`remote.browse`）一起結束。焦點在浮層裡時，切回分頁落到該層的歸位目標。
- **每個成員的清理只在 `teardown` 一處**（窮舉 match）：主機瀏覽的 `end_browse`、日誌行、搜尋列的查詢與符合清單。

## 輸入法的取捨

組字中按 Escape 讓給輸入法。代價是組字時要按兩次 Escape 才關得掉選單；好處是不吞掉組字，而且 `scripts/check_native_ime.py` 的 escape 階段（在 `log-search-input` 組字時按 Escape，不得印出 `LOG_SEARCH`）要求如此。

## Consequences

- 刪掉 `workspace_menu`、`workspace_picker`、`workspace_menu_button`、`log_menu`、`log_menu_dismissed`、`popover`、`popover_cursor`、`Chrome.menu`、`Reader.find_open`、`is_adding_repo`：直接寫入的程式碼編譯不過。`TextInput` 沒有 `Dismiss` 事件與 Escape 綁定，`CancelPaste`、`ReaderClear`、`MenuCancel`、`FocusEditor` 這幾個 action 也不存在。
- `teardown`、`on_open`、`is_floating`、`panel_selector`、`FocusTarget::context`、`FieldId` 都是窮舉 match，新增種類、輸入框或 key context 不補分支就編譯不過；`every_keymap_context_has_a_focus_target` 從 `key_bindings()` 推導 context，新增 context 卻沒有 `FocusTarget` 時失敗。
- `text_inputs()` 是輸入框的唯一清單，`clear_workspace_inputs` 用它，所以 `remote_path_input` 也會在換工作區時清掉。
- `OverlayStack::check_invariants`（O1–O5）以 `eval(id, model, cx) -> Option<String>` 的形狀提供，供 #230 的不變式目錄直接包裝；面板有沒有畫出來要靠 `debug_bounds`，由測試端的 `assert_overlay_panels_match` 檢查，不放進 `eval`。
- 日誌行（`SELECTOR_OPEN`／`SELECTOR_CLOSED`、`MENU_OPEN`／`MENU_CLOSED`、`LOG_MENU: Some(<k>)`、只在再按觸發鈕時才有的 `LOG_MENU: None`、`FIND_BAR: open`／`closed`）格式不變，驅動程式與真實 UI 規程都在讀；只會多印，例如切分頁丟掉右鍵選單時的 `MENU_CLOSED`。
- 貼上面板不在 stack 內（它取代 center 區，開關由 `PastePreview` 擁有），但納入 Escape 根規則。
