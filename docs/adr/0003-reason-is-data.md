# Reason 是資料，Display 只給 CLI 與線上，ErrorCode 封閉

失敗的意義由 `snip_core::Reason`（型別化的資料）表達；各錯誤型別（`GitError`、`TransferError`、`CommitError`、`RemoteError`）各有一個窮盡的 `reason()`。桌面 App 只經 `reason_msg` 一個翻譯點把 `Reason` 譯成目前介面語言，不讀任何英文字串，也沒有 `Msg::raw`。thiserror 的 `Display` 只用於 CLI 輸出與線上的 `message`，其文字不變，CLI 輸出與舊版逐位元組相同。`ErrorCode` 搬到核心並維持封閉：新的意義加進 `Reason`，不加進 `ErrorCode`，線上拼法不變。

原因：此前 zh-TW 介面會漏出英文，因為各處把 `Display` 當後備，並從英文訊息猜原因（「; try again」、「 failed:」、「exceeds 1 MiB」）。型別化後，編譯器逼每個新錯誤選一個 `Reason`，漏掉的不會默默變成英文。

## Consequences

- `Response::Error`、`ReplayFailure` 與掃描錯誤列帶可選的 `reason`，以 `lenient` 讀取：未知變體只讓該欄位變成 `None`，不使整個 frame 失敗。沒有原因時不輸出該鍵，不升協定版本。
- 上線前一律經 `Reason::for_wire`（或掃描列的 `scrubbed`）清掉分享根的絕對路徑、scope 與 git stderr；這兩個函式是窮盡的 match，不用 `_`。
- 舊 worker 不送 `reason`，新 master 退回通用措辭，CLI 的 repo-busy 拒絕少一段後綴；見 `docs/spec.md` 版本相容。
- 桌面 App 中仍是英文的只剩資料：git stderr、OS 的 I/O 文字、ssh 文字，且只作為有鍵標題的尾端引數（`Msg::detail`）。
- `scripts/check_msg_raw.sh` 擋下繞過這條規則的後門（`reason_detail` 字面值、`Msg` 別名）。
- 例外：`client.rs` 分類外部工具 ssh 的 stderr，那是產生點，不是接收端。
