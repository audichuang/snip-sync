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

`scorecard.md` 每一格只能是這四個字之一：

| 判定 | 什麼時候用 |
|---|---|
| `pass` | 控制項身分、點下去之後的新日誌、剪貼簿、Git／檔案四者都符合該格的通過線 |
| `ui-defect` | Git／檔案符合通過線，預覽、顏色、計數或通知和實際動作不一致 |
| `fail` | 寫錯、拒絕錯、點不到指定控制項、或日誌對不上。點不到時證據欄寫 `missing-control`；同路徑兩列共用一個 ID 時證據欄寫 `identity-fail`。這兩個詞不是判定 |
| `not-run` | 這一格沒有做 |

計分表只有這四個判定。

受測 SHA 必須包含 `f5247ab`（`fix(core): 刪除二進位／非 UTF-8 檔的 commit 標為未複製，貼上不再重播刪除 (#55)`）。用 `git merge-base --is-ancestor f5247ab <SHA>` 檢查，結束碼不是 0 時本規程不適用，不開跑。規格 4.2 已定案：含 NUL 或非 UTF-8 的檔案在 commit 模式標成未複製，不寫入、不刪除目的端。本規程也預設受測 SHA 含 #56（`bc81086`）、#57（`dcbc079`）、#58（`0a45ca1`）、#59（`ed3d057`）、#75（`b07aace`）。用同一個 `merge-base` 命令逐一檢查；缺哪一個，第 7 節表上對應的格子就不適用，證據欄寫「缺 #NN」，閘門打開。

產品閘門只看下面這些格，每一格都是 `pass` 才寫「關閉」：

- 閘門 A 的 18 步
- T01–T22、T24–T37、T39、T40
- T23。T18 是 `pass` 時它納入閘門；T18 是 `fail` 時它寫 `not-run`，證據欄寫「T18 未套用」，不另算一次失敗
- B41–B44、B-notify
- 第 6 節末的 commit 預覽與鍵盤格：C-group、C-detail、C-reason、C-nonutf8、C-blocked、K-space、K-reinclude、K-fold、K-nav、K05-key
- 第 6 節最後一張表：I-config、L-chip、T17-toast、T18-filter、K-cmdc、B-group、B-compare、T13b、T-dirfile、T-budget、R-reader、B-nav

R-reader 與 B-nav 也納入閘門：每一步都有自己的日誌行或探針，再加剪貼簿、Git 或磁碟的 oracle，強度和其他格相同。

T25 和其他格一樣，只判 `pass`、`fail`、`not-run`。它不再有例外。

T31 與 C-blocked 照各自的通過線判定，沒有「預期 `ui-defect`」。

下面兩格要填，但不決定閘門關不關：

- T38：判定固定 `not-run`，證據欄寫「改走記憶體規程」
- C preflight：在受測 SHA 的乾淨 checkout 上跑 `just preflight`。macOS 會先跑 CI 的 macOS 檢查，再在 Apple `container` VM 裡跑 Linux 關卡（見 AGENTS.md）。結束碼 0 判 `pass`，非 0 判 `fail`，證據欄寫失敗的關卡；沒有跑就判 `not-run` 並寫原因

閘門裡有 `ui-defect`、`fail` 或 `not-run`，第一句就寫「產品閘門打開」，並列出那些 ID。第 9 節的項目尚未測，整個產品不在本規程宣告完成。

重現一個已知問題，代表這輪有觀察到它。只要該 ID 在閘門裡，產品閘門就打開。

## 2. 開跑

在 macOS 上用 `scripts/real_ui_round.py` 做本節與收尾，不要手工拼環境（之前幾輪因為縮放、`.app`、未隔離的設定與上一輪殘留整輪作廢）：

```text
python3 scripts/real_ui_round.py prepare --sha <SHA> --run "$RUN"   # 殘留檢查、worktree 建置、.app、三組 fixture、空的 config、真實設定快照、存剪貼簿、environment.json
python3 scripts/real_ui_round.py launch --gate b --run "$RUN"       # 帶隔離環境變數啟動，日誌寫進 $RUN/app-gate-b.log
python3 scripts/real_ui_round.py resize 900 600 --run "$RUN"        # 依程序與視窗標題縮放，等到 VIEWPORT 相符；不符就失敗
python3 scripts/real_ui_round.py point <控制項 ID> --run "$RUN"      # 第 3 節第 1–4 步：最新 CTRL_BOUNDS、換算螢幕點、action.json 草稿
python3 scripts/real_ui_round.py finish --run "$RUN"                # Cmd+Q、exit code、殘留、剪貼簿還原、真實設定比對（I-config）、移除 worktree
```

`prepare` 發現殘留時拒絕開跑並列出來；`--clean-leftovers` 只清能證明屬於先前回合的東西。腳本做不到的步驟照下面的手動流程做，並在證據裡寫明。下面的手動流程也是腳本的規格。

1. 向派工的人確認受測 SHA。沒有另外指定時，用 `origin/develop`。
2. 在乾淨的 worktree 或 checkout 上建置。不要拿別的分支的 `target/` 混用。

下面的賦值用 `export`。bash、zsh 與 fish 都能執行。這台機器的預設 shell 是 fish。fish 不接受沒有接命令的 `RUN=...`（會報 `Unsupported use of '='`）。fish 3.1 起，`VAR=value 命令` 會把值傳給那個命令。

```text
export RUN="$HOME/snip-sync-ui-runs/$(date +%Y-%m-%d)-$(git rev-parse --short HEAD)"
mkdir -p "$RUN" "$RUN/config"
ls -A "$RUN/config"
git rev-parse HEAD
cargo build -p snip-desktop-native --locked
shasum -a 256 target/debug/snip-desktop-native
```

後面的命令都用這個 `$RUN`。`export-hold` 先不要建立；檔案不存在時，複製不會停住。

`$RUN/config` 是這一輪的設定資料夾。`ls -A` 必須沒有輸出；不是空的就換一個新的 `$RUN`。每一次啟動都帶 `SNIP_CONFIG_DIR="$RUN/config"`，最近開啟的工作區與遠端紀錄只寫進這裡。空資料夾也代表設定全是預設值。少了這個變數，E2E 模式下最近開啟的工作區不會存（`recent::config_dir` 回傳空），B-nav 的 `workspace-recent:` 會缺。

3. 殘留檢查。第一次啟動前做一次，之後每一次啟動前再做一次：

```text
pgrep -fl snip-desktop-native
ls -la "$RUN/export-hold"
```

`pgrep` 必須沒有輸出。受測 binary 與安裝的 `snip-sync.app` 的執行檔都叫 `snip-desktop-native`，兩者都算。`ls` 必須回報檔案不存在。任一項不成立就停：不殺程序、不刪檔，也不在它上面開測。把兩行命令的輸出寫進 `$RUN/leftover.txt`，回報派工的人。第一次啟動前就有殘留時，計分表全部寫 `not-run`；之後某次啟動前才有，還沒做的格寫 `not-run`。證據欄都寫「開跑前有殘留」。

4. 真實設定資料夾快照，給 I-config 用。位置是 `crates/remote/src/lib.rs` 的 `default_config_dir` 在沒有 `SNIP_CONFIG_DIR` 時的 macOS 路徑。第一次啟動前拍一次：

```text
export REAL="$HOME/Library/Application Support/com.audichuang.snip-sync"
ls -la "$REAL" > "$RUN/real-config-before.txt" 2>&1
find "$REAL" -type f -exec shasum -a 256 '{}' + 2>&1 | sort > "$RUN/real-config-before.sha"
find "$REAL" -type f -exec stat -f '%m %z %N' '{}' + 2>&1 | sort > "$RUN/real-config-before.mtime"
```

最後一個 App 按 Cmd+Q、確認 exit code 之後，把三行裡的 `before` 換成 `after` 再跑一次。判法見 I-config。

5. 跑兩個 fixture。閘門 B：

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

6. 讀受測 SHA 的 `crates/desktop-native/src/paste.rs` 的 `control_id`，把格式 `paste-<row|include|overwrite>:<ix>:<path>` 抄進 `environment.json`。`ix` 是計畫內的項目索引，從 `CTRL_BOUNDS` 讀，不要自己算。若受測 SHA 的格式仍是只有路徑（`paste-row:<path>`），代表缺 #56。第 1 節只擋缺 `f5247ab`，缺 #56 時不擋：T03、T18 不適用，證據欄寫「缺 #56」，閘門打開；其餘格子的 ID 改照該 SHA 印出的格式。
7. 啟動 App。探針只有在 `SNIP_NATIVE_E2E=1` 時才會印。`SNIP_NATIVE_E2E_EXPORT_HOLD_FILE` 在 `WorkbenchModel::new` 讀一次，所以必須寫在啟動命令裡。`SNIP_CONFIG_DIR` 寫在每一行啟動命令前面，不靠 shell 裡的 export。不要加 `--restore-dir`，貼上目的地由畫面決定。每一次啟動的 stdout 與 stderr 寫進該次自己的日誌，否則讀不到 `CTRL_BOUNDS`。

同一時間只開一個 App。閘門 A 與閘門 B 各啟動一次，各寫一份日誌。先做哪一個都可以。換下一個之前，對目前這個按 Cmd+Q，等到 exit code 0，再啟動。第二次若用 `>` 寫進第一次的檔，會把 `CTRL_BOUNDS` 與 `PASTE_DONE` 截掉。

同一個 shell 裡，下面三個 export 做一次即可。新開 shell 要再 export。

```text
export SNIP_NATIVE_E2E=1
export SNIP_THEME=dark
export SNIP_NATIVE_E2E_EXPORT_HOLD_FILE="$RUN/export-hold"
```

閘門 B：

```text
SNIP_CONFIG_DIR="$RUN/config" ./target/debug/snip-desktop-native \
  --workspace "$RUN/gate-b/fixtures/ws-src" \
  > "$RUN/app-gate-b.log" 2>&1
```

閘門 A：

```text
SNIP_CONFIG_DIR="$RUN/config" ./target/debug/snip-desktop-native \
  --workspace "$RUN/gate-a/machine-a" \
  > "$RUN/app-gate-a.log" 2>&1
```

閘門 A 的工作區若不是 `machine-a`，改成該 fixture 實際的路徑。自己啟動它，結束時才讀得到 exit code。同一個 App 還開著時，換工作區用畫面上的開工作區，不要為了換目錄再疊一個程序。

8. `environment.json` 至少包含：受測 SHA、執行檔 SHA-256、兩次啟動命令、`SNIP_CONFIG_DIR` 的路徑、兩組真實設定資料夾快照的檔名、`app-gate-a.log` 與 `app-gate-b.log` 的路徑、三個視窗尺寸實際量到的寬高、paste ID 格式、fixture 目錄。

視窗要跑三種邏輯尺寸：1080×752（對照 2026-09-29 報告的 BUG-04）、1080×720 與 900×600（驗收規格）。把視窗調到該尺寸後，從這一次啟動的日誌抄下最新的 `[APP:VIEWPORT: WxH]`。這行是實體像素，Retina 上會比邏輯尺寸大。兩組數字都留下。功能 oracle 在三種尺寸相同；T36 每種尺寸各判一次。

快捷鍵（`main.rs` 的 `key_bindings`）：

| 按鍵 | 作用 | 日誌 |
|---|---|---|
| Cmd+C | 左側工具視窗游標所在的節點，等同它右鍵的「複製」；專案視窗是選取的列；閱讀器有選取文字時複製文字；Git log 有焦點時複製選取的 commit | `COPY_DONE`、`SELECTION_COPIED`、`COPY_COMMITS_DONE` |
| Cmd+V | 開預覽 | `PASTE_PREVIEW` 或 `PASTE_ERR` |
| Cmd+R | 重新整理，同 `btn-refresh` | 新的 `DISCOVERY_PROGRESS` 與 `READY_REPOS` |
| Cmd+Q | 結束 | `phase=drained intent=quit` |
| Cmd+Shift+O／Cmd+Shift+W | 開工作區／關工作區 | `WORKSPACE: state=` |
| Option+1／Option+0／Option+9 | 專案／變更／Git log | |
| Option+Shift+R | 選 repo | `SELECTOR_OPEN` |
| Option+L | 切換語言 | `LOCALE` |
| Option+N／Option+P | Git log 載入下一頁／上一頁。焦點不能在文字框 | `LOG_WINDOW: first_page= last_page= rows=` |
| Ctrl+F | 閱讀器有焦點：開尋找列並聚焦 `find-input`。Git log 有焦點：聚焦 `log-search-input`，沒有日誌 | `FIND_BAR: open`、`FIND: matches= first_line=` |
| Ctrl+G | 開尋找列並聚焦 `goto-input`，輸入行號按 Enter | `GOTO: line=`；超出範圍是 `GOTO_INVALID` |
| F3／Shift+F3 | 下一個／上一個符合。沒有符合時不印 | `FIND_AT: i/n line=` |
| F7／Shift+F7 | 下一個／上一個變更區塊 | `DIFF_STEP: line=` |
| Ctrl+A | 閱讀器全選。拖曳選取放開滑鼠時才印日誌，Ctrl+A 不印 | 拖曳：`SELECTION: from= to= chars=` |
| `h` | Git log 有焦點：回到 HEAD，同 `btn-head` | `HEAD_LOCATED: row=` |
| Tab／Shift+Tab、Ctrl+Tab／Ctrl+Shift+Tab | 焦點移到下一個／上一個 | `FOCUS: next`／`FOCUS: prev` |
| Enter／Escape／Space／Up／Down | 預覽面板裡：套用／取消／切換目前列／移動選取 | `PASTE_APPLYING`、`PASTE_CANCELLED`、`PASTE_SEL_TOGGLED`、`PASTE_NAV` |

