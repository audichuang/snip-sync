# 真實 UI 操作驗收規程

狀態：操作規程。給人照著點真實 macOS 視窗，也給會操作滑鼠的 agent 照著做。

產品行為以 [spec.md](spec.md) 為準。視窗長什麼樣子以 [native-workbench-ui-acceptance.md](native-workbench-ui-acceptance.md) 為準。30 repo 的語意資料以 [native-collaboration-acceptance.md](native-collaboration-acceptance.md) 為準。2026-09-29 的 [實測報告](test-reports/2026-09-29-real-ui/report.md) 是一輪紀錄，不是這份規程的通過線。

這份規程只回答一件事：點完之後，怎樣才算這一格功能正確。

## 1. 操作者要交什麼

一輪結束時交出一個目錄，放在 repo 外面。建議路徑：

```text
~/snip-sync-ui-runs/<日期>-<受測 SHA 前 7 碼>/
```

目錄裡要有 `environment.json`、`scorecard.md`、每個測案一個子目錄。測案子目錄的內容見第 4 節。不要把這個目錄 commit 進 repo，也不要在這一輪裡改產品程式。

`scorecard.md` 每一格只能是這五個字之一：

| 判定 | 什麼時候用 |
|---|---|
| `pass` | 控制項身分、點下去之後的新日誌、剪貼簿、Git／檔案四者都符合該格的通過線 |
| `ui-defect` | Git／檔案符合通過線，預覽、顏色、計數或通知和實際動作不一致 |
| `fail` | 寫錯、拒絕錯、點不到指定控制項、或日誌對不上。點不到時證據欄寫 `missing-control`；同路徑兩列共用一個 ID 時證據欄寫 `identity-fail`。這兩個詞不是判定 |
| `blocked-contract` | 規格與實作衝突，還沒有人決定以哪邊為準。目前只用於 T25，而且只用在這份衝突還沒定案的時候 |
| `not-run` | 這一格沒有做 |

計分表只有這五個判定。

產品閘門只看下面這些格，每一格都是 `pass` 才寫「關閉」：

- 閘門 A 的 18 步
- T01–T22、T24、T26–T37、T39、T40
- T23。T18 是 `pass` 時它納入閘門；T18 是 `fail` 時它寫 `not-run`，證據欄寫「T18 未套用」，不另算一次失敗
- B41–B44、B-notify
- T25。判定是 `pass`、`fail` 或 `not-run` 時算進閘門。判定是 `blocked-contract` 時不決定閘門關不關

T25 一定要做。`binary.dat` 仍在、位元組仍是 `a`、NUL、`b`、換行、並標成未複製時，這一格算進閘門，由下面兩項決定判定：根目錄 `gone.txt` 已刪除，而且 `dir/gone.txt` 仍是 `folder-gone`。兩項都成立是 `pass`，有一項不成立是 `fail`。`binary.dat` 被刪掉或位元組變了時，在受測 SHA 上執行 `git log --oneline --grep='不再重播刪除' -- crates/core`。沒有輸出就判定 `blocked-contract`；有輸出就判定 `fail`，這一格留在閘門裡。`-- crates/core` 不能省：只改文件的 commit 也可能在內文提到這幾個字，沒有路徑限制時會把還沒修的產品誤判成 `fail`。比對用這段 `--grep`，不要拿分支上 `d13e18f` 的整句主題做全字相等。feature PR 是 squash 合進 develop，主題會變成 PR 標題加 `(#NN)`。那筆出現在 develop 上之後，把實際主題或 PR 編號寫進本段，取代這段 `--grep`。沒做就記 `not-run`，閘門打開。

下面兩格要填，但不決定閘門關不關：

- T38：判定固定 `not-run`，證據欄寫「改走記憶體規程」
- C preflight：判定固定 `not-run`，證據欄寫「這台 Mac 沒有 xvfb-run 與 /proc」

閘門裡有 `ui-defect`、`fail` 或 `not-run`，第一句就寫「產品閘門打開」，並列出那些 ID。T25 的 `blocked-contract` 不把閘門撐開。T25 是 `blocked-contract` 時，第 8 節要寫二進位刪除的契約還沒決定。第 9 節的項目尚未測，整個產品不在本規程宣告完成。

重現一個已知問題，代表這輪有觀察到它。只要該 ID 在閘門裡，產品閘門就打開。

## 2. 開跑

1. 向派工的人確認受測 SHA。沒有另外指定時，用 `origin/develop`。
2. 在乾淨的 worktree 或 checkout 上建置。不要拿別的分支的 `target/` 混用。

下面的賦值用 `export`。bash、zsh 與 fish 都能執行。這台機器的預設 shell 是 fish。fish 不接受沒有接命令的 `RUN=...`（會報 `Unsupported use of '='`）。fish 3.1 起，`VAR=value 命令` 會把值傳給那個命令。

```text
export RUN="$HOME/snip-sync-ui-runs/$(date +%Y-%m-%d)-$(git rev-parse --short HEAD)"
mkdir -p "$RUN"
rm -f "$RUN/export-hold"
git rev-parse HEAD
cargo build -p snip-desktop-native --locked
shasum -a 256 target/debug/snip-desktop-native
```

後面的命令都用這個 `$RUN`。`export-hold` 先不要建立；檔案不存在時，複製不會停住。

3. 跑兩個 fixture。閘門 B：

```text
sh docs/real-ui-operator-fixture.sh "$RUN/gate-b"
python3 scripts/workload_generator.py "$RUN/perf15" \
  --repos 15 --files 1000 --commits 1000 --refs 30 --quiet
```

`perf15` 是 15×1,000×1,000。它只給 Git log 的版面、搜尋與切換。它不是標準效能資料集。

閘門 A：

