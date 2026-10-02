# snip-remote

遠端節點模式(spec 第 8 節)。master 是桌面 App,worker 可以是 `snip worker`(CLI),也可以是桌面 App 的 worker 模式。機制見 [plan 6.5 節](../../docs/plan.md)。

| 模組 | 職責 |
|---|---|
| `proto` | 幀格式(u32 長度加 JSON,上限 8 MiB)、請求與回應 |
| `tls` | 裝置身分(自簽憑證)、以指紋 pin 憑證的 TLS 1.3 設定、配對證明 |
| `worker` | 監聽器與請求處理,只服務分享資料夾之內的路徑 |
| `client` | 配對,以及 pin 住 worker 憑證之後的呼叫(含小型連線池) |

## 規範

- 一律用阻塞 IO,不用 async。每個 socket 都要設逾時;呼叫端自己決定要不要丟到背景執行緒。
- worker 處理請求時直接呼叫 `snip-core`(`browser::file_preview`、`workspace::DirectoryScan`、`browser::inside`),所以遠端讀取的大小上限、UTF-8 判斷、containment 都與本機相同。
- 改了任何會讓舊版對方誤讀的格式,就把 `PROTOCOL_VERSION` 加一。
