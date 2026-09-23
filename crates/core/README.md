# snip-core

CLI(`crates/cli`)與桌面 App(`crates/desktop/src-tauri`)共用的全部邏輯。
兩個外殼只負責收參數、顯示結果;能被兩邊共用的行為一律寫在這裡。

## 模組

| 模組 | 職責 | 對應的 TS(ClipCodeVSCode `0aa24c8`) |
|---|---|---|
| `format` | 檔案模式的線上格式:建立與解析剪貼簿內容 | `clipboardFormat.ts` |
| `copy` | 收集檔案、組出 payload | `copy.ts` |
| `gitsrc` | 從 git 讀異動檔案與 diff(直接呼叫 git plumbing) | `gitCopy.ts`、`gitHistory.ts`、`graphCopy.ts`、`catFile.ts` |
| `filter` | 忽略規則、二進位與大小限制 | `filterMatcher.ts` |
| `restore` | 把解析結果轉成檔案動作並執行 | `restore.ts`、`restoreBase.ts` |
| `paths` | 路徑正規化、還原目標的根目錄解析與越界檢查 | `pathResolver.ts` |
| `fsutil` | 讀檔、編碼判斷、安全寫入 | `fileSystem.ts` |
| `settings` | 使用者設定(JSON 欄位名與 TS 相同) | `settings.ts` |
| `stats` | 字元 / 行 / token 統計 | `copy.ts` 的 `payloadStats` |
| `commits` | commit 模式:序列化與重播一段 commit | 無(snip-sync 專有) |
| `clip` | 系統剪貼簿存取與模式判斷 | 無 |

## 規範

- **檔案模式必須與 TS 逐位元組一致。** 改 `format`、`copy`、`filter`、`restore`、`paths`、`fsutil`、`stats`
  之前先讀 `.ts-ref/` 裡對應的 TS。刻意的差異要寫進 `docs/porting-notes.md` 的「已知且接受的差異」。
- `fixtures/clipboard-contract.json` 由 ClipCodeVSCode 擁有,SHA 釘在 `tests/contract.rs`,不可在這裡修改。
- 同步 API,不用 async;長時間的工作由呼叫端決定要不要丟到背景執行緒。
- 錯誤用 `thiserror` 定義的型別,或回傳給使用者看的 `String` 訊息;不 `panic`、不 `unwrap` 使用者輸入。
- 檔案內容只接受嚴格 UTF-8(`fsutil::read_text_file`),不做有損解碼,否則還原時會寫回亂碼。
- 路徑在剪貼簿內一律是斜線字串;只有解析出來的還原目標和越界檢查才碰原生路徑。
- 給前端用的型別加 `#[derive(TS)]`,改完到 `crates/desktop` 執行 `bun run generate:dto && bun run format`。
- 禁止 `unsafe`(workspace lint `unsafe_code = "forbid"`)。

## 測試

- 單元測試放在各模組檔尾的 `#[cfg(test)]`。
- `tests/contract.rs`:跑 ClipCodeVSCode 的契約 fixture,確認輸出與 TS 相同。
- `tests/clipboard_roundtrip.rs`:真的讀寫系統剪貼簿,Linux 要在 `xvfb-run` 下執行。
- 缺環境(顯示器、node、`.ts-ref`)時要跳過的測試,跳過前必須先 `assert!(std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(), …)`。
- 跟路徑有關的程式碼,要在 Linux 上模擬其他平台的情況(例如用 symlink 模擬 macOS 的 `/var` → `/private/var`)。