```text
python3 scripts/collaboration_fixture.py generate --output "$RUN/gate-a"
python3 scripts/collaboration_fixture.py verify --fixture "$RUN/gate-a"
```

`verify` 的結束碼必須是 0。每個 A 步驟開始前再拍一張初始快照；不要把上一步改過的 repo 接著用。需要乾淨狀態時，重新 `generate` 到一個新目錄。

4. 讀受測 SHA 的 `crates/desktop-native/src/ui/paste.rs`，把 `paste-row`、`paste-include`、`paste-overwrite` 的 ID 格式抄進 `environment.json`。寫這份規程時的 `bd09e95` 格式只有相對路徑。若受測 SHA 仍是這個格式，T03 與 T18 的身份檢查會失敗，這兩格不能是 `pass`。
5. 啟動 App。探針只有在 `SNIP_NATIVE_E2E=1` 時才會印。`SNIP_NATIVE_E2E_EXPORT_HOLD_FILE` 在 `WorkbenchModel::new` 讀一次，所以必須寫在啟動命令裡。不要加 `--restore-dir`，貼上目的地由畫面決定。每一次啟動的 stdout 與 stderr 寫進該次自己的日誌，否則讀不到 `CTRL_BOUNDS`。

同一時間只開一個 App。閘門 A 與閘門 B 各啟動一次，各寫一份日誌。先做哪一個都可以。換下一個之前，對目前這個按 Cmd+Q，等到 exit code 0，再啟動。第二次若用 `>` 寫進第一次的檔，會把 `CTRL_BOUNDS` 與 `PASTE_DONE` 截掉。

同一個 shell 裡，下面三個 export 做一次即可。新開 shell 要再 export。

```text
export SNIP_NATIVE_E2E=1
export SNIP_THEME=dark
export SNIP_NATIVE_E2E_EXPORT_HOLD_FILE="$RUN/export-hold"
```

閘門 B：

```text
./target/debug/snip-desktop-native \
  --workspace "$RUN/gate-b/fixtures/ws-src" \
  > "$RUN/app-gate-b.log" 2>&1
```

閘門 A：

```text
./target/debug/snip-desktop-native \
  --workspace "$RUN/gate-a/machine-a" \
  > "$RUN/app-gate-a.log" 2>&1
```

閘門 A 的工作區若不是 `machine-a`，改成該 fixture 實際的路徑。自己啟動它，結束時才讀得到 exit code。同一個 App 還開著時，換工作區用畫面上的開工作區，不要為了換目錄再疊一個程序。

6. `environment.json` 至少包含：受測 SHA、執行檔 SHA-256、兩次啟動命令、`app-gate-a.log` 與 `app-gate-b.log` 的路徑、三個視窗尺寸實際量到的寬高、paste ID 格式、fixture 目錄。

視窗要跑三種邏輯尺寸：1080×752（對照 2026-09-29 報告的 BUG-04）、1080×720 與 900×600（驗收規格）。把視窗調到該尺寸後，從這一次啟動的日誌抄下最新的 `[APP:VIEWPORT: WxH]`。這行是實體像素，Retina 上會比邏輯尺寸大。兩組數字都留下。功能 oracle 在三種尺寸相同；T36 每種尺寸各判一次。

快捷鍵用 macOS 的 Cmd：Cmd+C 複製、Cmd+V 開預覽、Cmd+Q 結束、Cmd+Shift+O 開工作區、Cmd+Shift+W 關工作區。Option+1 專案、Option+0 變更、Option+9 Git log、Option+Shift+R 選 repo。預覽面板裡 Enter 套用、Escape 取消、Space 切換目前列。程式也綁了 Ctrl；操作仍按 Cmd，並用日誌確認送到的是複製。在這個 App 裡按 Cmd+C 是 App 自己的複製。

Shell 可以建立 fixture、用 argv 叫 Git、把 Git 輸出寫進檔再算雜湊、用 `pbcopy` 放入 sentinel 或 T15 的普通文字、用 `pbpaste` 讀剪貼簿。Shell 不執行 `snip copy` 或 `snip paste` 來代替畫面上的複製與貼上。

## 3. 怎麼點

每一次點擊都做完這七步。少一步，該格就是 `fail`。

1. 從這一次啟動的日誌取這個控制項最新的一行。閘門 A 讀 `$RUN/app-gate-a.log`，閘門 B 讀 `$RUN/app-gate-b.log`。

```text
[APP:CTRL_BOUNDS: id=<完整 ID> x= y= w= h=]
```

ID 整段相等。`paste-overwrite:common.txt` 不能拿去點 `paste-overwrite:common.txt.bak`。

