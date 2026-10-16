# Reason 是資料，Display 只給 CLI 與線上，ErrorCode 封閉

失敗的意義由 `snip_core::Reason`（型別化的資料）表達；各錯誤型別（`GitError`、`TransferError`、`CommitError`、`RemoteError`）各有一個窮盡的 `reason()`。桌面 App 只經 `reason_msg` 一個翻譯點把 `Reason` 譯成目前介面語言，不讀任何英文字串，也沒有 `Msg::raw`。thiserror 的 `Display` 只用於 CLI 輸出與線上的 `message`，其文字不變，CLI 輸出與舊版逐位元組相同。`ErrorCode` 搬到核心並維持封閉：新的意義加進 `Reason`，不加進 `ErrorCode`，線上拼法不變。

原因：此前 zh-TW 介面會漏出英文，因為各處把 `Display` 當後備，並從英文訊息猜原因（「; try again」、「 failed:」、「exceeds 1 MiB」）。型別化後，編譯器逼每個新錯誤選一個 `Reason`，漏掉的不會默默變成英文。

## Consequences

- `Response::Error`、`ReplayFailure` 與掃描錯誤列帶可選的 `reason`，以 `lenient` 讀取：未知變體只讓該欄位變成 `None`，不使整個 frame 失敗。沒有原因時不輸出該鍵，不升協定版本。
- 上線前一律清掉分享根的絕對路徑、scope 與 git stderr；這兩個函式是窮盡的 match，不用 `_`。帶 `ErrorCode` 的框（`Response::Error`）走 `Reason::beside_code`，它在 code 已說明時回 `None`；沒有 code 的框（replay 失敗、掃描列、restore 失敗）一律走 `scrubbed`，保留變體，否則 master 會退回通用措辭。
- `Reason` 的 String 欄位只能是使用者該原樣看到的資料；函式庫或產品的英文診斷（如 serde_json 的訊息）必須是型別化資料（`InvalidPayload { kind, line, column }`），CLI 的 Display 位元組不變。分類寫在 `reason_msg` 旁，`string_fields_are_classified` 釘住。使用者造成的條件同樣型別化：`.gitmodules` 不是檔案、路徑不在此 Git 來源、非 UTF-8 路徑等成為 `gitsrc::Condition`（`GitError::Condition`，Display 位元組與舊的 `Diagnostic` 相同），經 `Reason::Internal.condition` 由桌面 App 以兩種語言表達（`condition` 是加法欄位，舊 peer 不送也不讀）；啟動失敗的「沒有指令」「無法執行」成為 `StartReason::NoCommand`／`CannotRun`。`Internal.detail` 只留 bug 回報用的標籤，使用者看得到的英文句子不得放進 String 欄位；`product_text_never_reaches_a_data_field` 逐一餵入驗證。
- worker 是否隱藏一段文字由產生者決定：`GitError::Malformed { what, output }` 的 `what` 是我們的固定標籤、`output` 才是 git 的原始輸出（只有 `output` 是資料，隱藏）；`GitError::Diagnostic(&'static str)` 是我們自己的固定診斷（原樣送出，Display 位元組與 Malformed 相同）；使用者造成的條件各有型別（`Condition`、`NotAFile`）。我們自己的句子不得放進 `Malformed.output`。沒有 `ErrorCode` 的框各由 `snip-remote` 的 `codeless` 模組的一個函式產生 reason，`codeless_frames_keep_every_reason` 以每個 `Reason` 樣本驗證變體被保留。另外 e2e 日誌行（`[APP:…]`）是機器合約，印引擎的 Display 或訊息鍵，不隨翻譯改變。
- 舊 worker 不送 `reason`，新 master 退回通用措辭，CLI 的 repo-busy 拒絕少一段後綴；見 `docs/spec.md` 版本相容。
- 桌面 App 中仍是英文的只剩資料：git stderr、OS 的 I/O 文字、ssh 文字，且只作為有鍵標題的尾端引數（`Msg::detail`）。
- `scripts/check_msg_raw.sh` 擋下繞過這條規則的後門（`reason_detail` 字面值、`Msg` 別名）。
- 例外：`client.rs` 分類外部工具 ssh 的 stderr，那是產生點，不是接收端。