Cmd+C、Cmd+V、Cmd+R、Cmd+Q、Cmd+Shift+O、Cmd+Shift+W 也綁了 Ctrl 版本；操作按 Cmd，並用日誌確認送到的是那個動作。Ctrl+F、Ctrl+G、Ctrl+A、Ctrl+Tab 只有 Ctrl 版本，Cmd+F、Cmd+G 沒有綁定。Mac 鍵盤的 F3、F7 可能要加 Fn。在這個 App 裡按 Cmd+C 是 App 自己的複製。

Shell 可以建立 fixture、用 argv 叫 Git、把 Git 輸出寫進檔再算雜湊、做 T13、T13b、neg-stale-* 規定的外部修改、用 `pbcopy` 放入 sentinel、T15 的普通文字或 T-budget 的超大 payload、用 `pbpaste` 讀剪貼簿。Shell 不執行 `snip copy` 或 `snip paste` 來代替畫面上的複製與貼上。

## 3. 怎麼點

每一次點擊都做完這七步。少一步，該格就是 `fail`。

1. 從這一次啟動的日誌取這個控制項最新的一行。閘門 A 讀 `$RUN/app-gate-a.log`，閘門 B 讀 `$RUN/app-gate-b.log`。

```text
[APP:CTRL_BOUNDS: id=<完整 ID> x= y= w= h=]
```

ID 整段相等。`paste-overwrite:0:common.txt` 不能拿去點 `paste-overwrite:2:common.txt`，也不能拿去點 `paste-overwrite:0:common.txt.bak`。不要照 native-e2e driver 的寬鬆查法（`lookup_bounds` 仍接受只給路徑的 `paste-overwrite:<path>`）；人工操作一律用完整 ID。

2. 沒有這一行，或 `w < 1`，或 `h < 1`，或這一行之後又出現同一 ID 的 `[APP:CTRL_GONE: id=<ID>]`（控制項已收合或關閉，矩形是舊的）：判定 `fail`，證據欄寫 `missing-control`，停止。點擊前要確認沒有比最新 `CTRL_BOUNDS` 更晚的 `CTRL_GONE`。改點旁邊看起來像的列，不算測過。
3. 同一 ID 出現兩列是回歸。此時兩列輪流以同一個 ID 回報矩形，日誌可能在兩個矩形之間來回跳，也可能只剩一個，所以不要用日誌矩形的數量辨識；改用 T03／T18 的規則：相異 ID 數要等於同路徑列數，並比對 `PASTE_TOGGLED` 的 idx。不成立時判定 `fail`，證據欄寫 `identity-fail` 與兩欄數字，不能按 Apply。
4. 把 bounds 換算成螢幕上的點，再點那個點。`x`、`y`、`w`、`h` 是實體像素，原點在視窗內容區的左上角，不是螢幕，也不是含標題列的視窗框。`physical()` 乘了 `scale_factor`。直接點 `(x + w/2, y + h/2)` 在 Retina 上會偏到大約兩倍遠。換算順序：

   - 讀最新的 `[APP:VIEWPORT: Wp x Hp]`。
   - 讀 `snip-desktop-native` 視窗框的螢幕位置與邏輯尺寸，以及視窗框中心所在那一塊螢幕的 `backingScaleFactor`。視窗框含標題列。外接螢幕的 scale 可以和主螢幕不同；用主螢幕的值會把點算錯。
   - `scale` 用 `backingScaleFactor`。`Wp` 應等於內容區邏輯寬乘 `scale`。對不上就判定 `fail`，證據欄寫算式與兩個寬度，不要點。
   - 標題列高度 = 視窗框邏輯高 − (`Hp / scale`)。內容區左上角 = 視窗框左上角的 x，以及視窗框的 y 加上標題列高度。
   - 螢幕點 = 內容區左上角 + `((x + w/2) / scale, (y + h/2) / scale)`。點擊工具若吃邏輯點，用這個螢幕點。若吃實體像素，再乘 `scale`。`action.json` 同時留下原始 bounds、`scale`、內容區左上角與實際點到的螢幕點。

   Linux 的 native-e2e 用 `xdotool mousemove --window`，座標就是內容區裡的實體像素，而且那時 `scale` 是 1。那套算法不要用在這台 Mac 的螢幕游標上。

5. 點完之後必須有一行新日誌，而且裡頭的 idx、path、prefix 或 SHA 就是這一格。點下去之前就存在的日誌不算。例外：按停用的按鈕（例如對應還沒選完時的 `btn-apply`），通過線就是「1 秒內沒有任何新的 `PASTE_*` 行」。hover 看 tooltip 不算點擊；座標取所屬列的 `CTRL_BOUNDS`。另一個例外：下列點擊本來就不印日誌，改用該格寫的畫面轉錄或下一步的日誌判定：`log-branch-group:<key>`（展開分支群組）、`commit-file:` 的 Cmd 點與 Shift 點、為了取得焦點點一下 `reader`。
6. 捲動之後矩形會變。捲完再讀一次 bounds，用新的中心點重算螢幕點。
7. 然後才跑該格的剪貼簿與 Git／檔案 oracle。

`CTRL_BOUNDS` 是控制項真正看得到的部分（已扣掉被父容器裁掉的範圍）；整個被裁掉時是 `CTRL_GONE`。視窗寬度至少 700 時，App 還會稽核每一幀：

- `[APP:CTRL_COVERED: id=<A> by=<B>]`：A 的可見中心被後畫的 B 蓋住，點 A 的中心會點到 B。不要點；這一格判 `ui-defect`，證據欄寫 `covered A by B`。若已經點了而觸發 B 的動作，判 `fail`。
- `[APP:CTRL_DUPLICATE: id=<ID>]`：同一幀有兩個控制項用同一個 ID。涉及這個 ID 的格子判 `fail`，證據欄寫 `identity-fail`。

macOS 輔助使用在這個 App 上只到視窗。沒有 `CTRL_BOUNDS` 的點擊，不能記 `pass`。

### 3.1 會用到的控制項

ID 以受測 binary 印出的 `CTRL_BOUNDS` 為準。下面是寫這份規程時程式裡的名字；binary 印出的不同時，照 binary 點，並在證據欄寫下差異。

| 位置 | ID |
|---|---|
| 左軌 | `rail-project`、`rail-changes`、`rail-log` |
| 工作區 | `btn-workspace-menu`、`btn-open-workspace`、`btn-workspace-open-confirm`、`btn-close-workspace`、`btn-open-folder`。工作區選單裡最近開啟的是 `workspace-recent:<ix>`（0 是最新的一個），點擊後是 `[APP:WORKSPACE: state=open path=<該路徑> generation=N]` |
| 工具列 | `btn-repo-selector`、`btn-ref-selector`、`btn-paste`、`btn-refresh` |
| 右鍵選單 | `context-menu`、`menu-item:copy-files`（「複製」：只複製選單所在的那個節點）、`menu-item:show-diff`、`menu-item:copy-path`、`menu-item:copy-relative-path`；Git log 的 commit 列另有 `menu-item:copy-commits`（複製目前選取的 commit，等同 `btn-copy-commits`）。日誌 `[APP:MENU_OPEN: Left items=…]`、`[APP:MENU_ACTION: <id>]`、`[APP:MENU_CLOSED]` |
| 選單列 | `pick-repo:<名稱>`；只有同名的 repo 才用 `pick-repo:<序號>:<名稱>`（新產生的閘門 A 在 machine-a 只有 `billing` 與 `docs` 兩個同名，其他都是 `pick-repo:<名稱>`）。`pick-ref:<名稱>` |
| 變更列 | `change-row@<repo>:<staged\|unstaged\|untracked\|conflicted>:<path>`，沒有勾選框；右鍵「複製」複製這一個檔案（帶它的來源）。目前選取中的 repo 才同時有舊 ID `change-row:<source>:<path>` |
| 變更群組 | `change-header:<群組>`、`change-repo:<群組>:<repo>`、`change-repo-toggle:<群組>:<repo>`、`change-dir:<群組>:<repo>:<path>`。右鍵「複製」：群組列是所有 repo 在該群組的檔案，repo 列是該 repo 在該群組的檔案，資料夾列是該 repo、該群組、該資料夾底下的檔案 |
| 專案列 | `tree-row:<path>`、`ws-tree-row:<path>`。點擊選取該列，Ctrl/Cmd 點擊加入或移出選取（`[APP:TREE_TOGGLED: <path>]`），Shift 點擊選範圍。右鍵「複製」複製目前選取的列（資料夾整個走訪）；右鍵點在選取之外會先單獨選取那一列 |
| Git log | `commit-row:<repo>:<7 字元 SHA>`；目前選取 repo 的列另外有 `commit-row:<7 字元 SHA>`。`btn-copy-commits`、`btn-log-regex`（開關，日誌 `[APP:LOG_REGEX: true\|false]`）、`btn-log-refresh`（`[APP:LOG_REFRESH]`）、`btn-head`、`btn-compare`（有兩端點的選取才啟用，`[APP:COMPARE: from= to=]`）、`log-search-input` |
| log 顯示 | `btn-log-more` → `log-more:hash`（hash 欄，預設關）。日誌 `[APP:LOG_VIEW: hash]` |
| 多 repo 篩選 | `log-filter-repo`、`log-repo:<名稱>`、`log-repo-check:<名稱>` |
| 單一 repo 工作區的路徑篩選 | `log-filter-paths`。多 repo 工作區不要去點這個 ID |
| log 篩選 chip | 分支 `log-filter-branch`、使用者 `log-filter-user`、日期 `log-filter-date`。窄視窗（如 900×600）寬度不足時，容納不下的 chip 會收合至溢位 chip `log-filter-more`，點擊後於選單展開隱藏項 `log-filter-more:<key>`（如 `log-filter-more:user`、`log-filter-more:date`）開啟對應篩選選單。未啟用的 chip 先收合，已啟用的盡量留在列上；已啟用而被收合的篩選，在溢位選單裡有清除項，ID 與 chip 上的清除鈕相同（`log-filter-<key>-clear`）。打開選單印 `[APP:LOG_MENU: Some(<選單>)]`（`Repo`、`Branch`、`User`、`Date`、`FilterMore`），再點同一個 chip 關上印 `[APP:LOG_MENU: None]`，點外面關上不印；chip 的收合變了才印 `[APP:LOG_CHIPS: avail= widths= more= shown=<4 位元>]`，`shown` 由右往左依序是 repo（或路徑）、分支、使用者、日期，1 是在列上 |
| 篩選選單項目 | 使用者 `log-user:me`、`log-user:<名稱>`，選了印 `[APP:LOG_SEARCH: active=… author=true]`；分支選單打開時群組全收合，先點 `log-branch-group:<key>` 展開（本機分支是 `refs_local`，沒有日誌），再點 `log-branch:<完整 ref>`（如 `log-branch:refs/heads/side`），日誌 `[APP:REF_FILTER: <完整 ref>]`，清除後是 `[APP:REF_FILTER: all]` |
| 歷史檔案 | `btn-browse-tree:<完整 SHA>`、`rev-row:<path>`、`btn-leave-tree`。檔案列右鍵「複製」複製該檔在這個 commit 的內容；資料夾列的「複製」是停用的 |
| Git log 變更檔案 | `commit-file:<path>`、`commit-dir:<path>`。資料夾列的選取 key 是 `<path>/`。左鍵資料夾是展開或收合；複製用右鍵 |
| 貼上 | `btn-apply`、`btn-cancel`、`paste-row:<ix>:<path>`、`paste-include:<ix>:<path>`、`paste-overwrite:<ix>:<path>`（`ix` 是計畫內的項目索引，由 `crate::paste::control_id` 產生）、`paste-items` |
| commit 預覽 | `paste-commit:<c>`（`c` 從 0 起算，是該 commit 的標頭，點擊收合或展開）、`paste-commit-count`、`paste-commit-whole` |
| 多 repo 對應 | `paste-mappings`、`paste-map:<prefix>`、`paste-map-target:<prefix>`、`paste-map-pick:<prefix>:<idx>`、`paste-map-keep:<prefix>` |
| 閱讀器 | `reader`（內文）、`reader-truncated-notice`（截斷提示）、`editor-error`（讀不到或超過 1 MiB）、`find-bar`、`find-input`、`goto-input`、`btn-find-next`、`btn-find-prev`、`btn-diff-mode`（diff 的整合／並排切換，日誌 `[APP:DIFF_MODE: Inline\|SideBySide]`；貼上預覽開著時它在預覽的詳情區，兩處不會同時畫出） |
| 通知 | `copy-toast`：複製結果的卡片。成功約 4 秒、失敗約 12 秒後消失 |

