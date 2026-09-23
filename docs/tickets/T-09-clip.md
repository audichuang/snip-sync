# T-09 clip:系統剪貼簿

## 目標
`crates/core/src/clip.rs`:以 `arboard` 讀寫系統剪貼簿,CLI 與 App 共用。

## 範圍
- `read_text() -> Result<String>`、`write_text(&str) -> Result<()>`。
- Linux X11:擁有剪貼簿的程序結束後內容會消失,CLI 需要 `write_text_and_wait`(`arboard::SetExtLinux::wait()`)在背景等到被取走;App 常駐不需要。
- `detect_mode(text) -> Mode { Commits, Files }`:以 `commits::is_commit_payload` 的 marker 判斷(T-08 未合併時,這裡直接比對 marker 字串常數,常數定義在本檔並由 T-08 引用)。
- 測試:沒有顯示環境(CI 無 `DISPLAY` / `WAYLAND_DISPLAY`)時跳過真實剪貼簿測試並印出原因;`detect_mode` 的單元測試一定要跑。

## 只可修改
`crates/core/src/clip.rs`。

## 驗收
`cargo test -p snip-core --lib clip` 全綠。
