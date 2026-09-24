# snip-cli(指令 `snip`)

`snip-core` 的命令列外殼。所有邏輯都在 core;這裡只處理參數、輸出文字與結束碼。

## 指令

```bash
snip copy <files…>            # 複製檔案或資料夾目前的內容
snip copy --working           # 未提交的異動(working tree、untracked、index)
snip copy --staged            # 只有 staged 的內容
snip copy --commit <SHA>      # 單一 commit 的異動
snip copy --range <A..B>      # 兩個 revision 端點的比較
snip copy --commits -n 3      # commit 模式:HEAD 往前 3 個 commit(或 --commits A..B)
snip paste --dry-run          # 列出會發生什麼(模式自動判斷)
snip paste --apply            # 執行還原
```

共用選項:`--repo <dir>`(預設 `.`)、`--settings <JSON 或檔案路徑>`。完整說明請看 `snip --help`。

## 規範

- **結束碼:** 0 成功、1 失敗、2 用法錯誤。
- 訊息文字比照 VS Code 擴充(`extension.ts`、`notify.ts`),與桌面 App 的英文訊息保持一致。
- 新功能先在 `snip-core` 實作,CLI 只加參數與輸出;不要在這裡寫第二份邏輯。
- Linux 寫剪貼簿時會 re-exec 自己成為背景 daemon(`__snip_clipboard_daemon`),讓內容在指令結束後仍然存在。

## 測試

- `tests/cli.rs`:參數解析與輸出。
- `tests/e2e.rs`:建立真的 git repo,跑 `snip copy` / `snip paste` 來回。需要剪貼簿的案例遵守 `SNIP_REQUIRE_ALL_TESTS` 規則(見 `crates/core/README.md`)。