Apply 被拒時，日誌一律是 `[APP:PASTE_STALE_DETECTED: <i18n key>]`，連不是 stale 的拒絕也用這個 tag。看 key 判斷是哪一種拒絕：`stale_modified`、`commit_subset_rejected`、`commit_overwrite_required`、`commit_replay_partial`（已建立部分 commit 後中途非版面衝突失敗）、`commit_replay_partial_refused`（已建立部分 commit 後因版面衝突被拒）、`commit_replay_refused`（沒有建立任何 commit 就被拒）等。

其他會用到的日誌 tag：`PASTE_SEL_TOGGLED: idx= state=`（勾選是否納入）、`PASTE_TOGGLED: idx= state=`（允許覆寫）、`PASTE_COMMIT_TOGGLED: idx=<c>`、`PASTE_NAV: idx=`、`PASTE_PLAN_CLEARED`、`COPY_COMMITS_DONE: commits=N`、`TOAST: ok=`、`LOG_VIEW: hash`、`WORKSPACE: state=`、`APPLY_IGNORED:`。

第 6 節最後一張表另外用到：`PASTE_DIFF: idx= state=`（檔案模式可覆寫列的 diff；`state` 是 `diff`、`same` 或 `unavailable`）、`DIFF_MODE:`、`COPY_PREP: files=`、`COPY_DONE: copied=N`、`COPY_COMMITS_ERR:`、`RANGE: commits=N chain=`、`COMPARE: from= to=`、`E2E_CHANGES: files=N`（commit 或比較的變更檔案清單載入完成）、`COMMIT_SELECTED:`、`HEAD_LOCATED: row=`、`REF_FILTER:`、`LOG_MENU:`、`LOG_CHIPS:`、`LOG_SEARCH: active= author=`、`LOG_REGEX:`、`LOG_REFRESH`、`TREE_TOGGLED:`、`TREE_SELECTED:`、`SELECTION_COPIED: chars= fnv=`、`FIND_BAR: open`、`FIND: matches= first_line=`、`FIND_AT:`、`GOTO: line=`、`CHANGES_EMPTY: state=`、`LOG_EMPTY: state=`、`DISCOVERY_PROGRESS:`、`READY_REPOS:`。

`[APP:CTRL_GONE: id=<ID>]`：控制項這一幀沒有畫出來（收合、關閉、換頁）。`CTRL_BOUNDS` 只在控制項是新的或位置變了才印，所以「沒有新的 `CTRL_BOUNDS`」不能證明控制項消失，要看 `CTRL_GONE`。控制項再出現時會重新印 `CTRL_BOUNDS`。

右鍵點列時，用該列的 bounds。選單出現後會有 `context-menu`，項目是 `menu-item:copy-files` 等，照第 3 節的點法點項目，通過線是新的一行 `[APP:MENU_ACTION: copy-files]`。

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
- commit 模式：`[APP:PASTE_DONE: created=N overwritten=0 skipped=0 deleted=0 errors=0 commits=N]`，`N` 是 commit 數，不是檔案數；`overwritten`、`skipped`、`deleted` 固定是 0。各格預期：T18、T19 是 `created=3 commits=3`；T24、T25、T26、T27、T28 是 `created=1 commits=1`。`git rev-list --count <before>..HEAD` 等於通過線的筆數。新 commit 的 SHA 不必等於來源 SHA。要比作者名稱、email、作者時間與時區、完整 message（含換行）、parent 數為 1，以及通過線點名的路徑位元組。
- 沒有點到的 repo：HEAD 與 status 雜湊不變。
- 負向步驟：快照等於該步規定的那一張，剪貼簿 sentinel 的 SHA-256 不變，並且有一行對得上的新拒絕日誌。畫面沒反應、按鈕看起來是灰的、或只有一句通用錯誤，都是 `fail`。
- 例外：通知、紅色橫幅、詳情列、commit 標頭、ref 標籤的文字不寫進日誌（`show_toast` 只印 `[APP:TOAST: ok=]`，ref 標籤沒有 probe）。下列格子的文字 oracle 就是截圖加畫面文字轉錄，寫進 `action.json` 的 `screen_text`：B-notify、B43、T36、T22、T25、T26、T28、T31，以及 C-group、C-detail、C-reason、C-nonutf8、C-blocked、K-space、K-reinclude、K05-key。另外，這些格有通過線依賴畫面上不進日誌的文字，也把該文字轉錄進 `screen_text`：T02（`btn-apply` 的提示「請先為每個來源前綴選擇目的地…」）、T13 與 neg-stale-target（「目的地檔案已在外部修改: …」）、T15（狀態列「剪貼簿內容不是有效的 snip-sync payload」）、T29（`commit_subset_rejected` 橫幅）、T16 的空狀態「這個資料夾裡沒有 Git 儲存庫」、T17-toast 的通知卡、T13b 的拒絕橫幅、T-dirfile 與 T-budget 的狀態列、R-reader 的截斷提示、B-notify 與 T17-toast 的通知消失時間。其餘格子的截圖仍不計分。
- 拒絕文字檢查：T02、T13、T13b、T15、T17-toast、T18-filter、T29、T30、T31、T-dirfile、T-budget、C-blocked、K-space、所有 `neg-*` 的每一次拒絕，都要轉錄狀態列、預覽紅色橫幅與通知的文字。任何一處出現符合 `[a-z]+(_[a-z0-9]+)+` 的原始 key（例如 `paste_err_destination`、`commit_subset_rejected`），判 `ui-defect`（K05-key）。

每筆 Git 自己有逾時。比對時設 `GIT_CONFIG_GLOBAL=/dev/null` 與 `GIT_CONFIG_NOSYSTEM=1`。

每格目錄：

| 檔案 | 內容 |
|---|---|
| `action.json` | 控制項 ID、bounds、中心點、點下之後那行新日誌 |
| `clipboard-before.bin` / `clipboard-after.bin` | 與兩邊的 SHA-256 |
| `snapshot-before.json` / `snapshot-after.json` | HEAD、status 雜湊、涉及路徑的 SHA-256 |
| `oracle.json` | 這一格預先寫下的預期 |
| `screen.png` | 當時畫面。它證明畫面長這樣，不參與通過與否；下面的例外格除外 |
| `screen_text`（寫進 `action.json`） | 例外格的畫面文字轉錄 |

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

1. `btn-repo-selector` → `pick-repo:<名稱>`；只有同名的 `billing` 與 `docs` 用 `pick-repo:<序號>:<名稱>`，其他 repo 沒有序號。等 `[APP:REPO_SELECTING]` 的 canonical root 與 manifest 的 `relativePath` 一致。basename 相同的 repo 不能只看名字。
2. 檔案步驟走 `rail-changes`，展開 `change-repo-toggle:<群組>:<repo>`，點 `change-row@<repo>:<source>:<path>` 看預覽；fixed OID 走 `btn-browse-tree:<完整 SHA>` 再點 `rev-row:<path>`。
3. 對同一列右鍵，等 `[APP:MENU_OPEN: Left items=copy-files,…]`，點 `menu-item:copy-files`。等 `[APP:COPY_DONE]`。剪貼簿讀回 SHA-256 等於複製當下。一次「複製」只讀一個節點，而一個檔案步驟的操作混了不同來源（worktree、index、fixed OID、刪除），所以每個操作各自走一次第 2–6 步（複製、貼上、套用），全部套用完才跑 `compare-step`。
4. 換到目的工作區，`btn-paste`。`[APP:PASTE_MAP_CANDIDATE]` 在 `[APP:PASTE_PREVIEW]` 之前出現，從按貼上之前的日誌位置開始讀。
5. 點 `paste-map-pick:<prefix>:<idx>`，等 `[APP:PASTE_MAPPED]` 的 `dest` 就是那個 canonical root。已存在且內容會變的檔才點覆寫。
6. `btn-apply`。等 `[APP:PASTE_DONE: ...]`，再跑 `compare-step`。

Commit 步驟把第 2–3 步換成：`rail-log`，點起點 `commit-row:<7 字元>`（多 repo 合併的 log 用 `commit-row:<repo>:<7 字元>`），Shift 點終點，等 `[APP:RANGE: commits=N chain=first_parent]`（非 first-parent 鏈退回 `chain=visual`），再點 `btn-copy-commits`。點完等 `[APP:COPY_COMMITS_DONE: commits=N]` 與 `[APP:TOAST: ok=true]`。

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
| `neg-stale-source` | 啟動命令裡已經有 `SNIP_NATIVE_E2E_EXPORT_HOLD_FILE`。選好變更列後對它右鍵，用 `pbcopy` 放入 sentinel，建立 `$RUN/export-hold`，再點一次 `menu-item:copy-files`。等到新的 `[APP:EXPORT_PLAN_READY: files=N]`（N 至少 1），改來源檔，拍快照，刪掉 hold 檔。同一次複製會接著跑。不要再按一次複製，也不要等選完才去設環境變數。這一步結束時 `$RUN/export-hold` 必須不存在。中途失敗而檔還在，先刪掉它，證據欄寫「hold 殘留，已刪」，然後才做後面的檔案模式複製。檔留著的話，之後每一次檔案模式複製都會停在 `EXPORT_PLAN_READY` | `[APP:COPY_FAILED: stale_source]` 與 `[APP:COPY_IDLE]`；沒有 `COPY_DONE`；sentinel 不變；快照等於改完之後、刪掉 hold 之前那張；`$RUN/export-hold` 不存在 |
| `neg-stale-target` | 預覽出現後改目的端，再 Apply | `[APP:PASTE_STALE_DETECTED: stale_modified]`（`stale_created`、`stale_deleted` 看外部改法）；畫面文字是「目的地檔案已在外部修改: …」；快照等於改完之後、Apply 之前那張 |
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
| `fixtures/file-dst` | 選用。一個一般檔（不是資料夾）；當工作區開會被 `workspace_bad_path` 擋下，到不了貼上。K05-key 的貼上目的地用 `nongit-dst` |
| `fixtures/files-src` | 檔案模式來源：staged 新增／修改／刪除／rename，以及 worktree 上另一份 `both.txt` |
| `fixtures/files-dst` | 檔案模式目的。已追蹤 `overwrite.txt`（`do-not-touch`）、`both.txt`（`dest-both`）、`gone.txt`（`gone-dest`）、`old-name.txt`（`old-dest`）、`binary.dat`（`keep-bin`）、`folder/utf16.txt`（`keep-utf16`） |
| `fixtures/commits-src` | first-parent 為 base、C1「多行中文」、C2、C3 merge、C4、empty replay。`side` 上另有 `SIDE` |
| `fixtures/commits-dst-overwrite` | 已有 `common.txt`，分支 `qa-replay`，另有 staged 與 untracked |
| `fixtures/commits-dst-clean` | 沒有 `common.txt`，分支 `qa-replay` |
| `fixtures/commits-dst-hooks` | 四種 hook 都 `exit 1` 並寫 `fixtures/hook-marker.txt` |
| `fixtures/commits-dst-present` | 已追蹤 `binary.dat`（`a`、NUL、`b`、換行）、根目錄 `gone.txt`（`gone`）、`old.txt`（`old`）、`dir/gone.txt`（`folder-gone`），分支 `qa-replay`。T25、T32、B42 各從這份初始樹開始。T25 與 T32 留下 `dir/gone.txt` |
| `fixtures/basket-src` | B42 的來源。`basket base` 有 `dir/keep.txt`（`keep`）與 `dir/gone.txt`（`folder-gone`）。下一筆 `basket folder and delete` 把 `dir/keep.txt` 改成 `keep-2`，並刪除 `dir/gone.txt`。沒有 `binary.dat` |
| `fixtures/nonutf8-src` | C-nonutf8 的來源。base 有 `latin1.txt`（位元組 `caf` `e9` 換行）。N1 把它改成 `caf` `e9` `!` 換行並新增 `ok.txt`，N2 刪除 `latin1.txt` |
| `fixtures/nonutf8-dst` | 已追蹤 `latin1.txt` 與 `common.txt`，兩者都是 `caf` `e9` 換行（非 UTF-8），分支 `qa-replay` |
| `fixtures/blocked-src` | C-blocked 的來源。一筆 `B1 blocked dir and fresh` 新增 `newdir/x.txt` 與 `fresh.txt` |
| `$RUN/perf15` | Git log 版面、搜尋、100 次切換 |