2. 沒有這一行，或 `w < 1`，或 `h < 1`：判定 `fail`，證據欄寫 `missing-control`，停止。改點旁邊看起來像的列，不算測過。
3. 同一個 ID 畫了兩次時，`ProbeFrame::report` 用 ID 當 key（`crates/desktop-native/src/ui/mod.rs`）。第一幀兩個矩形都會印出來；之後每次重繪，只會再印第一列的矩形，因為上一幀留下的是後畫的那一個，只有第一列跟它不同。所以「取最新一行」拿到的通常是第一列，不是第二列。不要把「日誌裡只剩一個矩形」當成辨識規則。辨識用第 6 節的算法：畫面上同相對路徑的列數大於相異 ID 數。成立時判定 `fail`，證據欄寫 `identity-fail`，並寫下兩欄數字。這種情況不能按 Apply，也不能把「有點到第二列」寫成 `pass`。
4. 把 bounds 換算成螢幕上的點，再點那個點。`x`、`y`、`w`、`h` 是實體像素，原點在視窗內容區的左上角，不是螢幕，也不是含標題列的視窗框。`physical()` 乘了 `scale_factor`。直接點 `(x + w/2, y + h/2)` 在 Retina 上會偏到大約兩倍遠。換算順序：

   - 讀最新的 `[APP:VIEWPORT: Wp x Hp]`。
   - 讀 `snip-desktop-native` 視窗框的螢幕位置與邏輯尺寸，以及視窗框中心所在那一塊螢幕的 `backingScaleFactor`。視窗框含標題列。外接螢幕的 scale 可以和主螢幕不同；用主螢幕的值會把點算錯。
   - `scale` 用 `backingScaleFactor`。`Wp` 應等於內容區邏輯寬乘 `scale`。對不上就判定 `fail`，證據欄寫算式與兩個寬度，不要點。
   - 標題列高度 = 視窗框邏輯高 − (`Hp / scale`)。內容區左上角 = 視窗框左上角的 x，以及視窗框的 y 加上標題列高度。
   - 螢幕點 = 內容區左上角 + `((x + w/2) / scale, (y + h/2) / scale)`。點擊工具若吃邏輯點，用這個螢幕點。若吃實體像素，再乘 `scale`。`action.json` 同時留下原始 bounds、`scale`、內容區左上角與實際點到的螢幕點。

   Linux 的 native-e2e 用 `xdotool mousemove --window`，座標就是內容區裡的實體像素，而且那時 `scale` 是 1。那套算法不要用在這台 Mac 的螢幕游標上。

5. 點完之後必須有一行新日誌，而且裡頭的 idx、path、prefix 或 SHA 就是這一格。點下去之前就存在的日誌不算。
6. 捲動之後矩形會變。捲完再讀一次 bounds，用新的中心點重算螢幕點。
7. 然後才跑該格的剪貼簿與 Git／檔案 oracle。

macOS 輔助使用在這個 App 上只到視窗。沒有 `CTRL_BOUNDS` 的點擊，不能記 `pass`。

### 3.1 會用到的控制項

ID 以受測 binary 印出的為準。下面是寫這份規程時 `bd09e95` 的名字。

| 位置 | ID |
|---|---|
| 左軌 | `rail-project`、`rail-changes`、`rail-log` |
| 工作區 | `btn-workspace-menu`、`btn-open-workspace`、`btn-workspace-open-confirm`、`btn-close-workspace`、`btn-open-folder` |
| 工具列 | `btn-repo-selector`、`btn-ref-selector`、`btn-copy`、`btn-paste`、`btn-refresh`、`btn-basket-clear` |
| 選單列 | `pick-repo:<序號>:<名稱>`、`pick-ref:<名稱>` |
| 變更列 | `change-row@<repo>:<staged\|unstaged\|untracked\|conflicted>:<path>`，勾選是 `change-chk@...`。目前選取中的 repo 才同時有舊 ID `change-row:<source>:<path>` |
| 變更群組 | `change-repo:<群組>:<repo>`、`change-repo-toggle:<群組>:<repo>` |
| Git log | `commit-row:<7 字元 SHA>`、`btn-copy-commits`、`btn-log-regex`、`btn-head` |
| 多 repo 篩選 | `log-filter-repo`、`log-repo:<名稱>`、`log-repo-check:<名稱>` |
| 單一 repo 工作區的路徑篩選 | `log-filter-paths`。多 repo 工作區不要去點這個 ID |
| 歷史檔案 | `btn-browse-tree:<完整 SHA>`、`rev-row:<path>`、`rev-chk:<完整 oid>:<path>`、`btn-leave-tree` |
| Git log 變更檔案 | `commit-file:<path>`、`commit-dir:<path>`。資料夾列的選取 key 是 `<path>/`。左鍵資料夾是展開或收合；複製用右鍵 |
| 貼上 | `btn-apply`、`btn-cancel`、`paste-row:<path>`、`paste-include:<path>`、`paste-overwrite:<path>` |
| 多 repo 對應 | `paste-map-pick:<prefix>:<idx>`、`paste-map-keep:<prefix>` |

右鍵選單沒有獨立的 bounds ID。點開之後，通過線是新的一行 `[APP:MENU_ACTION: copy-files]`（或該動作的 ID）。

## 4. 剪貼簿與 Git 怎麼對

每格在動作前記下：

```text
git -C "$repo" rev-parse HEAD
git -C "$repo" status --porcelain=v1 -z > "$CASE/status-before.bin"
shasum -a 256 "$CASE/status-before.bin"
git -C "$repo" rev-parse --abbrev-ref HEAD
```

Git 的參數以 argv 傳給 `git`。雜湊對寫好的檔算，不把 Git 輸出用管線串走。涉及的路徑另算 SHA-256。剪貼簿用 `pbpaste` 存成檔再算 SHA-256；複製之後再讀一次，兩次 SHA-256 必須相同。commit 模式的 payload 第一行是 `// snip-sync commits v1`。sentinel 與 T15 的普通文字用 `pbcopy` 放入。

動作後：

- 檔案模式：HEAD 與 index 不變，只有通過線點名的 worktree 路徑改變。
- commit 模式：`git rev-list --count <before>..HEAD` 等於通過線的筆數。新 commit 的 SHA 不必等於來源 SHA。要比作者名稱、email、作者時間與時區、完整 message（含換行）、parent 數為 1，以及通過線點名的路徑位元組。
- 沒有點到的 repo：HEAD 與 status 雜湊不變。
- 負向步驟：快照等於該步規定的那一張，剪貼簿 sentinel 的 SHA-256 不變，並且有一行對得上的新拒絕日誌。畫面沒反應、按鈕看起來是灰的、或只有一句通用錯誤，都是 `fail`。

每筆 Git 自己有逾時。比對時設 `GIT_CONFIG_GLOBAL=/dev/null` 與 `GIT_CONFIG_NOSYSTEM=1`。

每格目錄：

