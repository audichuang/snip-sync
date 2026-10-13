# 遠端檔案貼上協定量測

`scripts/bench_remote_paste.py` 比較同一組 master／worker 執行檔的協定 6 與 7。
它透過 `crates/remote/examples/bench_paste.rs` 呼叫產品共用的 `PasteTarget::plan_files`／`apply_files`，
以 worker 既有的 `serve --max-protocol 6/7` 選擇實際協商版本，不更換執行檔；frame bridge 只負責量測。

先提交原始碼，再用 release profile 建置：

```sh
cargo build --release -p snip-cli
cargo build --release -p snip-remote --example bench_paste
python3 scripts/bench_remote_paste.py \
  --driver target/release/examples/bench_paste \
  --worker target/release/snip \
  --out /tmp/snip-paste-bench-local-results
```

正式量測預設四組資料：真實 Rust source、1,200 個小檔案、接近 32 MiB 的 payload、固定 seed 的低壓縮 UTF-8。
每組冷連線／既有連線各跑 10 輪，協定 6／7 逐輪交換先後順序。
payload 由產品 formatter 產生，保留來源中的大小寫混合標頭、escape 與結束標記。量測前，先用原始來源獨立計算預期路徑、內容 hash 與筆數，檢查 parser 與本機貼上都符合、沒有意外略過或額外檔案；每次再使用全新目的資料夾，核對所有遠端檔案 SHA-256。
`--rounds 1 --scale 0.01` 只適合檢查工具可運作，報告會標成 exploratory。

跨機器時，先把**相同 commit** 的 worker 建置在遠端獨立暫存資料夾，不覆寫已安裝的 `snip`，然後加上：

```sh
--ssh ubuntu \
--remote-snip /tmp/your-isolated-build/target/release/snip \
--worker-source-sha <40-character-commit-sha>
```

SSH benchmark 自己建立的每輪 fixtures 與側錄工具由 finally 清理，結果保留在本機 `--out`；
遠端 worker 的建置資料夾由建立它的人清理。檢查 `results.json` 的 `cleanup.completed` 與 `failure`，
以及所有 sample 的 `hashes_match`。報告包含本機來源 commit／dirty 狀態、執行檔 hash 與呼叫者提供的 worker 來源 commit。

解讀數字時保留以下界線：

- bytes 是含長度前綴的協定 frames，不含 preamble、SSH 握手／加密、TCP/IP、ACK 或重傳。
- elapsed 是經 bridge 量測的 PasteTarget 路徑，排除載入輸入檔、建立 fixture 與核對 hash；不代表整個 CLI 或 GUI 的時間。既有連線排除最初的 OpenWorkspace，冷連線包含它。
- CPU 分別計 master、各 worker 自身的 CPU；bridge 另記，SSH 的 CPU 不計入。Linux 用 `/proc`，macOS 將 `proc_taskinfo` 的 Mach ticks 依該機 timebase 轉成秒，不用累積的子行程資源統計。
- memory 是每個 PID 的取樣 peak RSS，要求 5 ms 間隔，屬於真實峰值的下界；多個 PID 的各自峰值不相加冒充同時用量。
- SSH Compression 沿用使用者設定；只在這些命令停用 multiplexing，讓冷連線確實建立新 SSH transport。傳輸量下降不等於速度提升，時間、CPU、RSS 與逐檔正確性必須一起看。

避免和 preflight、其他編譯或資源測試同時量測；macOS、遠端主機的負載都會影響結果。