C1 的 message 是 `多行中文`、空行、`第二段`。C2 改 `common.txt`、把 `old.txt` 改名 `new.txt`、刪除 `gone.txt` 與含 NUL 的 `binary.dat`、新增 `emoji.txt`。這些路徑都在 `commits-src` 的根目錄。rename 在預覽裡是兩列（`old.txt` 與 `new.txt`），所以 C2 是 6 列，不是 5，這不算缺陷。C3 是 merge，first parent 是 C2，對 first parent 的 diff 是 `side.txt`。C4 新增 `newdir/content.txt` 與含 NUL 的 `new-binary.bin`。

預設單檔上限是 500 KiB（`Settings::max_file_size_kb`）。`large.txt` 是 1,360,000 byte，複製時應被略過。

目的地就是目前選取的 repo（`current_restore_destination`：有選 repo 取它的 root，沒有 repo 取工作區根目錄）。`commits-dst-*`、`nonutf8-dst` 各自用 `btn-workspace-menu` → `btn-open-workspace`，輸入路徑後按 `btn-workspace-open-confirm` 開啟，等 `[APP:WORKSPACE: state=open path=<該路徑> ...]` 與 `[APP:READY_REPOS: 1]`。`repo04` 是開 `ws-dst` 後點 `pick-repo:repo04`。`nongit-dst` 開啟後 `READY_REPOS` 是 0，目的地是工作區根目錄。`PASTE_PREVIEW` 的 `dest=` 必須等於這個路徑。

複製 commit 的來源是先開 `commits-src`（或該格指定的來源）工作區，`rail-log`，選 commit，`btn-copy-commits`，等 `COPY_COMMITS_DONE`，再換工作區貼上。

本節的 `ix`、標頭字串、摘要計數，是依 `paste.rs`、`ui/paste.rs`、`ui/mod.rs` 的原始碼推得，core 的動作與略過原因用 CLI（`snip paste --dry-run`／`--apply --stdin`，同一段 core）在這批 fixture 上核對過，沒有在真實視窗實點過。畫面與這裡不符時，以畫面與 `CTRL_BOUNDS` 為準，先判斷是規程寫錯還是產品錯，證據欄寫清楚。

每一格開始前，若該目的 repo 已被上一格改過，就重建 fixture 或換一個新目錄，並當場寫下 before SHA。不要用上一格的結果回推這一格的 before。

T19 即使通過，也不清除 T18。目的端一開始沒有 `common.txt` 時，兩列都還不是覆寫，Apply 可以按；身份檢查要看的是兩列覆寫各自能點。