| 檔案 | 內容 |
|---|---|
| `action.json` | 控制項 ID、bounds、中心點、點下之後那行新日誌 |
| `clipboard-before.bin` / `clipboard-after.bin` | 與兩邊的 SHA-256 |
| `snapshot-before.json` / `snapshot-after.json` | HEAD、status 雜湊、涉及路徑的 SHA-256 |
| `oracle.json` | 這一格預先寫下的預期 |
| `screen.png` | 當時畫面。它證明畫面長這樣，不參與通過與否 |

閘門 A 再加 fixture 自己的快照：

```text
python3 scripts/collaboration_fixture.py snapshot \
  --fixture "$RUN/gate-a" --output "$CASE/before.json"
# 做完這一步
python3 scripts/collaboration_fixture.py snapshot \
  --fixture "$RUN/gate-a" --output "$CASE/after.json"
python3 scripts/collaboration_fixture.py compare-step \
  --fixture "$RUN/gate-a" --step <步驟 id> \
  --snapshot "$CASE/after.json" --phase applied
```

`compare-step` 結束碼 0 才算 Git oracle 過。負向步驟的 `applied` 必須等於 baseline，或等於動作前、外部修改之後那張快照。規格寫在 [native-collaboration-acceptance.md](native-collaboration-acceptance.md)。

## 5. 閘門 A：18 個語意步驟

這 18 個 ID 與 `scripts/check_native_collaboration.py` 的 `REQUIRED_STEP_IDS` 相同。這台 Mac 沒有 Xvfb 與 xdotool，不要執行那個 driver。人用本規程的點法，oracle 仍用 `compare-step`。

正向步驟的共同點法：

1. `btn-repo-selector` → `pick-repo:<序號>:<basename>`。等 `[APP:REPO_SELECTING]` 的 canonical root 與 manifest 的 `relativePath` 一致。basename 相同的 repo 不能只看名字。
2. 檔案步驟走 `rail-changes`，展開 `change-repo-toggle:<群組>:<repo>`，再點 `change-chk@<repo>:<source>:<path>`。
3. `btn-copy`。等 `[APP:COPY_DONE]`。剪貼簿讀回 SHA-256 等於複製當下。
4. 換到目的工作區，`btn-paste`。`[APP:PASTE_MAP_CANDIDATE]` 在 `[APP:PASTE_PREVIEW]` 之前出現，從按貼上之前的日誌位置開始讀。
5. 點 `paste-map-pick:<prefix>:<idx>`，等 `[APP:PASTE_MAPPED]` 的 `dest` 就是那個 canonical root。已存在且內容會變的檔才點覆寫。
6. `btn-apply`。等 `[APP:PASTE_DONE: ...]`，再跑 `compare-step`。

Commit 步驟把第 2–3 步換成：`rail-log`，點起點 `commit-row:<7 字元>`，Shift 點終點，等 `[APP:RANGE: commits=N]`，再點 `btn-copy-commits`。

| 步驟 | 通過時還要看到 |
|---|---|
| `file-a-to-b-pair01` | worktree 刪除 `src/extra.txt` 在目的端刪除；HEAD 與 index 不變 |
| `file-b-to-a-pair09` | index 刪除 `notes/guide.txt` 同樣刪除；HEAD 與 index 的其他項目不變 |
| `file-explicit-valid-root-pair01-to-pair15b` | 目的地是 manifest 指定的另一個 root（`b-south-edge` 這一對），不是同名的另一個 repo |
| `commit-a-to-b-pair02` | 含 NUL 的 `assets/tiny.bin` 標成未複製，目的端不寫入也不刪除它；`local/hold.txt` 留在 index，不進新 commit |
| `commit-b-to-a-unmerged-pair03` | 重播到尚未合併的歷史上，`compare-step` 通過 |
| `commit-merge-first-parent-pair04` | 只帶 merge 對 first parent 的 diff，不把另一條分支的 commit 展開進來 |
| `commit-root-pair05` | orphan root 能重播，`compare-step` 通過 |
| `commit-octopus-first-parent-pair01` | 四親 merge 只算那一筆 first-parent commit |
| `commit-rename-delete-pair08` | rename 是刪舊路徑加寫新路徑；目的端不會多出只存在於後續 main 的檔 |

負向 9 步：動作後快照等於該步規定的基準，sentinel 不變，並有下面這一行新日誌。

