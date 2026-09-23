# T-02 stats:通知統計

## 目標
移植 `.ts-ref/src/copy.ts` 的 `payloadStats` 與 `estimateTokens` 到 `crates/core/src/stats.rs`。

## 範圍
- `PayloadStats { chars, lines, words, tokens }`、`payload_stats(text)`、`estimate_tokens(text)`。
- `chars` 是 UTF-16 code unit 數(`encode_utf16().count()`);`lines` 是 `\n` 數加 1(空字串為 0);`words` 是 ASCII 空白分隔的片段數;`tokens` 是 `words` 加上 `;{}()[],` 的出現次數。**以 TS 原始碼為準**。
- 參考測試:`.ts-ref/test/estimateTokens.test.ts`。

## 只可修改
`crates/core/src/stats.rs`、`crates/core/tests/contract.rs` 的 `token_cases` 函式。

## 驗收
`cargo test -p snip-core --test contract token_cases` 與 `cargo test -p snip-core --lib stats` 全綠。