| ID | 點什麼 | 通過線 |
|---|---|---|
| T01 | 以 `fixtures/ws-src` 開工作區 | `[APP:READY_REPOS: 15]`，選擇器裡 15 個 root |
| T02 | 貼上時先不選 `paste-map-pick`，再選一次 | 未選時 `PASTE_PREVIEW` 的 `mapping=false`；點 `btn-apply` 後 1 秒內沒有任何新的 `PASTE_*` 行（按鈕停用，tooltip 是「請先為每個來源前綴選擇目的地…」）；按 Enter 得到 `[APP:PASTE_ERR: mapping_required]`；點 `paste-map-pick:<prefix>:<idx>` 後出現 `[APP:PASTE_MAPPED: prefix=… dest=<絕對路徑> items=N]`，絕對路徑正確 |
| T03 | repo01 與 repo03 的 `unrelated.txt` 各點 `paste-overwrite:<ix>:<path>` | 兩個不同 ID，兩次 `[APP:PASTE_TOGGLED]` 的 idx 分別等於所點 ID 的 `ix` 段；套用後覆寫 2、跳過 0，兩邊位元組分別是 `from-repo01` 與 `from-repo03`。身份規則見本節末 |
| T04 | repo02／repo05 的 `only-src.txt` 與 repo06／repo07 的 `shared.txt` | 建立 2、覆寫 2；其餘 repo 的 HEAD 與 status 不變 |
| T05 | `files-src` 的 staged 新增、修改、刪除、rename，複製後貼到新鮮的 `files-dst`。預覽裡點 `paste-row:<ix>:both.txt` 選取這一列，再點 `paste-overwrite:<ix>:both.txt` 勾覆寫。詳情區出現 diff 後點 `btn-diff-mode` 兩次 | payload 含 `[NEW] staged-new.txt`、`[MODIFIED] both.txt`（內容 `index-body`）、`[DELETED] gone.txt`、`[MOVED] new-name.txt`。沒有舊路徑的刪除項。選取 `both.txt` 列得到 `[APP:PASTE_NAV: idx=<ix>]` 與 `[APP:PASTE_DIFF: idx=<ix> state=diff]`；預覽一開就選在這一列時，點擊不會再印，以預覽出現後那一行 `PASTE_DIFF` 為準。詳情區是目標檔目前內容（`dest-both`）↔ 剪貼簿內容（`index-body`）的 diff。`btn-diff-mode` 每點一次得到一行 `[APP:DIFF_MODE: SideBySide]` 或 `[APP:DIFF_MODE: Inline]`，兩行的值不同；並排與整合兩種畫面都看得到 `dest-both` 與 `index-body`（轉錄進 `screen_text`）。只有可覆寫的檔案模式列（目的端已存在、動作是建立或覆寫；`main.rs` 的 `load_paste_diff`）會印 `PASTE_DIFF`；`staged-new.txt`、`new-name.txt`、`gone.txt` 沒有這一行不是失敗。貼上後建立 2、覆寫 1、刪除 1、跳過 0：建立 `staged-new.txt` 與 `new-name.txt`、覆寫 `both.txt` 為 `index-body`、刪除 `gone.txt`。`old-name.txt` 仍是 `old-dest`。沒勾 `both.txt` 的覆寫時，結果是跳過 1，這一格不能算過 |
| T06 | 同一路徑 `both.txt` 分別從變更列（index）與 Project（worktree）複製 | 兩次 payload 的位元組分別等於 index 的 `index-body` 與 worktree 的 `worktree-body` |
| T07 | 複製 `folder/`。這個資料夾有 7 個檔：`a.txt`–`e.txt`、`bin.dat`、`utf16.txt`。`large.txt` 不在裡面 | 通知是已複製 5、略過 2。略過的是 `bin.dat` 與 `utf16.txt`。目的端建立 `a.txt`–`e.txt`；`folder/utf16.txt` 仍是 `keep-utf16`；不建立 `folder/bin.dat` |
| T08 | 複製來源的 `binary.dat` 與 `folder/utf16.txt`，貼到新鮮的 `files-dst` | payload 沒有這兩個檔的可還原內容。`files-dst/binary.dat` 仍是 `keep-bin`，`files-dst/folder/utf16.txt` 仍是 `keep-utf16` |
| T09 | 複製 `large.txt` | payload 是超過大小的 placeholder；貼上不建立 `large.txt` |
| T10 | 複製 `empty.txt` | 目的端檔案存在且長度 0 |
| T11 | 複製 `路徑 有空白/檔案.txt` 與 `crlf.txt` | 路徑與 `中文` 正確；CRLF 變 LF，首尾空行與檔尾換行依檔案模式契約去掉 |
| T12 | 預覽裡取消一個新檔（`paste-include:<ix>:<path>`），不勾 `overwrite.txt` 的覆寫 | 取消的檔不出現（`[APP:PASTE_SEL_TOGGLED: idx=<ix> state=false]`）；`overwrite.txt` 仍是 `do-not-touch` |
| T13 | 預覽出現後在外面改目的檔，再 Apply | `[APP:PASTE_STALE_DETECTED: stale_modified]`（`stale_created`、`stale_deleted` 看外部改法）；畫面文字是「目的地檔案已在外部修改: …」；外面寫入的內容保留；沒有部分寫入 |
| T14 | 預覽後 Escape | `[APP:PASTE_CANCELLED]`；HEAD 不變 |
| T15 | 用 `pbcopy` 放入普通文字 `QA invalid clipboard payload`，再按貼上。不要在這個 App 裡按 Cmd+C | 新的一行 `[APP:PASTE_ERR: paste_err_not_payload]`（`[APP:PASTE_ERR: clipboard]` 只在剪貼簿本身讀不出來時才有，這裡不會有），狀態列顯示「剪貼簿內容不是有效的 snip-sync payload」；之前若有預覽開著，先出現 `[APP:PASTE_PLAN_CLEARED]`；不出現 `btn-apply`（日誌有 `[APP:CTRL_GONE: id=btn-apply]`，或從未有過它的 `CTRL_BOUNDS`）；目的端零寫入 |
| T16 | 目的地改為 `fixtures/nongit-dst`，保留 repo 前綴。開這個工作區之後，先點 `rail-changes` 看變更，再看 Git log 面板（沒開就按 Option+9；它是開關，開著時再按會關掉） | 目錄裡沒有 `.git`；同時有保留前綴的路徑與去掉前綴的路徑，內容與來源一致。空狀態：`[APP:CHANGES_EMPTY: state=no_repository]` 與 `[APP:LOG_EMPTY: state=no_repository]`，兩處畫面文字都是「這個資料夾裡沒有 Git 儲存庫」（`changes_no_repository`、`log_no_repository`）。出現 `state=clean` 或「沒有變更」之類的乾淨狀態，判 `ui-defect` |
| T17 | 在 `commits-src` 把 `SIDE` 和 main 上不能組成 first-parent 鏈的 commit 一起 Shift 選取，再複製 | 出現 `[APP:RANGE: commits=N chain=visual]`；點複製出現 `[APP:COPY_COMMITS_ERR: commits are not contiguous:]`；sentinel 不變 |
| T18 | Shift 選取 C1 到 C3（出現 `[APP:RANGE: commits=3 chain=first_parent]`，直接沿 first-parent 鏈選取，無需任何變通或篩選；若篩選隱藏了鏈中間的 commit，會退回可見列選取，複製時被拒絕），貼到 `commits-dst-overwrite` 的 `qa-replay` | `PASTE_PREVIEW` 的 `items=8`。兩列 `common.txt` 的 ID 是 C1 `paste-overwrite:0:common.txt`、C2 `paste-overwrite:2:common.txt`。勾兩個覆寫之前摘要覆寫 0/2；C1 標頭是「1 個檔案，0 個不寫入」，C2 標頭是「6 個檔案，3 個不寫入」。各點一次得到 `PASTE_TOGGLED idx=0 state=true`、`idx=2 state=true`，Apply 得到 `PASTE_DONE created=3 … commits=3`，`git rev-list <before>..HEAD` 是 3。身份規則見本節末 |
| T19 | 同一段貼到 `commits-dst-clean` | `PASTE_DONE created=3 … commits=3`；3 個新 commit；C1 的 `common.txt` 在 Git 裡是新增 |
| T20 | 核對 T19 那 3 個 commit | `git log -1 -z --format=%an%x00%ae%x00%aI%x00%B` 與來源逐欄相等。C1 含多行中文與空行。作者時間是 fixture 寫入的 `+08:00` |
| T21 | 核對 T19 的 merge 那一筆 | 目的 commit 只有一個 parent；只多出 `side.txt`，內容 `from-side` |
| T22 | 看 T19 貼上前的 C2 預覽（`commits-dst-clean` 沒有 `old.txt` 與 `gone.txt`），再核對 T19 的結果 | 預覽：C2 的 `old.txt` 列帶 `→ new.txt`，動作是「跳過」，詳情列（24px 那條）是「目的地不存在，無需刪除」，下方無紅色區塊「將刪除目的地檔案，不寫入內容」（若出現紅色區塊則視為回歸，判 `ui-defect`）；`new.txt` 列帶 `← old.txt`，動作是「建立」；`gone.txt` 列也是「跳過」加同一句詳情，下方同樣無紅色區塊。結果：`new.txt` 為 `old`、`emoji.txt` 為 `你好 ✨`、`old.txt` 與 `gone.txt` 不存在。刪掉目的端真有的舊路徑，由 T25 點過 |
| T23 | 看 `commits-dst-overwrite` 在一次成功重播之後 | `staged-keep.txt` 仍在 index；`local-only.txt` 仍是 untracked。T18 是 `pass` 之後才做。T18 是 `fail` 時，本格判定 `not-run`，證據欄寫「T18 未套用」 |
| T24 | 把 C4 貼到 `commits-dst-hooks` | 重播成功（`PASTE_DONE created=1 … commits=1`）；`fixtures/hook-marker.txt` 不存在 |
| T25 | 只複製 C2，貼到新鮮的 `commits-dst-present`（含 `old.txt`） | 預覽：`paste-row:0:binary.dat` 動作「跳過」，詳情「未複製：二進位檔，不寫入也不刪除」；`old.txt` 列動作「刪除」加 `→ new.txt`；`new.txt` 列「建立」加 `← old.txt`；`gone.txt` 列「刪除」；C2 標頭「6 個檔案，1 個不寫入」；摘要建立 3、刪除 2、跳過 1。複製 C2 的通知含「1 個檔案未複製：#1 binary.dat」（`已複製 1 個 commit（5 個檔案、N 字元）至剪貼簿…`；CLI 實測 5 個檔案、636 字元，rename 算一個檔；桌面端的字元數以 pbpaste 的 UTF-16 code unit 數為準）。Apply：`PASTE_DONE created=1 … commits=1`。Git：`binary.dat` 仍是 `a`、NUL、`b`、換行；根目錄 `gone.txt` 已刪；`old.txt` 已刪；`new.txt` 為 `old`；`dir/gone.txt` 仍是 `folder-gone`。任何一項不成立是 `fail` |
| T26 | 貼 C4 到 `commits-dst-clean` | 預覽：`paste-commit:0` 標頭是「#1 C4 text and binary … 2 個檔案，1 個不寫入」；`paste-row:0:new-binary.bin` 是「跳過」，`paste-row:1:newdir/content.txt` 是「建立」；摘要建立 1、跳過 1，與每列一致。`PASTE_DONE created=1 … commits=1`；`git show --name-only` 只有 `newdir/content.txt` |
| T27 | 同一段 C4 在 T26 之後再貼一次 | 第二次預覽中 `newdir/content.txt` 是「待允許覆寫」；點 `paste-overwrite:1:newdir/content.txt` 得到 `[APP:PASTE_TOGGLED: idx=1 state=true]`，再按 Apply，得到 `PASTE_DONE created=1 … commits=1`。第二個 SHA 與第一個不同，兩者的 `git rev-parse <sha>^{tree}` 相同 |
| T28 | 只貼 `empty replay` 到 `commits-dst-clean` | 預覽：`paste-commit-count` 是「1 個 commit」，`paste-commit:0` 標頭有 message、作者、日期與「無檔案異動，仍會建立空 commit」，沒有任何 `paste-row:`；`btn-apply` 可按。Apply 得到 `PASTE_DONE created=1 … commits=1`；`before..HEAD` 是 1，tree 不變 |
| T29 | 用 T19 的設定開預覽。點 `paste-include:7:side.txt` | `[APP:PASTE_SEL_TOGGLED: idx=7 state=false]`，該列變「已排除」，面板立刻出現紅色橫幅「提交重放必須套用整段內容與中繼資料。取消任一項會在寫入前拒絕，不會只寫其餘檔案」。按 `btn-apply` 得到 `[APP:PASTE_STALE_DETECTED: commit_subset_rejected]`；沒有 `PASTE_DONE`；`rev-list --count` 為 0，HEAD 與檔案不變 |
| T30 | 用 T18 的設定開預覽，只點 `paste-overwrite:0:common.txt`，不點 `paste-overwrite:2:common.txt`，按 `btn-apply` | `[APP:PASTE_STALE_DETECTED: commit_overwrite_required]`；`rev-list --count` 為 0，沒有部分 commit。commit 模式下 `btn-apply` 在這一格是啟用的，因為 `executable()` 只看 commit 預覽 |
| T31 | 開 `fixtures/ws-dst` 工作區，`btn-repo-selector` → `pick-repo:repo04`，Cmd+V 貼上 C4 | 見下方 T31 說明 |
| T32 | 在 C2 的變更檔案裡，對 `gone.txt` 右鍵（該列 bounds），點 `menu-item:copy-files`，貼到新鮮的 `commits-dst-present`。範圍只有這一個檔 | `[APP:MENU_ACTION: copy-files]`；payload 含 `[DELETED]` 與刪除前內容 `gone`；根目錄 `gone.txt` 消失；`binary.dat` 仍是初始位元組；`dir/gone.txt` 仍是 `folder-gone` |
| T33 | `perf15` 上 `log-filter-repo` 選一個 repo。點 `btn-log-regex` 打開正規表示式，在 `log-search-input` 輸入能命中的字。做完再點一次 `btn-log-regex` 關掉 | 打開時 `[APP:LOG_REGEX: true]`，關掉時 `[APP:LOG_REGEX: false]`；輸入後 `[APP:LOG_SEARCH: active=true author=false]`。列只剩該 repo、且 message 符合的 commit。多 repo 視圖的列 ID 是 `commit-row:<repo>:<7 字元 SHA>`。`btn-log-regex` 是開關，不是輸入框；開關狀態會留到下一格，所以結束前要關掉 |
| T34 | 在 log 裡實際滾動 | 載入筆數增加（50 的倍數往上）。把前後的 `[APP:GRAPH_LOADED: commits=N]` 寫進 `action.json`。每頁 50 筆符合程式；規格 4.1 寫 300，記在第 9 節 |
| T35 | 搜尋尚未載入的較早 commit | 找到該 SHA，預覽指出它 |
| T36 | `perf15`、全部 repo。1080×752、1080×720、900×600 三種尺寸，各分 hash 欄關（預設）與開（`btn-log-more` → `log-more:hash`，日誌 `[APP:LOG_VIEW: hash]`）兩種，各看前 50 列。先從任一 `commit-row:` 的 `w÷scale` 抄下列表寬，1080 應約 574、900 應約 433；偏差超過 10px 時寫下實際值並說明側欄狀態 | 每列的 subject 至少有 1 個可辨識字元；被截斷時結尾是省略號，hover 可看到完整 subject。只有右側詳情有 message、列上是空白，就是 `fail`。各狀態的預期：1080、hash 關：ref 標籤未被捨棄（寬度 ≥ min(自然寬度, 80)，短的 ref 如 `main` 會比 80px 窄）、subject 至少 160px，兩者同時可見；1080、hash 開：graph 欄寬超過 64px 時標籤被捨棄；64px 以下時標籤保留，寬度為 min(自然寬度, 80)（欄寬本身上限 72px），兩種都算對，subject 至少 160px；900、hash 關：標籤依設計一律捨棄（與 graph 欄寬無關，#59 的 `a_narrow_list_squeezes_date_then_author_after_the_gutter` 也是 `labels == 0`），subject 至少 160px；900、hash 開：graph 欄寬 72px 時 subject 約 97px（#59 已記錄的下限），欄較窄時會更寬（例如 40px 欄約 129px），數值不當作判準，只要有可辨識前綴就算 `pass`，空白是 `fail`。graph 欄與 subject 儲存格、ref 標籤都沒有 probe（`log-gutter:` 只是 debug selector，不進 `CTRL_BOUNDS`），所以寬度只能從截圖量：graph 欄寬是列的左緣到 subject 文字起點的距離，再減 8px 間距。`collapse:<key>` 的位置是節點所在車道，不是欄的右緣，不能拿來算欄寬。graph 欄寬、subject 寬度與標籤有無量測後寫進 `screen_text`。文字 oracle 見第 4 節 |
| T37 | `btn-repo-selector` 實際切換 100 次，涵蓋 15 個 repo（`pick-repo:<名稱>`，名稱從 `CTRL_BOUNDS` 讀） | 100 次都有新的選取日誌；最後停在指定的 repo；程序還在。耗時含觀察，不當回應時間 |
| T38 | 沿用 [memory-measurement-protocol.md](memory-measurement-protocol.md) 的取樣時才記 RSS | 本規程不判記憶體。這格固定 `not-run`，原因寫「改走記憶體規程」 |
| T39 | Cmd+Shift+W 關掉，再 Cmd+Shift+O 打開一個 repo | 關閉後出現 `[APP:WORKSPACE: state=closed generation=N]`；重開後出現 `[APP:WORKSPACE: state=open path=<該 repo> generation=N+1]`，接著 `[APP:READY_REPOS: 1]`；前後剪貼簿 SHA-256 相同 |
| T40 | Cmd+Q | 程序消失，exit code 0；這輪出現過的 git 子程序都不在 |

T03 與 T18 的身份規則：`action.json` 要寫兩欄，尾段路徑相同的相異 ID 數，以及畫面上同路徑的列數。相異 ID 數要等於列數；而且每次點擊後 `PASTE_TOGGLED` 的 idx 等於所點 ID 的 `ix` 段。任一條不成立：`fail`，證據欄寫 `identity-fail`。

**T31 說明。** 預覽、Apply、Git 三項都符合，而且判定規則的 (a)(b)(c) 都不成立，才是 `pass`。

- 預覽：`paste-commit:0` 標頭是「#1 C4 text and binary (整個 commit 會被拒絕) … 2 個檔案，1 個不寫入」（只有 `new-binary.bin` 算不寫入；`newdir/content.txt` 是「拒絕」列，不算「不寫入」）；摘要「1 個 commit（1 個被拒絕）」，建立 0、覆寫 0/0、刪除 0、跳過 1。預覽一開就有紅色橫幅「第 1 個 commit「C4 text and binary」會被拒絕，重播將在此停止」（`commit_will_be_refused`）。點 `paste-row:1:newdir/content.txt` 得到 `[APP:PASTE_NAV: idx=1]`，詳情列是「整個 commit 會被拒絕：父目錄被檔案佔住」。點 `paste-row:0:new-binary.bin`，詳情列是「未複製：二進位檔，不寫入也不刪除」。
- Apply：按 `btn-apply` 得到新的一行 `[APP:PASTE_STALE_DETECTED: commit_replay_refused]`，沒有 `PASTE_DONE`，預覽仍開著，紅色橫幅是「沒有建立任何 commit；第 1 個 commit「C4 text and binary」被拒絕：父目錄被檔案佔住」。英文介面是「No commit was created; commit #1 "C4 text and binary" was refused: a file is in the way of its parent directory」。
- Git：HEAD、status 雜湊、`newdir` 位元組（`not-a-directory` 加換行）都不變。有任何寫入，或出現 `PASTE_DONE`，判 `fail`。
- 設計：core 的 `replay_commit` 在動任何檔案之前，先用 `layout_conflicts` 發現父目錄被一般檔佔住，整個 commit 拒絕。這符合規格 4.3（「父目錄被一般檔案佔住時，整個 commit 拒絕且不動任何檔案（預覽標為 UNSAFE_PATH）」）。重播依序進行、停在失敗的 commit，之前已建立的 commit 保留，所以只有「沒建立任何 commit」時才用 `commit_replay_refused`，已建立過才用 `commit_replay_partial_refused`（若為版面衝突）或 `commit_replay_partial`（若為非版面衝突錯誤）。預覽與 dry-run 依序規劃：每個 commit 都以「前面未被拒絕的 commit 已寫入」的模擬版面為準（新增、刪除、清空的父目錄都算），所以較早的 commit 移除擋路檔案時，較晚的 commit 不會被標成拒絕；較早的 commit 建出擋路檔案時，較晚的 commit 會在預覽就標為拒絕，與 Apply 一致。覆寫確認（寫入列的 existed）仍看真實磁碟，因為它保護的是重播前就存在的檔案；刪除列與改名舊路徑（old_existed）看模擬版面。符號連結與非 UTF-8 檢查看真實磁碟，但只認「完全相同路徑」的覆蓋：較早的 commit 刪掉某個符號連結或非 UTF-8 檔後，較晚的 commit 再寫同一個路徑，預覽與 Apply 一致。已知限制（預覽與 dry-run 可能與 Apply 不一致；Apply 本身依規格 4.3 執行，在已重現的情境中未觀察到既有檔案被破壞，但這不是對所有配置的保證）：(a) 較早的 commit 刪掉符號連結目錄後，較晚的 commit 寫入其下的路徑（例如先刪 `link`、再寫 `link/x.txt`），預覽仍會穿過舊連結看真實磁碟，可能誤報拒絕或跳過；(b) 在不分大小寫的檔案系統（macOS、Windows 預設）上，與較早 commit 的路徑只差大小寫時，可能誤報筆數與停止點。追蹤於 https://github.com/audichuang/snip-sync/issues/76。
- 判定規則：Git 與日誌都對，但符合下列任一項，就判 `ui-defect`，並把三項各自的真假寫進證據欄：(a) 橫幅含英文 core 原因（zh-TW 介面）或 `(none)`；(b) 預覽（標頭、摘要、詳情、紅色橫幅或 `btn-apply` 狀態）沒有標出整個 commit 會被拒；(c) 橫幅說「重放中途失敗」而 `rev-list --count` 為 0。三項都不成立才是 `pass`。
- 附註：#57 的 commit 說明寫「同一個 commit 的其他有效檔案照常貼上」，與實際行為不符，以規格為準。CLI 的 `--dry-run` 現在在被拒的 commit 標題後印「(refused: …)」，結尾印「N commit(s) would be created; replay stops at commit #M (refused); K not reached.」。