| 步驟 | 做法與必須出現的日誌 | 磁碟 |
|---|---|---|
| `neg-cross-repo-commits` | 選 repo 1 的 tip，切到 repo 2 再選它的 tip，按一次複製 | 剪貼簿與「只用 repo 2 那個 tip」做出的 oracle 逐欄相同；repo 1 的 commit 不在 payload 裡；零寫入 |
| `neg-noncontiguous-tips` | 先放入 sentinel。Shift 選 manifest 的 `baseOid` 與 `tipOid`，再複製 | 新的 `[APP:COPY_COMMITS_ERR: commits are not contiguous:]`；sentinel 不變 |
| `neg-mapping-collision` | 兩條路徑最後都映到同一個目的檔 | 第二次 replan 出現 `[APP:PASTE_PLAN_REFUSED: reason=target_collision]`；零寫入 |
| `neg-mapping-ambiguous-basename` | 只給 `billing`，west 與 east 都有 | 候選 root 同時含這兩個 canonical path；`mapping_required`；取消後零寫入 |
| `neg-mapping-missing-destination` | 不提交不存在的 repo id | `billing` 的候選正好是目的工作區那 15 個真實 root；Return 後 `[APP:PASTE_ERR: mapping_required]`；點 `btn-apply` 後 1 秒內沒有 `PASTE_APPLYING` 或 `PASTE_DONE` |
| `neg-stale-source` | 啟動命令裡已經有 `SNIP_NATIVE_E2E_EXPORT_HOLD_FILE`。選好變更後用 `pbcopy` 放入 sentinel，建立 `$RUN/export-hold`，再按一次 `btn-copy`。等到新的 `[APP:EXPORT_PLAN_READY: files=N]`（N 至少 1），改來源檔，拍快照，刪掉 hold 檔。同一次複製會接著跑。不要再按一次複製，也不要等選完才去設環境變數。這一步結束時 `$RUN/export-hold` 必須不存在。中途失敗而檔還在，先刪掉它，證據欄寫「hold 殘留，已刪」，然後才做後面的檔案模式複製。檔留著的話，之後每一次檔案模式複製都會停在 `EXPORT_PLAN_READY` | `[APP:COPY_FAILED: stale_source]` 與 `[APP:COPY_IDLE]`；沒有 `COPY_DONE`；sentinel 不變；快照等於改完之後、刪掉 hold 之前那張；`$RUN/export-hold` 不存在 |
| `neg-stale-target` | 預覽出現後改目的端，再 Apply | `[APP:PASTE_STALE_DETECTED:]`；快照等於改完之後、Apply 之前那張 |
| `neg-overwrite-unauthorized` | 目的檔已存在，不勾覆寫就 Apply | `[APP:PASTE_DONE:]` 且 `overwritten=0`；該檔位元組不變 |
| `neg-cancel` | 預覽後 Escape 或 `btn-cancel` | `[APP:PASTE_CANCELLED]`；快照等於生成時的 baseline |

18 步都做完後，對這一個 App 按 Cmd+Q。10 秒內程序結束，exit code 是 0，日誌有 `phase=drained intent=quit jobs=0`，沒有殘留的 git 子程序。

## 6. 閘門 B：這個視窗

Fixture 由 `docs/real-ui-operator-fixture.sh` 建立。目錄意義：

| 路徑 | 用途 |
|---|---|
| `fixtures/ws-src` | 15 個來源 repo。repo01 與 repo03 的 staged `unrelated.txt` 內容不同 |
| `fixtures/ws-dst` | 15 個目的 repo。repo01／repo03／repo06／repo07 已有不同內容；repo04 的 `newdir` 是一般檔 |
| `fixtures/nongit-dst` | 空的非 Git 資料夾 |
| `fixtures/files-src` | 檔案模式來源：staged 新增／修改／刪除／rename，以及 worktree 上另一份 `both.txt` |
| `fixtures/files-dst` | 檔案模式目的。已追蹤 `overwrite.txt`（`do-not-touch`）、`both.txt`（`dest-both`）、`gone.txt`（`gone-dest`）、`old-name.txt`（`old-dest`）、`binary.dat`（`keep-bin`）、`folder/utf16.txt`（`keep-utf16`） |
| `fixtures/commits-src` | first-parent 為 base、C1「多行中文」、C2、C3 merge、C4、empty replay。`side` 上另有 `SIDE` |
| `fixtures/commits-dst-overwrite` | 已有 `common.txt`，分支 `qa-replay`，另有 staged 與 untracked |
| `fixtures/commits-dst-clean` | 沒有 `common.txt`，分支 `qa-replay` |
| `fixtures/commits-dst-hooks` | 四種 hook 都 `exit 1` 並寫 `fixtures/hook-marker.txt` |
| `fixtures/commits-dst-present` | 已追蹤 `binary.dat`（`a`、NUL、`b`、換行）、根目錄 `gone.txt`（`gone`）、`dir/gone.txt`（`folder-gone`），分支 `qa-replay`。T25、T32、B42 各從這份初始樹開始。T25 與 T32 留下 `dir/gone.txt` |
| `fixtures/basket-src` | B42 的來源。`basket base` 有 `dir/keep.txt`（`keep`）與 `dir/gone.txt`（`folder-gone`）。下一筆 `basket folder and delete` 把 `dir/keep.txt` 改成 `keep-2`，並刪除 `dir/gone.txt`。沒有 `binary.dat` |
| `$RUN/perf15` | Git log 版面、搜尋、100 次切換 |

C1 的 message 是 `多行中文`、空行、`第二段`。C2 改 `common.txt`、把 `old.txt` 改名 `new.txt`、刪除 `gone.txt` 與含 NUL 的 `binary.dat`、新增 `emoji.txt`。這些路徑都在 `commits-src` 的根目錄。C3 是 merge，first parent 是 C2，對 first parent 的 diff 是 `side.txt`。C4 新增 `newdir/content.txt` 與含 NUL 的 `new-binary.bin`。

預設單檔上限是 500 KiB（`Settings::max_file_size_kb`）。`large.txt` 是 1,360,000 byte，複製時應被略過。

每一格開始前，若該目的 repo 已被上一格改過，就重建 fixture 或換一個新目錄，並當場寫下 before SHA。不要用上一格的結果回推這一格的 before。

T19 即使通過，也不清除 T18。目的端一開始沒有 `common.txt` 時，兩列都還不是覆寫，Apply 可以按；身份檢查要看的是兩列覆寫各自能點。

