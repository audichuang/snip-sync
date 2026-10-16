# Reason 是資料，Display 只給 CLI 與線上，ErrorCode 封閉

失敗的意義由 `snip_core::Reason`（型別化的資料）表達；各錯誤型別（`GitError`、`TransferError`、`CommitError`、`RemoteError`）各有一個窮盡的 `reason()`。桌面 App 只經 `reason_msg` 一個翻譯點把 `Reason` 譯成目前介面語言，不讀任何英文字串，也沒有 `Msg::raw`。thiserror 的 `Display` 只用於 CLI 輸出與線上的 `message`，其文字不變，CLI 輸出與舊版逐位元組相同。`ErrorCode` 搬到核心並維持封閉：新的意義加進 `Reason`，不加進 `ErrorCode`，線上拼法不變。

原因：此前 zh-TW 介面會漏出英文，因為各處把 `Display` 當後備，並從英文訊息猜原因（「; try again」、「 failed:」、「exceeds 1 MiB」）。型別化後，編譯器逼每個新錯誤選一個 `Reason`，漏掉的不會默默變成英文。

## Consequences

- `Response::Error`、`ReplayFailure` 與掃描錯誤列帶可選的 `reason`，以 `lenient` 讀取：未知變體只讓該欄位變成 `None`，不使整個 frame 失敗。沒有原因時不輸出該鍵，不升協定版本。
- 上線前一律清掉分享根的絕對路徑、scope 與 git stderr；這兩個函式是窮盡的 match，不用 `_`。帶 `ErrorCode` 的框（`Response::Error`）走 `Reason::beside_code`，它在 code 已說明時回 `None`；沒有 code 的框（replay 失敗、掃描列、restore 失敗）一律走 `scrubbed`，保留變體，否則 master 會退回通用措辭。
- `Reason` 的六個自由文字欄位（`Start.detail`、`Disconnected.detail`、`GitFailed.stderr`、`Internal.detail`、`Io.detail`、`SubmoduleUnreadable.detail`）是 `Datum`：沒有 `From<String>`，每個建構子（`os_error`、`git_stderr`、`git_output`、`transport_stderr`、`spawn_failure`、`label`、`command`、`old_peer_text`）說出文字的來源；線上仍是裸字串（`serde(transparent)`）。只有 `Datum::os_error` 是硬保證，它只留 `raw_os_error()` 有值的文字：我們、std 或函式庫寫的句子（"exceeds 1 MiB"、"failed to fill whole buffer"）不再變成資料，畫面只顯示標題。其餘建構子只強迫產生者說出來源，另由 `check_msg_raw.sh` 拒絕對它們傳字面值，並由 `PRODUCT_TEXT_FIELDS` 把六個欄位都餵入英文句子驗證偵測器看得到。
- `snip-remote` 自己拋的 `io::Error`（worker 關閉連線、frame 過大、無法解析）是型別化的 `IoFault`：`kind()` 與 `Display` 逐位元組不變（CLI 與重試判斷照舊），`RemoteError::reason` 依變體措辭（`WorkerClosed`、`Protocol`、`PayloadTooLarge`）；其他 `io::Error` 走漏斗，只留 OS 的文字。`Reason::WorkerClosed` 只在 master 端產生，不上線。`GraphError` 與剪貼簿錯誤（`ClipFault`）同理：畫面用型別的變體措辭，函式庫的英文留在 CLI 與日誌。
- `Reason` 的 String 欄位只能是使用者該原樣看到的資料；函式庫或產品的英文診斷（如 serde_json 的訊息）必須是型別化資料（`InvalidPayload { kind, line, column }`），CLI 的 Display 位元組不變。分類寫在 `reason_msg` 旁，`string_fields_are_classified` 釘住。使用者造成的條件同樣型別化：`.gitmodules` 不是檔案、路徑不在此 Git 來源、非 UTF-8 路徑等成為 `gitsrc::Condition`（`GitError::Condition`，Display 位元組與舊的 `Diagnostic` 相同），經 `Reason::Internal.condition` 由桌面 App 以兩種語言表達（`condition` 是加法欄位，舊 peer 不送也不讀）；啟動失敗的「沒有指令」「無法執行」成為 `StartReason::NoCommand`／`CannotRun`。`Internal.detail` 只留 bug 回報用的標籤，使用者看得到的英文句子不得放進 String 欄位；`product_text_never_reaches_a_data_field` 逐一餵入驗證。
- worker 是否隱藏一段文字由產生者決定：`GitError::Malformed { what, output }` 的 `what` 是我們的固定標籤、`output` 才是 git 的原始輸出（只有 `output` 是資料，隱藏）；`GitError::Diagnostic(&'static str)` 是我們自己的固定診斷（原樣送出，Display 位元組與 Malformed 相同）；使用者造成的條件各有型別（`Condition`、`NotAFile`）。我們自己的句子不得放進 `Malformed.output`。沒有 `ErrorCode` 的框（`Response::{Repos, Imported, Replayed}`）收 `Codeless<T>`，唯一的建構點是 `Codeless::new`，每個 payload 型別以不含 `..` 的全解構清 reason，由型別強制，不再靠逐樣本驗證（`codeless_frames_keep_every_reason` 只剩回歸守護）。Apply 失敗印在 `PASTE_STALE_DETECTED` 的 token 同樣由型別擁有（`ApplyRefusal { msg, token }`），不再由手寫的鍵清單推回。另外 e2e 日誌行（`[APP:…]`）是機器合約，印引擎的 Display 或訊息鍵，不隨翻譯改變。
- 舊 worker 不送 `reason`，新 master 退回通用措辭，CLI 的 repo-busy 拒絕少一段後綴；見 `docs/spec.md` 版本相容。
- 桌面 App 中仍是英文的只剩資料：git stderr、OS 的 I/O 文字、ssh 文字，且只作為有鍵標題的尾端引數（`Msg::detail`）。
- `scripts/check_msg_raw.sh` 擋下繞過這條規則的後門（`reason_detail` 字面值、`Msg` 別名、`reason.rs` 以外的 `Msg::detail`、錯誤的 `Display` 進畫面、對外部文字建構子傳字面值）。哪些行算測試碼只有一份判定（`scripts/lib/test_code.sh`，與 `check_test_refresh.sh` 共用）：檔名以 `_tests.rs` 結尾的整檔，或第一個頂層 `mod tests {` 之後；整檔的測試模組因此必須命名為 `*_tests.rs`。
- 例外：`client.rs` 分類外部工具 ssh 的 stderr，那是產生點，不是接收端。