#46 之後多出來的格子，40 項矩陣沒有：

| ID | 點什麼 | 通過線 |
|---|---|---|
| B41 | 多選只在兩處有：Git log 的變更檔案（`commit-file:<path>`）與專案樹（`tree-row:<path>`）。變更列 `change-row@…` 沒有多選，Cmd 點與 Shift 點都只是單選，不在這格測。(1) `commits-src` 選 C2，點 `commit-file:<a>`，Shift 點 `commit-file:<c>`，再 Cmd 點中間一列 `commit-file:<b>` 把它移出；對任一選取中的列右鍵，點 `menu-item:copy-files`。(2) `files-src` 的專案樹點 `tree-row:folder`（展開並選取），Cmd 點 `tree-row:folder/a.txt`；右鍵選取中的列，點 `menu-item:copy-files`。(3) 再點一列 `tree-row:<x>`，Shift 點另一列 `tree-row:<y>` | (1) `commit-file:` 的選取沒有日誌，以畫面的反白轉錄加 payload 判定：`[APP:COPY_DONE: copied=N]`，payload 的檔案集合等於反白的列，順序與畫面一致，`<b>` 不在裡面。(2) 得到 `[APP:TREE_TOGGLED: folder/a.txt]`；複製得到 `[APP:COPY_DONE: copied=5]`，payload 裡 `folder/a.txt` 只出現一次（資料夾走訪與單選的檔重疊不重複）。(3) 得到 `[APP:TREE_SELECTED: …]`，列出的路徑就是畫面上 `<x>` 到 `<y>` 之間的列，順序相同 |
| B42 | 開 `fixtures/basket-src`，選 message 為 `basket folder and delete` 的 commit。變更檔案裡對資料夾 `dir`（`commit-dir:dir`）右鍵，點 `menu-item:copy-files`，貼到新鮮的 `commits-dst-present`。範圍就是這個資料夾：修改的 `dir/keep.txt` 與刪除的 `dir/gone.txt`。不要用 C2 | payload 有 `[DELETED] dir/gone.txt`，內容是刪除前的 `folder-gone`；另有 `dir/keep.txt`，內容 `keep-2`。貼上後 `dir/gone.txt` 消失，`dir/keep.txt` 是 `keep-2`（檔尾換行依檔案模式契約去掉）。根目錄 `gone.txt` 仍是 `gone`，`binary.dat` 仍是 `a`、NUL、`b`、換行，`old.txt` 仍是 `old`。檔案模式若複製已刪除的二進位檔，內容是 `// This file has been deleted in this change`，貼上會刪掉目的端；規格 4.2 只管 commit 模式，那種選取不記成產品錯誤 |
| B43 | 只在 1080×720、hash 欄關時判定。多 repo log 看 ref 標籤 | 有 ref 的列只有一個合併標籤，寬度不超過 320 邏輯 px，而且與 subject 同時可見。合併標籤的 tooltip 列出它顯示的 ref；有 `+N` 時，`+N` 的 tooltip 列出全部 ref。900 或 hash 欄開時標籤依設計捨棄，不判 B43。文字 oracle 見第 4 節 |
| B44 | 多 repo 只點 `log-filter-repo`；另開一個單一 repo 工作區 | 單一 repo 工作區的 chip 是 `log-filter-paths`，篩選結果與所選 repo、路徑一致 |
| B-notify | 複製 `commits-src` 的 C4（這筆含未複製的 `new-binary.bin`） | `[APP:COPY_COMMITS_DONE: commits=1]`、`[APP:TOAST: ok=true]`；狀態列與通知文字是「已複製 1 個 commit（2 個檔案、373 字元）至剪貼簿；1 個檔案未複製：#1 new-binary.bin」。373 是實測值，等於 pbpaste 內容的 UTF-16 code unit 數（`commits.rs` 的 `copy_summary`）。成功的通知（`ok=true`）約 4 秒後消失，失敗的通知（`ok=false`，例如 T17）約 12 秒後消失（`main.rs` 的 `show_toast`）；兩種消失後文字都仍可從狀態列讀取。消失時間從 `TOAST` 那行起算，各記一次秒數，差 1 秒內算對。缺任何一項，而 commit 已經複製成功時，判定 `ui-defect` |

commit 預覽與鍵盤的格子，對應 #58 與 #57。除非該格另有說明，貼到 `commits-dst-clean`，來源是 `commits-src` 的 C1–C3。每一格都從新開的預覽開始（Escape 之後再 Cmd+V）。

| ID | 點什麼 | 通過線 |
|---|---|---|
| C-group | 開 T19 的預覽（`commits-dst-clean`） | `paste-commit-count` 是「3 個 commit」；`paste-commit:0`、`paste-commit:1`、`paste-commit:2` 依重播順序，文字依序是「#1 多行中文 QA Operator <qa@example.com> 2026-03-02 09:00 1 個檔案，0 個不寫入」、「#2 C2 rename delete emoji … 2026-03-03 09:00 6 個檔案，3 個不寫入」、「#3 C3 merge … 2026-03-05 09:00 1 個檔案，0 個不寫入」。標頭 tooltip 是完整 message 加作者加 ISO 時間。點 `paste-commit:1` 後出現 `[APP:PASTE_COMMIT_TOGGLED: idx=1]`，畫面上 C2 的 6 列消失，日誌有 6 行 `[APP:CTRL_GONE: id=paste-row:1:binary.dat]` … `[APP:CTRL_GONE: id=paste-row:6:new.txt]`，以及對應的 `paste-include:1:…`–`paste-include:6:…` 的 `CTRL_GONE`（`paste-row` 的完整 ID 抄自 `CTRL_BOUNDS`）；再點一次列回來，這 6 個 ID 各有新的 `CTRL_BOUNDS`。只有「沒有新的 `CTRL_BOUNDS`」不算證據。摘要是建立 5、覆寫 0/0、刪除 0、跳過 3。T18 在 `commits-dst-overwrite` 上的 C2 標頭同樣是「6 個檔案，3 個不寫入」 |
| C-detail | T18 的預覽，點 `paste-row:0:common.txt` | `[APP:PASTE_NAV: idx=0]`；詳情標題是「common.txt → <目的端絕對路徑>」，內容以「commit: 多行中文」開頭。commit 重播列目前維持來源內容檢視，規格 4.3 的 commit 模式 diff 維持記為規格落差（第 9 節），不判這格 `fail`。檔案還原模式之覆寫列（規格 3.2）則必須顯示 diff（見 T05） |
| C-reason | 在 T19／T22（`commits-dst-clean`）、T26、T31、T25 的預覽裡，逐列點 `paste-row:<ix>:<path>` | 每次出現 `[APP:PASTE_NAV: idx=<ix>]`，詳情列依序是：二進位（T26 的 `new-binary.bin`、T25 的 `binary.dat`）「未複製：二進位檔，不寫入也不刪除」；版面衝突拒絕（T31 的 `newdir/content.txt`）「整個 commit 會被拒絕：父目錄被檔案佔住」；目的端不存在的刪除（只在 T19／T22 的預覽有，`commits-dst-clean` 的 `old.txt`、`gone.txt`；T25 在 `commits-dst-present` 上同一列是「刪除」）「目的地不存在，無需刪除」。每一列的詳情都不是「此檔案不會寫入」（`reason_skip_generic`）這句泛用話。目的端不存在的刪除列被選取時，下方不顯示紅色區塊「將刪除目的地檔案，不寫入內容」（`reason_delete`）；若出現紅色區塊則視為回歸，判 `ui-defect` |
| C-nonutf8 | 開 `nonutf8-src`，選 N1、N2 複製，貼到 `nonutf8-dst`（先做這段）。再用 `commits-src` 的 C1，貼到 `nonutf8-dst`，只看預覽，Escape 取消（後做這段） | 前段預覽：N1 標頭「2 個檔案，1 個不寫入」、N2 標頭「1 個檔案，1 個不寫入」；`latin1.txt` 兩列的詳情是「未複製：非 UTF-8 編碼，不寫入也不刪除」，`ok.txt` 是「建立」。複製通知含「2 個檔案未複製」，並列出「#1 latin1.txt」與「#2 latin1.txt」。Apply：`PASTE_DONE created=2 … commits=2`；`latin1.txt` 位元組仍是 `caf` `e9` 換行；`ok.txt` 已建立。後段預覽：`common.txt` 列是「跳過」，詳情「目的地現有檔案不是 UTF-8，不覆寫」；Escape 得到 `[APP:PASTE_CANCELLED]`，`common.txt` 位元組不變、HEAD 不變 |
| C-blocked | 開 `blocked-src`，複製 `B1 blocked dir and fresh`，貼到 `ws-dst/repo04` | 預覽：標頭帶「 (整個 commit 會被拒絕)」，「2 個檔案，0 個不寫入」；摘要「1 個 commit（1 個被拒絕）」；紅色橫幅「第 1 個 commit「B1 blocked dir and fresh」會被拒絕，重播將在此停止」；`fresh.txt`「建立」，`newdir/x.txt`「拒絕」，詳情「整個 commit 會被拒絕：父目錄被檔案佔住」。Apply：`[APP:PASTE_STALE_DETECTED: commit_replay_refused]`，沒有 `PASTE_DONE`，橫幅「沒有建立任何 commit；第 1 個 commit「B1 blocked dir and fresh」被拒絕：父目錄被檔案佔住」；`fresh.txt` 不存在；HEAD、status 不變。整筆拒絕是規格 4.3 的要求，也證明 #57 commit 說明的「其他檔照常貼上」不成立。判定規則同 T31 的 (a)(b)(c)。T31 與這格都不寫入，`repo04` 可連續做 |
| K-space | T26 的預覽（C4 到 `commits-dst-clean`），點 `paste-row:0:new-binary.bin`，按 Space | 沒有 `PASTE_TOGGLED`（這一列不可覆寫，不能被切成允許覆寫）。有 `[APP:PASTE_SEL_TOGGLED: idx=0 state=false]`；該列變成「已排除」，詳情是「已排除；commit 重播不能只套用部分檔案，需重新勾選才能套用」；面板出現紅色橫幅。此時 `btn-apply` 得到 `[APP:PASTE_STALE_DETECTED: commit_subset_rejected]`，`rev-list --count` 為 0。預覽仍開著；若被關掉，重貼再做 K-reinclude，並在證據欄記下 |
| K-reinclude | 接著 K-space，再按一次 Space | `[APP:PASTE_SEL_TOGGLED: idx=0 state=true]`，該列回到「跳過」，紅色橫幅消失。`btn-apply` 得到 `PASTE_DONE created=1 … commits=1`。預覽若在排除前已有另一種紅色橫幅（例如 Apply 剛回 `stale_created`），排除與重新勾選都不能蓋掉或清掉它，只有「會被拒絕」的提示與無橫幅會讓位給子集橫幅。重新勾選後橫幅消失且 Apply 成功為 `pass`；橫幅仍在而 Apply 成功判 `ui-defect`；Apply 不成功判 `fail` |
| K-fold | T19 的預覽。點 `paste-row:2:common.txt`（`PASTE_NAV idx=2`），再點 `paste-commit:1`。最後把三個標頭全收合，再按 Space | 日誌依序是 `[APP:PASTE_COMMIT_TOGGLED: idx=1]`、`[APP:PASTE_NAV: idx=7]`（`side.txt`，選取移到可見列），然後才是 `paste-row:1:…`–`paste-row:6:…` 與 `paste-include:1:…`–`paste-include:6:…` 的 `[APP:CTRL_GONE: id=…]`（`CTRL_GONE` 在下一幀繪製時才印，所以在 NAV 之後；三者都要出現，NAV 與 GONE 之間的順序以此為準）；按 Space 只出現 `PASTE_SEL_TOGGLED idx=7`，沒有作用在被藏起來的 ix 1–6。三個標頭全收合後按 Space，1 秒內沒有 `PASTE_SEL_TOGGLED` 或 `PASTE_TOGGLED`。展開後這些 ID 各有新的 `CTRL_BOUNDS`，列都回來 |
| K-nav | T19 的預覽，點 `paste-row:0:common.txt`，按 Down 7 次 | `PASTE_NAV` 的 idx 依序是 1、2、…、7，也就是重播順序（C1、C2 的 6 列、C3），不是全部路徑排序。每一次選取的列，就是畫面上的下一列 |
| K05-key | 依第 4 節的拒絕文字檢查，轉錄 T02、T13、T15、T29、T30、T31、C-blocked、K-space 的每一次拒絕文字。再按 Option+L 切到英文，重做 T15、T30、T31，轉錄英文文字，最後切回。另外複製 `commits-src` 的 C1，開 `fixtures/nongit-dst` 為工作區，按 Cmd+V：走 `build_commit` → `Git::open_with` 失敗 → `destination_error`，得到 `[APP:PASTE_ERR: paste_err_destination]`，狀態列是「無法使用貼上目的地「…/nongit-dst」: <core 的英文原因>」，英文原因形如「… is not inside a git repository (commit mode and git sources need one)」，轉錄整句。這句夾英文是已接受的限制（core 錯誤字串沒有翻譯），不屬於 T31 規則 (a)（(a) 只看 T31 與 C-blocked 的 `commit_replay_refused` 橫幅）；只要沒有原始 key 就不判 `ui-defect`。`fixtures/file-dst` 現在是選用：以它開工作區在 `open_workspace_path` 就被拒，只會出現 `workspace_bad_path`「找不到工作區資料夾：…」，到不了貼上，`paste_err_destination_not_dir` 從 UI 走不到。要做就轉錄那則訊息，也套用 key 規則 | 所有畫面文字都沒有符合 `[a-z]+(_[a-z0-9]+)+` 的原始 key（例如 `paste_err_destination`、`paste_err_not_payload`、`commit_subset_rejected`），也沒有「key (args)」的形式。zh-TW 與 en 各查一次。任何一處露出 key，判 `ui-defect` |