| ID | 點什麼 | 通過線 |
|---|---|---|
| T01 | 以 `fixtures/ws-src` 開工作區 | `[APP:READY_REPOS: 15]`，選擇器裡 15 個 root |
| T02 | 貼上時不選 `paste-map-pick`，再選一次 | 未選時 `[APP:PASTE_ERR: mapping_required]`，點 `btn-apply` 沒有 `PASTE_APPLYING`；選後 `[APP:PASTE_MAPPED]` 的絕對路徑正確 |
| T03 | repo01 與 repo03 的 `unrelated.txt` 各點 `paste-overwrite` | 兩個不同 ID，兩次 `[APP:PASTE_TOGGLED]` 的 idx 不同；套用後覆寫 2、跳過 0，兩邊位元組分別是 `from-repo01` 與 `from-repo03` |
| T04 | repo02／repo05 的 `only-src.txt` 與 repo06／repo07 的 `shared.txt` | 建立 2、覆寫 2；其餘 repo 的 HEAD 與 status 不變 |
| T05 | `files-src` 的 staged 新增、修改、刪除、rename，複製後貼到新鮮的 `files-dst`。預覽裡勾選 `both.txt` 的 `paste-overwrite` | payload 含 `[NEW] staged-new.txt`、`[MODIFIED] both.txt`（內容 `index-body`）、`[DELETED] gone.txt`、`[MOVED] new-name.txt`。沒有舊路徑的刪除項。貼上後建立 2、覆寫 1、刪除 1、跳過 0：建立 `staged-new.txt` 與 `new-name.txt`、覆寫 `both.txt` 為 `index-body`、刪除 `gone.txt`。`old-name.txt` 仍是 `old-dest`。沒勾 `both.txt` 的覆寫時，結果是跳過 1，這一格不能算過 |
| T06 | 同一路徑 `both.txt` 分別從變更列（index）與 Project（worktree）複製 | 兩次 payload 的位元組分別等於 index 的 `index-body` 與 worktree 的 `worktree-body` |
| T07 | 複製 `folder/`。這個資料夾有 7 個檔：`a.txt`–`e.txt`、`bin.dat`、`utf16.txt`。`large.txt` 不在裡面 | 通知是已複製 5、略過 2。略過的是 `bin.dat` 與 `utf16.txt`。目的端建立 `a.txt`–`e.txt`；`folder/utf16.txt` 仍是 `keep-utf16`；不建立 `folder/bin.dat` |
| T08 | 複製來源的 `binary.dat` 與 `folder/utf16.txt`，貼到新鮮的 `files-dst` | payload 沒有這兩個檔的可還原內容。`files-dst/binary.dat` 仍是 `keep-bin`，`files-dst/folder/utf16.txt` 仍是 `keep-utf16` |
| T09 | 複製 `large.txt` | payload 是超過大小的 placeholder；貼上不建立 `large.txt` |
| T10 | 複製 `empty.txt` | 目的端檔案存在且長度 0 |
| T11 | 複製 `路徑 有空白/檔案.txt` 與 `crlf.txt` | 路徑與 `中文` 正確；CRLF 變 LF，首尾空行與檔尾換行依檔案模式契約去掉 |
| T12 | 預覽裡取消一個新檔，不勾 `overwrite.txt` 的覆寫 | 取消的檔不出現；`overwrite.txt` 仍是 `do-not-touch` |
| T13 | 預覽出現後在外面改目的檔，再 Apply | `[APP:PASTE_STALE_DETECTED]`；外面寫入的內容保留；沒有部分寫入 |
| T14 | 預覽後 Escape | `[APP:PASTE_CANCELLED]`；HEAD 不變 |
| T15 | 用 `pbcopy` 放入普通文字 `QA invalid clipboard payload`，再按貼上。不要在這個 App 裡按 Cmd+C | `[APP:PASTE_ERR: clipboard]`；目的端零寫入 |
| T16 | 目的地改為 `fixtures/nongit-dst`，保留 repo 前綴 | 目錄裡沒有 `.git`；同時有保留前綴的路徑與去掉前綴的路徑，內容與來源一致 |
| T17 | 在 `commits-src` 把 `SIDE` 和 main 上不能組成 first-parent 鏈的 commit 一起 Shift 選取，再複製 | `[APP:COPY_COMMITS_ERR: commits are not contiguous:]`；sentinel 不變 |
| T18 | 選 C1、C2、C3，貼到 `commits-dst-overwrite` 的 `qa-replay` | 兩列 `common.txt` 各有自己的覆寫 ID 與 idx；Apply 後 `git rev-list <before>..HEAD` 是 3 |
| T19 | 同一段貼到 `commits-dst-clean` | 3 個新 commit；C1 的 `common.txt` 在 Git 裡是新增 |
| T20 | 核對 T19 那 3 個 commit | `git log -1 -z --format=%an%x00%ae%x00%aI%x00%B` 與來源逐欄相等。C1 含多行中文與空行。作者時間是 fixture 寫入的 `+08:00` |
| T21 | 核對 T19 的 merge 那一筆 | 目的 commit 只有一個 parent；只多出 `side.txt`，內容 `from-side` |
| T22 | 核對 T19 的 C2 | `old.txt` 消失、`new.txt` 為 `old`、`gone.txt` 消失、`emoji.txt` 為 `你好 ✨`。預覽同時把 `old.txt` 顯示為刪除 |
| T23 | 看 `commits-dst-overwrite` 在一次成功重播之後 | `staged-keep.txt` 仍在 index；`local-only.txt` 仍是 untracked。T18 是 `pass` 之後才做。T18 是 `fail` 時，本格判定 `not-run`，證據欄寫「T18 未套用」 |
| T24 | 把 C4 貼到 `commits-dst-hooks` | 重播成功；`fixtures/hook-marker.txt` 不存在 |
| T25 | 只重播 C2 到新鮮的 `commits-dst-present`。預覽會列出這次對 `binary.dat` 的動作 | 規格 4.2：含 NUL 的刪除標成未複製，不寫入、不刪除目的端的 `binary.dat`，位元組仍是 `a`、NUL、`b`、換行。`binary.dat` 維持這組位元組時，這一格算進閘門：根目錄 `gone.txt` 已刪除且 `dir/gone.txt` 仍是 `folder-gone` 才是 `pass`；兩項有一項不成立是 `fail`。`binary.dat` 被刪掉或位元組變了時，依第 1 節跑 `git log --oneline --grep='不再重播刪除' -- crates/core`：沒有輸出就寫 `blocked-contract`，有輸出就寫 `fail` |
| T26 | 貼 C4 到乾淨目的 | 預覽把 `new-binary.bin` 標成 SKIP，摘要的建立／跳過與每一列一致；實際只提交 `newdir/content.txt` |
| T27 | 同一段 C4 再貼一次 | 第二個 SHA 不同，tree 與前一個相同 |
| T28 | 只貼 `empty replay` | 預覽有 message、作者與時間；Apply 後 `before..HEAD` 是 1，tree 不變 |
| T29 | 在 commit 預覽裡取消其中一個檔，再 Apply | 拒絕；HEAD 與檔案不變 |
| T30 | 有覆寫列但少勾一個 | 拒絕；沒有部分 commit |
| T31 | 把 C4 貼到 `ws-dst/repo04`（`newdir` 是一般檔） | 拒絕；HEAD、status、`newdir` 的位元組不變。訊息指出 `newdir` 與「不是目錄」 |
| T32 | 在 C2 的變更檔案裡，對 `gone.txt` 右鍵複製，貼到新鮮的 `commits-dst-present`。範圍只有這一個檔 | `[APP:MENU_ACTION: copy-files]`；payload 含 `[DELETED]` 與刪除前內容 `gone`；根目錄 `gone.txt` 消失；`binary.dat` 仍是初始位元組；`dir/gone.txt` 仍是 `folder-gone` |
| T33 | `perf15` 上 `log-filter-repo` 選一個 repo，`btn-log-regex` 輸入能命中的字 | 列只剩該 repo、且 subject 符合的 commit |
| T34 | 在 log 裡實際滾動 | 載入筆數增加（50 的倍數往上）。把前後的 `[APP:GRAPH_LOADED: commits=N]` 寫進 `action.json` |
| T35 | 搜尋尚未載入的較早 commit | 找到該 SHA，預覽指出它 |
| T36 | `perf15`、全部 repo，三種視窗寬度各看前 50 列 | 每一列的 subject 有可辨識文字。只有右側詳情有 message、列上是空白，就是 `fail` |
| T37 | `btn-repo-selector` 實際切換 100 次，涵蓋 15 個 repo | 100 次都有新的選取日誌；最後停在指定的 repo；程序還在。耗時含觀察，不當回應時間 |
| T38 | 沿用 [memory-measurement-protocol.md](memory-measurement-protocol.md) 的取樣時才記 RSS | 本規程不判記憶體。這格固定 `not-run`，原因寫「改走記憶體規程」 |
| T39 | Cmd+Shift+W 關掉，再 Cmd+Shift+O 打開一個 repo | 關閉時 `[APP:READY_REPOS: 0]`；重開成功；前後剪貼簿 SHA-256 相同 |
| T40 | Cmd+Q | 程序消失，exit code 0；這輪出現過的 git 子程序都不在 |

