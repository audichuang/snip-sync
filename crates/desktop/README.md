# snip-sync 桌面 App

Tauri 2 + React 19 + HeroUI v3。常駐系統匣,複製與貼上都在這裡完成。

| 目錄              | 內容                                                 |
| ----------------- | ---------------------------------------------------- |
| `src-tauri/`      | Rust 後端:系統匣、單一實例、開機啟動、Tauri commands |
| `src/`            | 前端(React)                                          |
| `src/lib/`        | 不碰 UI 的純邏輯,各有 `*.test.ts`                    |
| `src/components/` | 畫面元件                                             |
| `src/generated/`  | 由 Rust `#[derive(TS)]` 產生的 DTO,**不可手改**      |
| `e2e/`            | 真實 App 情境測試(tauri-driver)                      |

## 規範

### 後端(`src-tauri`)

- `src-tauri/src/commands/` 的 command 保持輕薄:只呼叫 `snip-core`、搬移資料,不放邏輯。
  CLI 也需要的邏輯放 `crates/core`。
- 改了任何 `#[derive(TS)]` 型別,執行 `bun run generate:dto && bun run format`,並 commit `src/generated/`。

### 前端(`src`)

- **型別:** TypeScript 7 原生版(`@typescript/native`)。`tsconfig.json` 在 `strict` 之外再開
  `noUncheckedIndexedAccess`、`exactOptionalPropertyTypes`、`noPropertyAccessFromIndexSignature`、
  `verbatimModuleSyntax`、`erasableSyntaxOnly` 等全部額外檢查。
  單元測試用 `node --experimental-strip-types` 直接跑 `.ts`,所以只能用可被擦除的語法(不能用 `enum`、`namespace`、參數屬性)。
- **Lint:** oxlint(Rust 寫的),設定在 `.oxlintrc.json`。`correctness`、`suspicious`、`perf`、`pedantic` 都是 error,CI 再加 `--max-warnings 0`。
  關掉的規則都在設定檔裡寫了原因。個別誤報用 `// oxlint-disable-next-line <rule> -- <原因>`,一定要寫原因。
- **格式:** prettier。
- **HeroUI v3** 與舊版差異很大:寫元件時照 `https://v3.heroui.com/docs/react/…` 的最新文件,不要憑記憶。
- **i18n:** 字串全部走 `src/lib/locales/`。`en` 和 `zh-Hant` 的 key 與 `{{placeholder}}` 必須一致(`i18n.test.ts` 會檢查)。
  純邏輯函式收 `Translate` 型別的 `t`,不直接依賴 react-i18next。
- 不包 `<StrictMode>`:它的 ref callback 會掛載兩次,讓 `@pierre/diffs` 的 PatchDiff 變成空的 `<pre>`。
- E2E 會用到的控制項要加 `data-testid`。

### E2E(`e2e/`)

- 每個情境自己建立新的 git repo,透過 tauri-driver 操作真的 App,最後用 git 驗證結果。
- 新的 UI 行為或指令要在 `e2e/scenarios.mjs` 加情境;Linux 用 `just desktop-e2e` 在本機跑,Windows 由 CI 跑。

## 指令

```bash
bun run typecheck      # tsc(兩份 tsconfig)
bun run lint:check     # oxlint
bun run format:check   # prettier
bun run test           # 前端單元測試
just desktop-e2e       # 在 repo 根目錄執行:build debug App + 全部 E2E 情境
```
