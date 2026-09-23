# T-00 workspace 骨架

## 目標
建立 Cargo workspace 與 `snip-core` 的模組骨架,讓 L1 之後的 ticket 可以各自只改自己的檔案、互不衝突。

## 產出
- 根目錄 `Cargo.toml`:workspace,`members = ["crates/core", "crates/cli"]`,`[workspace.package]` 版本 `0.1.0`、edition 2021、license MIT。
  (`crates/desktop` 由 T-11 加入,這裡不建。)
- `crates/core/Cargo.toml`(套件名 `snip-core`):**一次加齊**後續會用到的依賴:
  `regex`、`serde`(derive)、`serde_json`、`sha2`、`dunce`、`similar`、`arboard`(Linux 開 `wayland-data-control`)、`thiserror`、`ts-rs`;
  dev-dependencies:`tempfile`。
- `crates/core/src/lib.rs`:宣告全部模組 `pub mod format; pub mod stats; pub mod paths; pub mod filter; pub mod settings; pub mod fsutil; pub mod restore; pub mod copy; pub mod gitsrc; pub mod commits; pub mod clip;`,每個模組一個 stub 檔(只放一行 module doc comment)。
- `crates/cli/Cargo.toml`(套件名 `snip-cli`,`[[bin]] name = "snip"`)依賴 `snip-core` 與 `clap`(derive);`src/main.rs` 印出版本即可。
- `fixtures/clipboard-contract.json`:從 `.ts-ref/test/fixtures/clipboard-contract.json` **逐位元組複製**。
- `crates/core/tests/contract.rs`:
  - 載入 fixture,斷言 SHA-256 = `df317eb7b412d4bd71222d71d4cd64a1652fbcac2d82468ec417e4ce95ec2468`(這個測試直接啟用)。
  - 用 serde 定義 fixture 各區塊(`buildCases`、`parseCases`、`tokenCases`、`pathLayout` + `pathCases`、`restoreCases`)的反序列化型別,並有一個測試確認整份 fixture 能反序列化。
  - 每個區塊各一個 `#[ignore = "T-0X"]` 的空測試函式:`build_cases`(T-01)、`parse_cases`(T-01)、`token_cases`(T-02)、`path_cases`(T-03)、`restore_cases`(T-05)。函式本體留 `todo!()`。
- `justfile`:`build`、`test`(`cargo test --workspace --no-fail-fast`)、`lint`(clippy `-D warnings`)、`fmt`、`preflight`(fmt check + clippy + test)。
- `rustfmt.toml`:照 `../aghub/rustfmt.toml`。
- 跑一次 `cargo build --workspace` 產生 `Cargo.lock` 並一起提交。

## 只可修改
上面列出的檔案。`.gitignore` 已存在,不要動。

## 驗收
`cargo test --workspace`(SHA 與反序列化測試綠燈,其餘 ignored)、`cargo clippy --workspace --all-targets -- -D warnings`。