T03 與 T18 的 `action.json` 要寫兩欄：畫面上同相對路徑的列數，以及 `paste-overwrite:` 的相異 ID 數。列數大於相異 ID 數時，判定寫 `fail`，證據欄寫 `identity-fail`。

#46 之後多出來的格子，40 項矩陣沒有：

| ID | 點什麼 | 通過線 |
|---|---|---|
| B41 | 變更檔案上 Cmd 點兩列、Shift 點出範圍，右鍵複製 | 選取集合與畫面順序一致；資料夾展開後同一檔只出現一次 |
| B42 | 開 `fixtures/basket-src`，選 message 為 `basket folder and delete` 的 commit。變更檔案裡對資料夾 `dir`（`commit-dir:dir`）右鍵，選複製檔案，貼到新鮮的 `commits-dst-present`。範圍就是這個資料夾：修改的 `dir/keep.txt` 與刪除的 `dir/gone.txt`。不要用 C2 | payload 有 `[DELETED] dir/gone.txt`，內容是刪除前的 `folder-gone`；另有 `dir/keep.txt`，內容 `keep-2`。貼上後 `dir/gone.txt` 消失，`dir/keep.txt` 是 `keep-2`（檔尾換行依檔案模式契約去掉）。根目錄 `gone.txt` 仍是 `gone`，`binary.dat` 仍是 `a`、NUL、`b`、換行。檔案模式若複製已刪除的二進位檔，內容是 `// This file has been deleted in this change`，貼上會刪掉目的端；規格 4.2 只管 commit 模式，那種選取不記成產品錯誤 |
| B43 | 多 repo log 看分支標籤 | 一列一個合併標籤，寬度不超過 320px；tooltip 含全部 ref |
| B44 | 多 repo 只點 `log-filter-repo`；另開一個單一 repo 工作區 | 單一 repo 工作區的 chip 是 `log-filter-paths`，篩選結果與所選 repo、路徑一致 |
| B-notify | 複製 `commits-src` 的 C4（這筆含未複製的 `new-binary.bin`） | 通知寫出第幾個 commit 少了哪些檔。只複製 C4 時，就是第 1 個 commit 少了 `new-binary.bin`。同一則通知還要有規格 4.2 的四個數：commit 數 1、檔案數 2（未複製的檔也算）、未複製檔數 1、字元數等於剪貼簿全文的 UTF-16 code unit 數（`commits.rs` 的 `copy_summary`）。少任何一項，而 commit 已經複製成功時，判定 `ui-defect` |

## 7. 寫這份規程時，`bd09e95` 上已經對過的程式

下面是開跑前的程式閱讀，不是操作者可以抄去充數的結果。受測 SHA 若已改過這些位置，以新的程式與第 6 節的通過線為準，舊判定作廢。