規格對照與 UI 回報補上的格子。除非該格另有說明，用閘門 B 的 App 與 fixture；換工作區用 `btn-workspace-menu` → `btn-open-workspace`。C1–C4、SIDE 的 7 字元 SHA 從 `commit-row:` 的 `CTRL_BOUNDS` 讀，再用 `git -C <commits-src> log --format='%h %s' --all` 對名字。

| ID | 點什麼 | 通過線 |
|---|---|---|
| I-config | 第 2 節第 4 步的兩次快照：第一次啟動前（`before`），最後一個 App 按 Cmd+Q、exit code 確認之後（`after`） | `diff` 兩個 `.sha` 檔與兩個 `.mtime` 檔都沒有輸出：檔案數、SHA-256、mtime 與大小全部相同。資料夾兩次都不存在也算相同。`$RUN/config/recent-workspaces.json` 存在，證明 `SNIP_CONFIG_DIR` 生效。任何差異判 `fail`，證據欄貼 diff。這一輪中途若有別的程式寫了它，同樣是 `fail`：無法證明不是受測程式寫的 |
| L-chip | `perf15`，視窗 1080×720，Git log 開著。`log-filter-repo` → `log-repo:<名稱>` 選一個 repo。(1) 之後不點、不按鍵，把視窗拉到 900×600。(2) 點 `log-filter-more` → `log-filter-more:user` → `log-user:<名稱>`（名稱從 `CTRL_BOUNDS` 讀）。(3) 清掉使用者篩選：使用者 chip 在列上（`log-filter-user` 有 `CTRL_BOUNDS`）就點 chip 上的 `log-filter-user-clear`；被收合時點 `log-filter-more`，再點選單裡的 `log-filter-user-clear`。(4) 拉回 1080×720。(5) 把寬度拉到 700，點 `log-filter-more` | (1) 拉完 1 秒內有一行新的 `[APP:LOG_CHIPS: avail=… widths=… more=… shown=<4 位元>]`，`shown` 與拉之前 1080×720 的那一行不同。`shown` 是 1 的 chip 在這一行之後有 `CTRL_BOUNDS`，是 0 的有 `[APP:CTRL_GONE: id=log-filter-<key>]`，`log-filter-more` 有 `CTRL_BOUNDS`。每個畫出來的 chip（`log-filter-repo`、`log-filter-branch`、`log-filter-user`、`log-filter-date`、`log-filter-more`）的矩形，都不和 `btn-log-refresh`、`btn-head`、`btn-compare`、`btn-copy-commits` 的矩形相交；用各自最新的 `CTRL_BOUNDS` 算，寫進 `action.json`。(2) 依序 `[APP:LOG_MENU: Some(FilterMore)]`、`[APP:LOG_MENU: Some(User)]`、`[APP:LOG_SEARCH: active=true author=true]`。列上每個 `commit-row:<repo>:<7 字元>` 的 repo 段都是所選的 repo，SHA 都在 `git -C <repo> log --fixed-strings --author=<名稱> --format=%H` 的輸出裡。(3) `[APP:LOG_SEARCH: active=false author=false]`，列回到只有 repo 篩選。(1)–(3) 之間沒有任何 `[APP:LOG_REFRESH]`。(4) 有新的 `LOG_CHIPS` 行，`shown` 與 (1) 之前 1080×720 那一行相同，收合過的 chip 各有新的 `CTRL_BOUNDS`。(5) `log-filter-more` 有 `CTRL_BOUNDS`，`w` 與 `h` 都至少 1，矩形完全在 `VIEWPORT` 內；點下去得到 `[APP:LOG_MENU: Some(FilterMore)]` |
| T17-toast | 照 T17 做，在 900×600 與 1080×720 各做一次，每次先放 sentinel。通知出現後 12 秒內讀 `copy-toast` 最新的 `CTRL_BOUNDS` 與最新的 `VIEWPORT`，截圖並轉錄通知卡的文字 | 兩種尺寸都有新的 `[APP:COPY_COMMITS_ERR: commits are not contiguous: …]` 與 `[APP:TOAST: ok=false]`。`copy-toast` 的 `x ≥ 0`、`y ≥ 0`、`x + w ≤ Wp`、`y + h ≤ Hp`（`Wp×Hp` 是 `VIEWPORT`）；`h / scale > 40`，也就是卡片換成多行。轉錄的文字是「產生 payload 失敗: 」接上日誌 `COPY_COMMITS_ERR: ` 後面的整段英文，一字不缺，沒有被省略號截掉，也沒有原始 key。sentinel 不變。通知約 12 秒後消失（見 B-notify） |
| T18-filter | `commits-src`，Git log。點 `btn-log-regex`，在 `log-search-input` 輸入 `多行中文\|C3 merge`。放入 sentinel。點 C1 的 `commit-row:<7 字元>`，Shift 點 C3 的，點 `btn-copy-commits`。接著清空 `log-search-input`，再點 `btn-log-regex` 關掉；重新點 C1、Shift 點 C3、點 `btn-copy-commits` | 規格 4.1 的例子。搜尋後 `[APP:LOG_REGEX: true]`、`[APP:LOG_SEARCH: active=true author=false]`；列上有 C1 與 C3，C2 沒有畫出（它的 `commit-row:` 有 `CTRL_GONE`，或搜尋後沒有 `CTRL_BOUNDS`）。Shift 點後 `[APP:RANGE: commits=2 chain=visual]`；複製得到 `[APP:COPY_COMMITS_ERR: commits are not contiguous: …]` 與 `[APP:TOAST: ok=false]`，沒有 `COPY_COMMITS_DONE`，sentinel 不變。清空後 `[APP:LOG_SEARCH: active=false author=false]` 與 `[APP:LOG_REGEX: false]`。再選一次得到 `[APP:RANGE: commits=3 chain=first_parent]`，複製得到 `[APP:COPY_COMMITS_DONE: commits=3]`，payload 第一行是 `// snip-sync commits v1`，含 C1、C2、C3 三筆 message |
| K-cmdc | 每處複製兩次，比對兩次的剪貼簿。開 `files-src`：(1) 專案樹點 `tree-row:both.txt`，按 Cmd+C；再對同一列右鍵 → `menu-item:copy-files`。(2) `rail-changes`，點 `change-row@<repo>:staged:both.txt`（`<repo>` 從 `CTRL_BOUNDS` 讀），按 Cmd+C；再對同一列右鍵 → `menu-item:copy-files`。開 `commits-src`：(3) Git log 點 C4 的 `commit-row:<7 字元>`，按 Cmd+C；再對同一列右鍵 → `menu-item:copy-commits`。(4) 開 `files-src`，點 `tree-row:folder/a.txt`，在 `reader` 裡拖曳選取一段文字，按 Cmd+C | (1)(2) 每次複製各有新的 `[APP:COPY_DONE: copied=1]`，同一處兩次的 pbpaste SHA-256 相同；(1) 的內容是 worktree 的 `worktree-body`，(2) 是 index 的 `index-body`。(3) 兩次各有 `[APP:COPY_COMMITS_DONE: commits=1]`，右鍵那次先有 `[APP:MENU_ACTION: copy-commits]`；兩次 SHA-256 相同，第一行是 `// snip-sync commits v1`。(4) 放開滑鼠時有 `[APP:SELECTION: from=… to=… chars=N]`，Cmd+C 後有 `[APP:SELECTION_COPIED: chars=N fnv=…]`，沒有新的 `COPY_DONE`；pbpaste 有 N 個字元，內容等於畫面上反白的文字（轉錄），不是 snip-sync payload |
| B-group | `ws-src`，先 `btn-repo-selector` → `pick-repo:repo01`，等 `[APP:REPO_SELECTING]`。`rail-changes`，放入 sentinel。(1) 對 `change-header:staged` 右鍵 → `menu-item:copy-files`。(2) 對 `change-repo:staged:repo01` 右鍵 → `menu-item:copy-files` | (1) `[APP:MENU_OPEN: Left items=copy-files…]`、`[APP:MENU_ACTION: copy-files]`、`[APP:COPY_DONE: copied=6]`。檔案集合等於 15 個 repo 各自 `git -C <repo> diff --cached --name-only` 的聯集，共 6 個。payload 的寫法照 core 的 `plan_export_with`：主 root（目前選取的 repo01）的檔不帶前綴，其他 repo 的檔帶 `<repo 名稱>/`。所以路徑是 `unrelated.txt`（repo01）、`repo02/only-src.txt`、`repo03/unrelated.txt`、`repo05/only-src.txt`、`repo06/shared.txt`、`repo07/shared.txt`；沒有 `// clipcode-root:` 行。每個檔的內容等於 `git -C <repo> show :<path>`（檔尾換行依檔案模式契約去掉）。(2) `[APP:COPY_DONE: copied=1]`，payload 第一行是 `// clipcode-root: repo01`，只有 `unrelated.txt` 一個檔，內容 `from-repo01` |
| B-compare | `commits-src`，Git log。點 C2 的 `commit-row:<7 字元>`，Shift 點 C4 的，點 `btn-compare` | Shift 點後 `[APP:RANGE: commits=3 chain=first_parent]`。點後 `[APP:COMPARE: from=<C2 的 7 字元> to=<C4 的 7 字元>]`，接著 `[APP:E2E_CHANGES: files=N]`。`N` 與變更檔案裡的 `commit-file:<path>` 集合（`commit-dir:` 要展開）都等於 `git -C <commits-src> diff -M --name-only <C2> <C4>` 的輸出。這是規格 3.1 的端點比較：有 C3 帶進來的 `side.txt` 與 C4 的 `newdir/content.txt`、`new-binary.bin`，沒有 C2 自己改的 `common.txt`、`emoji.txt`。剪貼簿與 Git 不變 |
| T13b | 照 T19 開預覽（C1–C3 貼到新鮮的 `commits-dst-clean`）。預覽出現後，在 shell 執行 `git -C <commits-dst-clean> commit --allow-empty -m qa-head-moved`，記下新的 HEAD，拍一張快照，再按 `btn-apply` | 規格 3.2(b)：預覽後 HEAD 變了，套用拒絕。新的一行 `[APP:PASTE_STALE_DETECTED: stale_modified]`，沒有 `PASTE_DONE`。畫面文字是「目的地檔案已在外部修改: destination HEAD commit changed from <預覽時的 HEAD> to <qa-head-moved 的 SHA>」（用詞見第 9 節）。HEAD 仍是 `qa-head-moved`，`git rev-list --count <qa-head-moved>..HEAD` 是 0，status 雜湊等於 commit 之後、Apply 之前那張快照 |
| T-dirfile | `commits-src` 的 Git log 選 C4。變更檔案裡對 `commit-file:newdir/content.txt` 右鍵（在 `commit-dir:newdir` 底下就先展開）→ `menu-item:copy-files`。開 `fixtures/ws-dst`，`btn-repo-selector` → `pick-repo:repo04`，Cmd+V | 複製得到 `[APP:COPY_DONE: copied=1]`，payload 第一行是 `// clipcode-root: commits-src`，檔案路徑是 `newdir/content.txt`。貼上得到新的一行 `[APP:PASTE_ERR: paste_err_plan]`；不出現 `btn-apply`（有 `CTRL_GONE`，或從未有 `CTRL_BOUNDS`）；狀態列是「無法建立貼上計畫: Not a directory (os error 20)」，後半是 OS 的英文原因，同 K05-key 已接受的限制，沒有原始 key。這是規格 3.2 末的整批拒絕（父層片段是一般檔案）。repo04 的 HEAD、status 雜湊與 `newdir` 位元組（`not-a-directory` 加換行）都不變。payload 第一行不是 `clipcode-root` 時貼上會改走前綴對應，這格判 `not-run`，證據欄寫實際的第一行 |
| T-budget | 照 T14 開一個預覽，不按 Escape。在 shell 建立超過上限的 payload：`python3 -c 'import sys; sys.stdout.write("// FILE: big.txt\n" + "x" * (33 * 1024 * 1024) + "\n")' > "$RUN/big-payload.txt"`，`pbcopy < "$RUN/big-payload.txt"`，`pbpaste > "$RUN/big-readback.txt"`，兩個檔的 `wc -c` 都要大於 33554432。回到 App 按 Cmd+V | 先 `[APP:PASTE_PLAN_CLEARED]`，接著 `[APP:PASTE_ERR: preview_memory_limit]`；沒有 `PASTE_LOADING` 或 `PASTE_PREVIEW`。`btn-apply` 與之前預覽的 `paste-row:` 都有 `CTRL_GONE`。狀態列是「預覽資料超過 32 MiB 上限，未載入這次變更。請縮小預覽範圍。」上限是 `transfer::CLIPBOARD_PAYLOAD_MAX`（33,554,432 byte），`trigger_paste_preview` 在解析前就拒絕。目的端零寫入 |
| R-reader | 在 shell 建一個不是 Git 的資料夾：`mkdir -p "$RUN/reader-ws"`，`seq 1 60000 > "$RUN/reader-ws/many-lines.txt"`（約 350 KB：超過 50,000 行，小於 1 MiB），`head -c 1100000 /dev/zero \| tr '\0' x > "$RUN/reader-ws/too-big.txt"`。開這個資料夾為工作區。(1) 點 `ws-tree-row:many-lines.txt`。(2) 點一下 `reader` 讓它有焦點，按 Ctrl+F，在 `find-input` 輸入 `12345`，按 F3。(3) 按 Ctrl+G，在 `goto-input` 輸入 `40000` 按 Enter；再按 Ctrl+G，輸入 `70000` 按 Enter。(4) 按 Escape 直到尋找列關上，在 `reader` 拖曳選取幾行，按 Cmd+C。(5) 點 `ws-tree-row:too-big.txt` | (1) `reader-truncated-notice` 有 `CTRL_BOUNDS`，文字是「[注意: 內容過長，已截斷顯示前 50000 行 / 1024KB]」。(2) `[APP:FIND_BAR: open]`；最後一行 `[APP:FIND: matches=1 first_line=12345]`；F3 得到 `[APP:FIND_AT: 1/1 line=12345]`。(3) `[APP:GOTO: line=40000]`；`70000` 超過載入的 50,000 行，得到 `[APP:GOTO_INVALID]`。(4) `[APP:FIND_BAR: closed]`；`[APP:SELECTION: … chars=N]` 與 `[APP:SELECTION_COPIED: chars=N fnv=…]`；pbpaste 是反白的那幾行數字（轉錄），有 N 個字元。(5) 規格 5.1 的 1 MiB 預覽上限：`editor-error` 有 `CTRL_BOUNDS`，`reader-truncated-notice` 有 `CTRL_GONE`，畫面文字是「預覽「…too-big.txt」失敗: Preview exceeds 1 MiB」（引號內是 App 顯示的路徑，照畫面轉錄） |
| B-nav | `commits-src`（單一 repo）。(1) `log-filter-branch` → `log-branch-group:refs_local` → `log-branch:refs/heads/side`，再點 SIDE 的 `commit-row:<7 字元>`。(2) 點 `btn-head`。(3) 點任一不是 HEAD 的 `commit-row:`，按 `h`。(4) 點 `btn-refresh`，再按 Cmd+R。(5) `btn-workspace-menu`，點不是目前工作區的 `workspace-recent:<ix>`（tooltip 是完整路徑） | (1) `[APP:LOG_MENU: Some(Branch)]`、`[APP:REF_FILTER: refs/heads/side]`；列上每個 `commit-row:` 的 SHA 都在 `git -C <commits-src> rev-list side` 裡；點 SIDE 得到 `[APP:COMMIT_SELECTED: <SIDE 的 7 字元>]`。(2) 分支 chip 回到沒有值（轉錄），之後有 `[APP:HEAD_LOCATED: row=<n>]` 與 `[APP:COMMIT_SELECTED: <HEAD 的 7 字元>]`，等於 `git -C <commits-src> rev-parse --short=7 HEAD`。只回到全圖、選取停在 SIDE，判 `fail`。(3) 又一行 `HEAD_LOCATED` 與同一個 `COMMIT_SELECTED`。(4) 兩次各有新的 `[APP:DISCOVERY_PROGRESS: …]` 與 `[APP:READY_REPOS: 1]`；剪貼簿與 Git 不變。(5) `[APP:WORKSPACE: state=open path=<該列 tooltip 的路徑> generation=N]`，`N` 比上一行 `WORKSPACE` 大；`$RUN/config/recent-workspaces.json` 列著這一輪開過的工作區 |