| 位置 | `bd09e95` 上的程式 | 操作者應有的判定 |
|---|---|---|
| `ui/paste.rs` 的 paste ID 只有 path | 同相對路徑的第二個覆寫控制項不會有自己的 ID | T03、T18 判定 `fail`，證據欄寫 `identity-fail` |
| commit 預覽的動作標籤 | binary 新增會畫成建立；rename 的舊路徑不一定畫成刪除；空 commit 預覽沒有 message／作者／時間 | T22、T26、T28：Git 對則 `ui-defect`，Git 也錯則 `fail` |
| 規格 4.2 與 commit 刪除含 NUL 的檔 | 規格要求不刪除；`bd09e95` 的重播仍可能套用刪除。預覽會列出 delete `binary.dat` | 依第 1 節。`bd09e95` 上 `git log --oneline --grep='不再重播刪除' -- crates/core` 沒有輸出，刪掉 `binary.dat` 時 T25 判定 `blocked-contract`。`binary.dat` 沒被刪時，依第 6 節用根目錄 `gone.txt` 與 `dir/gone.txt` 判 `pass` 或 `fail`，並把這一格算進閘門 |
| Git log 的 subject 欄 | 窄寬度下訊息欄可能只剩空白 | T36 以這一輪的截圖與列文字為準 |
| `paste_err_destination` | `bd09e95` 的 i18n 沒有這個 key，畫面會露出 key | T31：安全拒絕且檔案沒變，但訊息仍是 key 時為 `ui-defect` |
| 複製 commit 的通知 | `status_commits_copied` 只有「已複製 N 個 commit 至剪貼簿」，沒有未複製檔，也沒有第幾個 commit 少了哪些檔 | `bd09e95` 上複製 C4 後記 `ui-defect`，ID 寫 `B-notify`。通過線在第 6 節。通知寫出規格 4.2 要求的那幾項時改判 `pass` |
| 歷史刪除檔的 Copy Files | #46 已讓刪除檔以 `[DELETED]` 複製 | T32 必須本輪重測。`167c10c` 的「選單停用」作廢 |

2026-09-29 報告測的是 `main` 的 `167c10c`。那一輪的 PASS 不能抄進這份計分表。

## 8. 結論怎麼寫

`scorecard.md` 開頭三行：

```text
受測 SHA:
執行檔 SHA-256:
產品閘門: 打開 | 關閉
```

接著一張表：ID、判定、一句話證據（日誌行或 Git SHA）。產品閘門按第 1 節的名單關閉：18 步、T01–T22、T24、T26–T37、T39、T40、條件內的 T23、B41–B44、B-notify 全部是 `pass`。T25 的判定是 `pass`、`fail` 或 `not-run` 時也算進這個判斷；只有 `blocked-contract` 不參與。T38、C preflight 不參與。T25 是 `blocked-contract` 時，結論寫「二進位刪除的契約尚未決定」。第 9 節的項目尚未測，整個產品不在本規程宣告完成。

## 9. 這份規程不關閉的項目

Windows 與 Linux 的真實輸入、IME、跨機剪貼簿、與 ClipCode 的實際互貼、standard 15×10,000×20,000 負載、重播到第 N 個 commit 失敗後保留前面幾個、磁碟滿、權限、缺 user identity、預覽後 HEAD 被改的完整矩陣、symlink、控制字元路徑、非 UTF-8 檔名、submodule、sparse checkout、linked worktree、light theme 的完整操作。

規格 5.1 寫了系統匣。`crates/desktop-native` 沒有對應控制項。找不到選單列圖示時記成規格落差，不在本規程找圖示，也不寫進計分表。

檔案模式和 IDE 套件的逐位元組契約由 `fixtures/clipboard-contract.json` 與 core／CLI 測試負責。本規程不重跑那一套。

記憶體是否持續成長，只按 [memory-measurement-protocol.md](memory-measurement-protocol.md) 在 Linux 上量。T37 的 100 次切換只證明程序還在、最後停在指定 repo。

## 10. 計分表

複製到 `$RUN/scorecard.md` 後填寫。

```text
受測 SHA:
執行檔 SHA-256:
產品閘門:

| ID | 判定 | 證據 |
|---|---|---|
| file-a-to-b-pair01 | | |
| file-b-to-a-pair09 | | |
| file-explicit-valid-root-pair01-to-pair15b | | |
| commit-a-to-b-pair02 | | |
| commit-b-to-a-unmerged-pair03 | | |
| commit-merge-first-parent-pair04 | | |
| commit-root-pair05 | | |
| commit-octopus-first-parent-pair01 | | |
| commit-rename-delete-pair08 | | |
| neg-cross-repo-commits | | |
| neg-noncontiguous-tips | | |
| neg-mapping-collision | | |
| neg-mapping-ambiguous-basename | | |
| neg-mapping-missing-destination | | |
| neg-stale-source | | |
| neg-stale-target | | |
| neg-overwrite-unauthorized | | |
| neg-cancel | | |
| T01 | | |
| T02 | | |
| T03 | | |
| T04 | | |
| T05 | | |
| T06 | | |
| T07 | | |
| T08 | | |
| T09 | | |
| T10 | | |
| T11 | | |
| T12 | | |
| T13 | | |
| T14 | | |
| T15 | | |
| T16 | | |
| T17 | | |
| T18 | | |
| T19 | | |
| T20 | | |
| T21 | | |
| T22 | | |
| T23 | | |
| T24 | | |
| T25 | | |
| T26 | | |
| T27 | | |
| T28 | | |
| T29 | | |
| T30 | | |
| T31 | | |
| T32 | | |
| T33 | | |
| T34 | | |
| T35 | | |
| T36 | | |
| T37 | | |
| T38 | | |
| T39 | | |
| T40 | | |
| B41 | | |
| B42 | | |
| B43 | | |
| B44 | | |
| B-notify | | |
| C preflight | | |
```