## 7. 歷史：已合入的修正

這張表只說明第 1 節的祖先檢查對應哪些格。它是程式閱讀的結果，不是操作者可以抄去充數的結果。受測 SHA 含這些 commit 時，表上的格照第 6 節的通過線判定；不是 `pass` 就是回歸。缺哪一個 commit，那一列的格寫「缺 #NN」，閘門打開。

| PR 與 commit | 修了什麼 | 對應的格 |
|---|---|---|
| #55 `f5247ab` | 刪除二進位／非 UTF-8 檔的 commit 標為未複製，貼上不再重播刪除 | T25、C-nonutf8 |
| #56 `bc81086` | paste ID 改成 `paste-<kind>:<ix>:<path>`，同路徑的兩列各有自己的 ID | T03、T18 |
| #57 `dcbc079` | 補上 `paste_err_*` 翻譯 | K05-key |
| #58 `0a45ca1` | commit 預覽依重播計畫分組、標頭與收合、略過原因、rename 註記、空 commit 顯示、複製通知帶數量、鍵盤選取 | T26、T28、B-notify、C-group、K-space、K-fold、K-nav |
| #59 `ed3d057` | 多 repo 記錄的訊息欄保留最小寬度，1080 視窗下 ref 標籤不再被擠掉 | T36、B43（1080） |
| #75 `b07aace` | staged rename 匯出為單一 `[MOVED]`；Shift 範圍沿 first-parent 鏈；預覽標出整筆被拒的 commit，Apply 用 `commit_replay_refused`；缺檔的刪除列不畫紅色區塊；重新勾選後清掉子集橫幅；變更檔案多選複製依畫面順序 | T05、T18、T18-filter、T22、T31、C-blocked、C-reason、K-reinclude、B41 |

#75 合入之前（例如 `ed3d057`），T31、C-blocked、T22、C-reason、K-reinclude 是 `ui-defect`。合入之後這些格沒有例外。

2026-09-29 報告測的是 `main` 的 `167c10c`。那一輪的 PASS 不能抄進這份計分表。

## 8. 結論怎麼寫

`scorecard.md` 開頭三行：

```text
受測 SHA:
執行檔 SHA-256:
產品閘門: 打開 | 關閉
```

接著一張表：ID、判定、一句話證據（日誌行或 Git SHA）。產品閘門按第 1 節的名單關閉：18 步、T01–T22、T24–T37、T39、T40、條件內的 T23、B41–B44、B-notify、C-group、C-detail、C-reason、C-nonutf8、C-blocked、K-space、K-reinclude、K-fold、K-nav、K05-key、I-config、L-chip、T17-toast、T18-filter、K-cmdc、B-group、B-compare、T13b、T-dirfile、T-budget、R-reader、B-nav 全部是 `pass`。T38、C preflight 不參與。第 9 節的項目尚未測，整個產品不在本規程宣告完成。

## 9. 這份規程不關閉的項目

Windows 與 Linux 的真實輸入、IME、跨機剪貼簿、與 ClipCode 的實際互貼、standard 15×10,000×20,000 負載、重播到第 N 個 commit 失敗後保留前面幾個、磁碟滿、權限、缺 user identity、預覽後 HEAD 被改的完整矩陣、symlink、控制字元路徑、非 UTF-8 檔名、submodule、sparse checkout、linked worktree、light theme 的完整操作。

規格 5.1 寫了系統匣。`crates/desktop-native` 沒有對應控制項。找不到選單列圖示時記成規格落差，不在本規程找圖示，也不寫進計分表。

規格落差另外記在 `scorecard.md` 末尾，不算判定：

- 規格 4.1 寫 log 每頁 300 筆；程式是單一 repo `history_page_size` 50、多 repo feed `FEED_PAGE` 50。T34 依程式寫 50。
- 規格 4.3 的 commit 模式 diff：「每個 commit 可展開看檔案清單與 diff」，但 commit 重播列目前只顯示來源內容，沒有 diff。記在這裡；C-detail 不因此判 `fail`（規格 3.2 的檔案模式覆寫 diff 已支援並由 T05 檢核）。
- 規格 3.1 的檔案模式複製通知：規格寫檔案數、字元數、行數、字數、token 數與跳過數；程式的 `status_copied` 是「已從 {} 複製 {} 個檔案（{} 字元、{} 行，略過 {} 個）至剪貼簿」，沒有字數與 token 數。T07、K-cmdc 不因此判 `fail`。commit 模式的通知（規格 4.2：commit 數、檔案數、字元數、未複製數）與程式相符，由 B-notify 檢核。
- T13b 的拒絕文字：HEAD 移動被 `paste.rs` 的 `stale_msg` 歸到 `stale_modified`，畫面是「目的地檔案已在外部修改: destination HEAD commit changed from … to …」。前半句說檔案被改，與實際原因不符。記在這裡；T13b 不因此判 `ui-defect`。
- 預覽的版面衝突列（父目錄被檔案佔住等）現在標為「拒絕」並說整個 commit 會被拒；路徑規則不安全（如 `../x`）仍是一般「跳過」。CLI 的 `--dry-run` 對被拒的 commit 不計入「would be created」。

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
| C-group | | |
| C-detail | | |
| C-reason | | |
| C-nonutf8 | | |
| C-blocked | | |
| K-space | | |
| K-reinclude | | |
| K-fold | | |
| K-nav | | |
| K05-key | | |
| I-config | | |
| L-chip | | |
| T17-toast | | |
| T18-filter | | |
| K-cmdc | | |
| B-group | | |
| B-compare | | |
| T13b | | |
| T-dirfile | | |
| T-budget | | |
| R-reader | | |
| B-nav | | |
| C preflight | | |
```
