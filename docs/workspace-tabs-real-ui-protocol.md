# 工作區分頁真實 UI 驗收規程

狀態：操作規程。給會操作滑鼠與鍵盤的 agent（Codex）照著點真實 macOS 視窗，也給人照著做。受測功能是 issue #136 的工作區分頁（`crates/desktop-native/src/tabs.rs` 的 `TabsRoot`）。

點擊方法、座標換算、判定用詞、剪貼簿與 Git 的比對方式沿用 [real-ui-operator-protocol.md](real-ui-operator-protocol.md)（下稱「本機規程」）。遠端分頁、貼上中拒絕關閉的格子另用 [remote-real-ui-protocol.md](remote-real-ui-protocol.md)（下稱「遠端規程」）的 Ubuntu 準備。本文只寫分頁多出來的部分。產品行為以 [spec.md](spec.md) 為準；規格與本文不符時，先判斷是規程寫錯還是產品錯，證據欄寫清楚。

這份規程只回答一件事：開、切、關、收分頁之後，畫面、日誌、剪貼簿與磁碟是不是都對得上這一格寫的預期。

標成 `TODO(verify)` 的地方，是寫這份規程時讀程式碼也推不出、或還沒在真實視窗上實點過的事。遇到時照該格寫的退路做，並把實際看到的寫進證據欄。不要把 `TODO(verify)` 當成通過線的一部分。

## 0. 範圍與行為摘要

一個主視窗，分頁列（高 34 邏輯 px）在原生標題列下面，每個分頁一個 `WorkbenchModel`。下面這些是本文各格的預期，來源是 `tabs.rs` 與 `main.rs`：

- 開工作區（選單輸入路徑、最近開啟、遠端）：先找已經開著同一個工作區的分頁，有就切過去；沒有時，若目前分頁是空分頁（沒有工作區、沒有正在開的工作區）就在原地填入，否則新增一個分頁。工作區的身分：本機是 `canonicalize` 之後的路徑；遠端是 ssh Host 別名加 worker 解析後的真實路徑。同一台機器的兩個 Host 別名是兩個工作區。
- 開在新分頁：`WS_TAB_OPENED` 然後 `WS_TAB_ACTIVE`，再來才是新分頁自己的 `WORKSPACE: state=open`。填入空分頁：只有 `WORKSPACE: state=open`，沒有 `WS_TAB_OPENED`、沒有 `WS_TAB_ACTIVE`（空分頁在「+」那一刻已經印過）。切到已開的分頁：只有 `WS_TAB_ACTIVE`，沒有 `WORKSPACE`。
- 點目前已顯示的分頁、`Cmd+N` 跳到目前分頁：什麼日誌都不印（`activate` 在 `active == ix` 時直接返回）。
- 關分頁（Cmd/Ctrl+W、Cmd/Ctrl+Shift+W、工作區選單的 `btn-close-workspace`、分頁上的 ×）：該分頁自己的關閉排空先跑，印 `[APP:WORKSPACE: state=closed generation=N]`，再印 `[APP:WS_TAB_CLOSED: id=N count=M`。**空分頁**沒有東西要排空，只印 `WS_TAB_CLOSED`，沒有 `WORKSPACE: state=closed`。關掉目前分頁後，位置上滑進來的右鄰變成目前分頁（最右的分頁被關時是左鄰），印 `WS_TAB_ACTIVE`。關背景分頁不切換，也不印 `WS_TAB_ACTIVE`；目前分頁的索引若因此改變，只是分頁列上的位置變了。
- Cmd/Ctrl+W 與 Cmd/Ctrl+Shift+W 永遠只關**目前**分頁，不會碰背景分頁。
- 最後一個分頁關掉後，視窗只剩分頁列與「+」，沒有工作區選單、沒有歡迎畫面，App 不結束。「+」加一個空分頁，它的工作區選單已經打開。
- 背景（沒顯示）分頁印的 `[APP:…]` 行，在結尾的 `]` 前面多一個 ` ws_tab=<id>`（無冒號形式的行，例如 `[APP:PASTE_APPLYING]`，變成 `[APP:PASTE_APPLYING: ws_tab=<id>]`）。目前顯示的分頁印的行不變。分頁列（root）印的行**永遠不帶** ` ws_tab=`：`WS_TAB_OPENED`、`WS_TAB_ACTIVE`、`WS_TAB_CLOSED`、`QUIT: deferred`，以及 root 印的 `VIEWPORT`，都直接以 `]` 結尾。`<id>` 是分頁的穩定 id，從 1 起算、單調遞增、不重用；`ix` 是分頁列上的位置，從 0 起算，關掉前面的分頁後會變。
- `generation` 是每個分頁自己的計數，不是全域的。不要拿一個分頁的 generation 去跟另一個分頁比大小。
- Cmd/Ctrl+Q 與視窗關閉鈕是同一件事：所有分頁都排空之後才結束。任何一個分頁正在寫入已確認的貼上，就拒絕（`PASTE_BUSY: refused=quit`），並切到那個分頁，沒有任何分頁開始排空。每按一次 Cmd+Q 或關閉鈕，`QUIT: deferred` 恰好印一行；拒絕時 `PASTE_BUSY: refused=quit` 也恰好一行（分頁自己的 Quit 處理只轉給 root，不印日誌）。
- 視窗標題：`<目前分頁的標籤> — snip-sync`（em dash U+2014，前後各一個空格），沒有分頁或目前是空分頁時恰好是 `snip-sync`。
- 標籤：本機是資料夾名稱，遠端是 `host ▸ name`；標籤相同的分頁加上父資料夾直到不同（`one/app`、`two/app`）；空分頁是「新分頁」（英文介面 `New tab`）。標題與分頁列用同一份標籤，標題用完整標籤，分頁列上超過 220 邏輯 px 的標籤以省略號截斷。

不在範圍：

- 「複製、貼上、Git log 等工作台功能本身」的正確性：本機規程與遠端規程負責。這裡只驗「跨分頁」與「切走再回來」。
- Cmd/Ctrl+Shift+O 在**有分頁**時會開原生資料夾對話框（`open_folder_dialog`），在**沒有分頁**時 root 先 `new_tab` 再開同一個對話框。macOS 的 `NSOpenPanel` 這份規程不驅動。需要選單輸入框時一律用「+」或 `btn-workspace-menu` → `btn-open-workspace`。誤開了對話框就按 Escape（`Ok(Ok(None))` 什麼都不做；其他結果會退回輸入框）。
- Windows 與 Linux：這份規程只在 macOS 跑。快捷鍵的 Ctrl 版本由 `#[gpui::test]` 涵蓋。

## 1. 開跑

### 1.1 要交什麼

和本機規程第 1 節相同：一輪結束時交出 repo 外的目錄 `$RUN`，裡面有 `environment.json`、`scorecard.md`、每個測案一個子目錄（見 1.5）。判定只有 `pass`、`ui-defect`、`fail`、`not-run`，意義同本機規程第 1 節（點不到寫 `missing-control`，同一 ID 兩列寫 `identity-fail`）。不要 commit 這個目錄，不要在這一輪改產品程式。

受測 SHA 必須包含 #136 的分頁實作（有 `crates/desktop-native/src/tabs.rs`，並且 `grep -c 'WS_TAB_OPENED' crates/desktop-native/src/tabs.rs` 大於 0）。不符就不開跑。

### 1.2 用腳本做準備與收尾

下面的命令區塊用 bash 執行（這台 Mac 的預設 shell 是 fish；先執行 `bash`）。cargo 在 `$HOME/.cargo/bin`。

```bash
export REPO=/path/to/受測的乾淨 checkout        # 腳本在 $REPO/scripts
export SHA=$(git -C "$REPO" rev-parse --short HEAD)
export RUN="$HOME/snip-sync-ui-runs/$(date +%Y-%m-%d)-$SHA-tabs"
python3 "$REPO/scripts/real_ui_round.py" prepare --sha "$SHA" --run "$RUN"      # 遠端格再加 --remote ubuntu
export RUN=$(cd "$RUN" && pwd -P)       # 真實路徑；本機工作區的身分是 canonicalize 後的路徑，$RUN 若經過 symlink 會對不上日誌
python3 "$REPO/scripts/real_ui_round.py" launch --gate b --run "$RUN"        # 啟動，日誌寫進 $RUN/app-gate-b.log
python3 "$REPO/scripts/real_ui_round.py" resize 1080 720 --run "$RUN"
python3 "$REPO/scripts/real_ui_round.py" point <控制項 ID> --run "$RUN"      # 見 2.1：螢幕點不需修正
python3 "$REPO/scripts/real_ui_round.py" finish --run "$RUN"                  # 全部格子做完才跑
```

重點：

- `launch --gate b` 一定從 `$RUN/gate-b/fixtures/ws-src` 開始（`--workspace`）。所以**每一次啟動後第一個分頁固定是 `ws-src`，`id=1 ix=0`**，日誌開頭有 `[APP:WS_TAB_OPENED: id=1 count=1` 與 `[APP:WS_TAB_ACTIVE: id=1 ix=0`。腳本沒有「無工作區」的啟動方式；要空狀態就用 Cmd+W 關掉它。
- 同一個 `$RUN` 內要重新啟動（換主題、重置分頁）時，**不要跑 `finish`**（它會移除 worktree 並還原剪貼簿，整輪結束）。做法：對 App 按 Cmd+Q，等 exit code 0（`$RUN/app-gate-b-exit.json`），然後
  ```bash
  N=$(ls "$RUN"/app-gate-b-*.log 2>/dev/null | wc -l); mv "$RUN/app-gate-b.log" "$RUN/app-gate-b-$N.log"
  python3 "$REPO/scripts/real_ui_round.py" launch --gate b --run "$RUN"
  ```
  監督程式以附加模式開日誌檔，所以先把舊日誌改名，新日誌才只含這一次啟動。`point` 讀的是 `app-process.json` 裡的 `log`，就是新的 `app-gate-b.log`。
- 設定資料夾 `$RUN/config` 在重新啟動之間保留（最近開啟的工作區會留下）。這是預期的，用在最近開啟的格子。
- `SNIP_THEME` 寫死在 `$RUN/environment.json` 的 `launch_env.SNIP_THEME`（預設 `dark`）。跑 light 的格子：Cmd+Q、改 `launch_env.SNIP_THEME` 成 `light`、照上面重新 `launch`。見 WT81。

### 1.3 殘留、設定快照

照本機規程第 2 節第 3、4 步：每次啟動前 `pgrep -fl snip-desktop-native` 必須沒有輸出，否則停；第一次啟動前拍真實設定資料夾快照，最後 `finish` 後比對（I-config，見第 5 節）。`prepare` 已經做了其中大部分，腳本與本節不符時以本節為準並寫出差異。

### 1.4 分頁用的 fixture

`prepare` 已經在 `$RUN/gate-b/fixtures` 建了本機規程的 fixture（`ws-src` 15 個 repo、`ws-dst`、`files-src`、`commits-src`、`commits-dst-clean` 等）。分頁另外要一批小資料夾，全部放在 `$RUN/tabs`。用下面的命令建；每個資料夾都是只有一個提交的 Git repo，方便每個分頁開完都是 `READY_REPOS: 1`。

```bash
export B="$RUN/gate-b/fixtures" T="$RUN/tabs" LOG="$RUN/app-gate-b.log"
mkdir -p "$T"
G="git -c user.name=t -c user.email=t@t"
mkrepo() {   # $1=資料夾；寫一個 readme 並提交
  mkdir -p "$1" && git -C "$1" init -q -b main && printf '%s\n' "$(basename "$1")" > "$1/readme.txt" \
    && git -C "$1" add -A && $G -C "$1" commit -qm init
}
for d in alpha beta gamma one/app two/app a/x/app b/x/app outer; do mkrepo "$T/$d"; done
mkdir -p "$T/alpha/sub" && echo sub > "$T/alpha/sub/s.txt"         # 巢狀的一般資料夾（不是 repo）
ln -s "$T/alpha" "$T/alpha-link"                                    # symlink 拼法
LONGN=$(python3 -c "print('very-long-workspace-folder-name-' * 4 + 'end')")   # 約 130 字元，遠超 220 邏輯 px
export LONGN; mkrepo "$T/$LONGN"
for i in 01 02 03 04 05 06 07 08 09 10 11 12; do mkrepo "$T/many/workspace-number-$i"; done
# 跨分頁複製貼上用：src-a 有兩個提交（base 與 tabs commit one），dst-b 是另一個乾淨 repo
mkdir -p "$T/src-a/sub" && git -C "$T/src-a" init -q -b main
printf 'hello-a' > "$T/src-a/hello.txt"; printf 'deep-a' > "$T/src-a/sub/deep.txt"
git -C "$T/src-a" add -A && $G -C "$T/src-a" commit -qm base
printf 'c1' > "$T/src-a/c1.txt" && git -C "$T/src-a" add -A && $G -C "$T/src-a" commit -qm "tabs commit one"
mkdir -p "$T/dst-b" && git -C "$T/dst-b" init -q -b main && printf 'base-b' > "$T/dst-b/base.txt"
git -C "$T/dst-b" add -A && $G -C "$T/dst-b" commit -qm base
# 遠端貼上中的格子（WT6x）用的檔案模式來源：兩個新檔。刻意不用 $RUN/paste-src，那是遠端規程 2.2 的 fixture，不能動
mkdir -p "$T/busy-src" && printf 'new\n' > "$T/busy-src/new.txt" && printf 'new2\n' > "$T/busy-src/new2.txt"
```

CLI 只用來產生 payload 與 oracle（不替代畫面上的複製與貼上）。`prepare` 不一定建 CLI，所以自己建：

```bash
( cd "$REPO" && export PATH=$HOME/.cargo/bin:$PATH && cargo build -p snip-cli --locked ) && export SNIP="$REPO/target/debug/snip"
```

負向格用的不存在路徑是 `$T/no-such-dir`（不要建立它）。

遠端格（WT4x、WT6x）另外要 Ubuntu 端的準備：遠端規程第 2.1 到 2.3 節（`prepare --remote ubuntu`、第 0 步閘門 `just remote-e2e-ssh ubuntu`、`$W`、`~/.local/bin/snip` wrapper、`stop_run_workers`、收尾），**同一個 `$RUN`**。沒有做完就把遠端格寫 `not-run`，證據欄寫「缺 Ubuntu 準備」。

### 1.5 每一格怎麼收證據

每格開始時執行 `case_start <ID>`，每一次動作前後用 `shot`、`expect`、`forbid`。下面的函式在 bash 裡定義一次（新 shell 要再定義）：

```bash
export PID=$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["pid"])' "$RUN/app-process.json")   # 每次重新 launch 後再執行
case_start() { export CASE="$RUN/$1"; mkdir -p "$CASE"; export L0=$(wc -l < "$LOG"); echo "case $1 log start line $L0"; }
newlog()  { tail -n +"$((L0+1))" "$LOG"; }                         # 這一格開始之後的日誌
mark()    { export L0=$(wc -l < "$LOG"); }                         # 一個動作之後重設起點（要分段判定時用）
expect()  { newlog | grep -F -e "$1" > /dev/null && echo "ok      : $1" || echo "MISSING : $1"; }
forbid()  { newlog | grep -F -e "$1" > /dev/null && echo "UNEXPECTED: $1" || echo "ok-absent: $1"; }
shot()    { sleep 0.5; screencapture -x "$CASE/$1.png" && echo "$CASE/$1.png"; }   # 整個主螢幕；視窗要在主螢幕
wtitle()  { osascript -e "tell application \"System Events\" to get name of first window of (first application process whose unix id is $PID)"; }
clip()    { pbpaste | shasum -a 256 | cut -d' ' -f1; }
```

- `case_start` 之後用到的所有截圖、`action.json`、剪貼簿檔案都放在 `$CASE`（`$RUN/<ID>/`）。同一格重做時先把舊目錄改名保留。
- `expect` 與 `forbid` 的字串是**子字串**，`WS_TAB_OPENED`、`WS_TAB_ACTIVE`、`WS_TAB_CLOSED`、`QUIT: deferred` 這幾種行只寫到最後一個欄位為止、不寫結尾的 `]`（可以用前綴比對；這幾種行是 root 印的，結尾一定是 `]`、不帶 ` ws_tab=`，要證明就用 `NOT:` ` ws_tab=`）。為了不和 `id=22` 之類混淆，需要精確時用 `newlog | grep -E 'WS_TAB_CLOSED: id=2 count=2( |])'`。表格與清單裡凡是寫成 `L:` 的，都是「這一格開始後，依序出現」；寫成 `NOT:` 的是「不得出現」。順序用 `newlog | grep -nF` 比行號。
- 每格的截圖名稱固定寫在該格的「截圖」欄，放在 `$RUN/<ID>/`。「before」是動作前、「after」是動作後，其他名稱照該格寫。截圖不參與通過與否，除了本機規程第 4 節列的文字 oracle 格；這份規程裡「標籤」「圖示」「tooltip」這類只在畫面上的東西，用截圖加畫面文字轉錄（寫進 `action.json` 的 `screen_text`）當 oracle。標題與標籤另有 `wtitle` 的文字 oracle。
- 一個分頁的標籤沒有日誌。要證明標籤，兩個證據都留：截圖，以及切到那個分頁後的 `wtitle`（`<標籤> — snip-sync`）。

## 2. 怎麼點（與本機規程的差異）

點擊照本機規程第 3 節的七步：取該 ID 最新的 `CTRL_BOUNDS`、確認沒有更晚的 `CTRL_GONE`、換算螢幕點、點、點完要有新的日誌行。差異只有下面幾條。

### 2.1 分頁列與座標

`[APP:VIEWPORT: WxH]` 由 root（`TabsRoot`）印，是**整個視窗內容區**，包含 34 邏輯 px 的分頁列。`CTRL_BOUNDS` 的座標也是從視窗內容區左上角起算，兩者同一個原點，所以 `scripts/real_ui_round.py point` 算出來的螢幕點不需要任何修正，`point --hover` 也可以直接用。`resize W H` 讓內容區恰好是 W×H（VIEWPORT 就是 W×scale 乘 H×scale）。第 4 節 WT90、WT91 的「視窗尺寸」都指 `resize` 的參數。

`pt` 只是 `point` 的薄包裝，`hv` 把游標移到螢幕點停留、不點擊，用來顯示 tooltip 與 hover 色：

```bash
pt() {   # pt <控制項 ID>：印 point 的結果（Screen Point 就是要點的螢幕點）
  python3 "$REPO/scripts/real_ui_round.py" point "$1" --run "$RUN"
}
hv() {   # hv <x> <y> [秒]：把游標移到螢幕點（邏輯點）停留，不點擊，用來顯示 tooltip 與 hover 色
  python3 -I - "$1" "$2" "${3:-2}" <<'PY'
import subprocess, sys, time
x, y, s = sys.argv[1], sys.argv[2], float(sys.argv[3])
script = ('ObjC.import("CoreGraphics");'
          f'$.CGEventPost($.kCGHIDEventTap, $.CGEventCreateMouseEvent(null, $.kCGEventMouseMoved, {{x:{x},y:{y}}}, 0));')
subprocess.run(["osascript", "-l", "JavaScript", "-e", script], check=True, capture_output=True, timeout=10)
time.sleep(s)
PY
}
```

`action.json` 留下 `point` 的原始輸出與實際點到的螢幕點。

### 2.2 分頁列的探針

分頁列的控制項只在 `SNIP_NATIVE_E2E=1` 時印 `CTRL_BOUNDS`（`launch_env` 已設）。分頁列的探針 ID 見第 3 節。注意：

- 分頁的狀態探針 ID 把狀態寫在名字裡：`ws-tab-state:<ix>:<local|connecting|connected|failed>`。狀態改變時，舊 ID 出現 `CTRL_GONE`，新 ID 出現 `CTRL_BOUNDS`。要證明「現在是什麼狀態」，看最新的那一行，不要看舊的。
- 空分頁與本機分頁的狀態都是 `local`。
- 探針會裁到可見區域：分頁被橫向捲出視野、完全看不見時，`ws-tab:<ix>` 出現 `CTRL_GONE`；只露出一部分時，`CTRL_BOUNDS` 是看得見的那部分。
- 「+」（`ws-tab-new`）在捲動容器外面，永遠在分頁列最右邊，bounds 不隨捲動改變。
- 點分頁用滑鼠按下時就切換（`on_mouse_down`），不是放開時；× 的按下不會穿透成切換（`stop_propagation`），所以點背景分頁的 × 不會先切過去。

### 2.3 輸入文字

`workspace-path-input`：先 `pt workspace-path-input` 點一下取得焦點，再用鍵盤輸入路徑（不要在輸入框以外按 Cmd+V 當作貼文字）。路徑含 `$RUN`、`$T` 的地方，操作者先在 shell 展開成絕對路徑再輸入，截圖確認輸入框裡的字。按 `btn-workspace-open-confirm` 送出。輸入含中文以外的 ASCII 路徑即可，這份規程的資料夾名稱都是 ASCII。工具打不出路徑時的退路：`printf %s "<路徑>" | pbcopy`，點輸入框後按 Cmd+V（此時焦點在輸入框，是貼文字），但這會蓋掉剪貼簿，所以只在不判剪貼簿的格子用。

### 2.4 「開某路徑」的標準動作 OPEN(path)

下面凡寫 `OPEN(<路徑>)`，就是：

1. 若目前分頁的工作區選單沒打開：`pt btn-workspace-menu`，點。（空分頁的選單已經打開，跳過這步。）
2. `pt btn-open-workspace`，點。
3. `pt workspace-path-input`，點，輸入路徑。
4. `pt btn-workspace-open-confirm`，點。

`OPEN` 從**有工作區的分頁**發出、目標還沒開著時，日誌依序是（`M` 是開之前的分頁數）：

```text
[APP:WS_TAB_OPENED: id=<新 id> count=<M+1>
[APP:WS_TAB_ACTIVE: id=<新 id> ix=<M>
[APP:WORKSPACE: state=open path=<canonical 路徑> generation=<G>]     # 沒有 ws_tab：此時新分頁已是目前分頁
[APP:READY_REPOS: <N>]
```

### 2.5 標準起點

| 名稱 | 內容 |
|---|---|
| S1 | 剛 `launch`：`[ws-src]`，`id=1`，目前 `ix=0`，1080×720 |
| S3 | S1 之後依序 `OPEN($T/alpha)`（`id=2`）、`OPEN($T/beta)`（`id=3`）：`[ws-src, alpha, beta]`，目前 `ix=2` |
| S4 | S3 之後 `OPEN($T/gamma)`（`id=4`）：`[ws-src, alpha, beta, gamma]`，目前 `ix=3` |

每一格的「前置狀態」寫明用哪個起點、目前是哪個分頁。不確定現在是不是那個狀態時，Cmd+Q、改名日誌、重新 `launch`、重建（`id` 從 1 重新起算）。開始動作前截一張 `before.png` 確認分頁列與標題。

## 3. 用到的控制項與日誌

### 3.1 新的探針 ID（`tabs.rs`）

| ID | 控制項 | 點擊之後的日誌 |
|---|---|---|
| `ws-tab:<ix>` | 第 `ix` 個分頁（整個分頁晶片）。按下就切換。tooltip 是完整路徑（本機）或 `host:path`（遠端），空分頁是「新分頁」；連線中、失敗、貼上中時尾巴加 ` · 連線中`／` · 連線失敗`／` · 貼上中` | 切到另一個分頁：`[APP:WS_TAB_ACTIVE: id=<id> ix=<ix>`；點目前分頁：沒有 |
| `ws-tab-close:<ix>` | 分頁上的 ×。tooltip 是「關閉分頁 ⌘W」（英文 `Close tab ⌘W`） | 有工作區：`WORKSPACE: state=closed`，然後 `WS_TAB_CLOSED`；空分頁：只有 `WS_TAB_CLOSED`；忙碌（貼上寫入中）：`PASTE_BUSY: refused=close-workspace` |
| `ws-tab-new` | 分頁列最右邊的「+」，tooltip「開新的工作區分頁」（英文 `New workspace tab`） | `WS_TAB_OPENED` 然後 `WS_TAB_ACTIVE` |
| `ws-tab-state:<ix>:<state>` | 分頁左邊的狀態圖示。`local` 資料夾圖示（灰）、`connecting` 重新整理圖示（灰）、`connected` 遠端分支圖示（強調色）、`failed` 警告圖示（錯誤色） | 不可點，只讀它的 `CTRL_BOUNDS` 與 `CTRL_GONE` |
| `ws-tab-pasting:<ix>` | 標籤右邊的貼上圖示（強調色）。只有那個分頁正在寫入已確認的貼上時才畫 | 不可點；寫入結束後出現 `CTRL_GONE` |

### 3.2 分頁會印的日誌

| 日誌 | 意思 |
|---|---|
| `[APP:WS_TAB_OPENED: id=N count=M` | root 印，不帶 ` ws_tab=`。新增了分頁（`N` 是新 id，`M` 是新增後的分頁數）。啟動時第一個分頁也印一行 |
| `[APP:WS_TAB_ACTIVE: id=N ix=I` | root 印，不帶 ` ws_tab=`。分頁 `N` 成為目前分頁，位置 `I`。已經是目前分頁時不印 |
| `[APP:WS_TAB_CLOSED: id=N count=M` | root 印，不帶 ` ws_tab=`。分頁 `N` 被移除，剩 `M` 個 |
| `... ws_tab=<id>]` | 背景分頁印的行的標記，在結尾 `]` 前；root 印的行永不帶 |
| `[APP:WORKSPACE: state=open path=<p> generation=G]`／`state=closed` | 該分頁的工作區開／關 |
| `[APP:LIFECYCLE: phase=<…> intent=<quit\|close-workspace\|open-workspace\|open-remote-workspace> …]` | 該分頁的排空階段；結束時每個分頁各印一行 `phase=drained intent=quit jobs=0 …` |
| `[APP:QUIT: deferred` | Cmd+Q 或視窗關閉鈕，由 root 印，每按一次恰好一行，不帶 ` ws_tab=` |
| `[APP:PASTE_BUSY: refused=<close-workspace\|quit>]` | 貼上寫入中，拒絕關閉或結束。同時有 `LIFECYCLE: phase=refused intent=… reason=applying`，狀態列是「正在套用變更，已拒絕關閉、開啟與結束，以免寫入中斷。」 |
| `[APP:FOCUS: next]`／`[APP:FOCUS: prev]` | Ctrl+Tab／Ctrl+Shift+Tab：焦點移動，與分頁無關 |

### 3.3 本機規程已有、這份規程會用的控制項

| 位置 | ID |
|---|---|
| 工作區選單 | `btn-workspace-menu`、`btn-open-workspace`、`workspace-path-input`、`btn-workspace-open-confirm`、`btn-close-workspace`、`btn-open-folder`（不點，會開原生對話框）、`workspace-recent:<ix>`、`remote-recent:<n>` |
| 遠端選單 | `remote-host:<ix>`、`remote-folder:<n>`、`remote-up`、`btn-remote-open-here`、`remote-path-input`、`btn-remote-open` |
| 左軌與工具 | `rail-project`、`rail-changes`、`rail-log`、`btn-repo-selector`、`pick-repo:<名稱>`、`btn-refresh`、`btn-paste`、`splitter-left`（有探針，`SPLIT_RESIZED` 見下） |
| 專案樹 | 單一 repo 工作區：`repo-row:<名稱>`、`tree-row:<path>`；多 repo 或一般資料夾：`ws-tree-row:<path>` |
| 變更 | `change-row@<repo>:<source>:<path>`（ID 以 binary 印出的為準） |
| 貼上 | `btn-apply`、`btn-cancel`、`paste-row:<ix>:<path>`、`paste-commit:<c>` |
| Log | `commit-row:<7 字元 SHA>`、`btn-copy-commits` |

其他日誌（`COPY_DONE`、`COPY_COMMITS_DONE`、`PASTE_PREVIEW`、`PASTE_APPLYING`、`PASTE_DONE`、`PASTE_CANCELLED`、`READY_REPOS`、`DISCOVERY_PROGRESS`、`REMOTE_OPENED`、`SPLIT_RESIZED`、`VIEWPORT`、`CTRL_BOUNDS`、`CTRL_GONE`、`CTRL_COVERED`、`CTRL_DUPLICATE`）意義同本機規程與遠端規程。

## 4. 測案

每格五項：**前置狀態**、**操作**、**預期畫面**、**比對**、**截圖**。「比對」裡 `L:` 是這一格開始後依序要出現的日誌行，`NOT:` 是不得出現的行（用 `forbid`，動作後等 1 秒再判）。所有 `$T`、`$B`、`$RUN` 先在 shell 展開成絕對路徑再比對。每格開始前 `case_start <ID>`；動作分段時用 `mark` 重設起點。

### 4.0 校準

#### WT00 分頁列、座標與標題

- **前置狀態**：S1（`[ws-src]`，`id=1`，目前 `ix=0`），1080×720（`resize 1080 720`），`SNIP_THEME=dark`。
- **操作**：(1) `case_start WT00`，`shot before`。(2) `wtitle`。(3) `pt ws-tab:0`，記下螢幕點；`hv` 到該點停 2 秒，`shot hover-tab`。(4) 點一下 `ws-tab:0`（目前分頁）。(5) `pt ws-tab-new`，`hv` 到該點停 2 秒，`shot hover-plus`。把 `scale` 寫進 `environment.json` 的 `notes`。
- **預期畫面**：分頁列在原生標題列正下方、高 34 邏輯 px，一個分頁 `ws-src`：資料夾圖示、標籤 `ws-src`、右邊 ×；目前分頁底色明顯（dark `#233558`）、文字比非目前分頁亮；最右邊一個「+」。分頁列下面是工作台自己的 header（有 `btn-workspace-menu`）。`hover-tab` 的 tooltip 是完整路徑 `$B/ws-src`；`hover-plus` 的 tooltip 是「開新的工作區分頁」。標題列文字 `ws-src — snip-sync`。
- **比對**：`grep -F` 整份日誌有 `[APP:WS_TAB_OPENED: id=1 count=1`、`[APP:WS_TAB_ACTIVE: id=1 ix=0`、`[APP:WORKSPACE: state=open path=$B/ws-src generation=`。最新 `CTRL_BOUNDS` 有 `ws-tab:0`、`ws-tab-close:0`、`ws-tab-new`、`ws-tab-state:0:local`。`ws-tab:0` 的 `y ≥ 0` 且 `y + h ≤ 34 × scale`；`btn-workspace-menu` 的 `y ≥ 34 × scale`。`[APP:VIEWPORT:]` 是 `1080×scale` 乘 `720×scale`。`wtitle` 等於 `ws-src — snip-sync`（比對含 em dash 的完整字串）。步驟 (4) 之後 `newlog` 沒有任何新行（`NOT:` 任何 `WS_TAB_`）。
- **截圖**：`$RUN/WT00/before.png`、`hover-tab.png`、`hover-plus.png`。

### 4.1 開啟與切換（WT01–WT04）

#### WT01 開本機工作區進新分頁

- **前置狀態**：S1，目前 `ws-src`。`$T/alpha` 沒有開著。
- **操作**：`OPEN($T/alpha)`（`btn-workspace-menu` → `btn-open-workspace` → 輸入 `$T/alpha` → `btn-workspace-open-confirm`）。完成後 `shot after-open`，再 `hv` 到 `ws-tab:1` 停 2 秒 `shot hover-tab`。
- **預期畫面**：分頁列兩個分頁 `ws-src`、`alpha`，`alpha` 是目前分頁（底色亮、文字亮），`ws-src` 變成非目前（文字較灰）；兩個都是資料夾圖示。`alpha` 的工作台已載入（`rail-project` 顯示專案樹）。標題 `alpha — snip-sync`。`hover-tab` 的 tooltip 是 `$T/alpha`。`ws-src` 分頁沒有變化。
- **比對**：`L:` `[APP:WS_TAB_OPENED: id=2 count=2`、`[APP:WS_TAB_ACTIVE: id=2 ix=1`、`[APP:WORKSPACE: state=open path=$T/alpha generation=`、`[APP:READY_REPOS: 1]`，順序用行號確認。`WORKSPACE: state=open` 那一行不帶 ` ws_tab=`（新分頁已經是目前分頁）；`ws-src` 沒有印任何 `COPY_`／`PASTE_`／`WORKSPACE` 行。`NOT:` `WS_TAB_OPENED`、`WS_TAB_ACTIVE` 這兩行含 ` ws_tab=`（root 印的行不帶）。`ws-tab:1`、`ws-tab-close:1`、`ws-tab-state:1:local` 有 `CTRL_BOUNDS`。`wtitle` = `alpha — snip-sync`。剪貼簿 `clip` 前後相同。`git -C "$T/alpha" status --porcelain` 前後相同（開啟不寫入）。
- **截圖**：`before.png`（S1）、`after-open.png`、`hover-tab.png`。

#### WT02 重開已開著的工作區：切換，不新增

- **前置狀態**：S3（`[ws-src id1, alpha id2, beta id3]`），目前 `beta`（`ix=2`）。
- **操作**：依序做 a 到 f，每一步之間 `mark`，並各存一組截圖。
  - a：`OPEN($T/alpha)`，輸入字串就是 `$T/alpha`。
  - b：目前回到 `beta`（`Cmd+3`），`OPEN("$T/alpha/")`（尾端多一個斜線）。
  - c：目前回到 `beta`，`OPEN($T/alpha-link)`（symlink 拼法）。
  - d（最近開啟，同一工作階段）：目前在 `beta`（`ix=2`），`btn-workspace-menu`。開選單時會重讀 `$RUN/config/recent-workspaces.json` 與 `remote-recent.json`，所以在別的分頁開過的 `alpha` 也會列在 `beta` 的選單裡。`hv` 到每一列看 tooltip 的完整路徑，找出 `$T/alpha` 那一列（`workspace-recent:<ix>`，`ix` 以 `CTRL_BOUNDS` 為準），點它：切到 `alpha`，沒有新分頁。
  - e：目前在 `alpha`（`ix=1`），`OPEN($T/alpha)`（開自己）。
  - f：點目前分頁自己 `ws-tab:1`。
  - d2（最近開啟，重新啟動版，做在 f 之後）：`Cmd+Q` 結束，等 exit code 0，日誌改名，重新 `launch`（1.2）。新的 `ws-src` 分頁建立時讀取 `$RUN/config/recent-workspaces.json`，所以選單裡有 `$T/alpha`、`$T/beta` 的 `workspace-recent:<ix>`。`btn-workspace-menu`，`hv` 到每一列看 tooltip，點 `$T/alpha` 那一列：開出 `alpha` 的新分頁（`WS_TAB_OPENED`、`WS_TAB_ACTIVE`、`WORKSPACE: state=open`）。`Cmd+1` 回到 `ws-src`，再開選單點同一列：只切換。
- **預期畫面**：每一步結束後分頁列仍是 `ws-src`、`alpha`、`beta` 三個，沒有新分頁；a 到 d 結束後目前分頁是 `alpha`（`ix=1`），標題 `alpha — snip-sync`，工作區選單已關。e 與 f 之後畫面沒變（e 只是把選單關了）。d、d2：選單裡一定有 `$T/alpha` 那一列；沒有就判 `fail`，證據欄寫 `missing-control`。d2 結束後分頁列是 `ws-src`、`alpha` 兩個。
- **比對**：a、b、c 各自 `L:` `[APP:WS_TAB_ACTIVE: id=2 ix=1`，`NOT:` `WS_TAB_OPENED`、`WORKSPACE: state=open`、`WORKSPACE: state=closed`、`WS_TAB_CLOSED`。d：同 a 到 c，`L:` `[APP:WS_TAB_ACTIVE: id=2 ix=1`，`NOT:` `WS_TAB_OPENED`、`WORKSPACE: state=open`。d2 的第一次點擊：`L:` `[APP:WS_TAB_OPENED: id=2 count=2`、`[APP:WS_TAB_ACTIVE: id=2 ix=1`、`[APP:WORKSPACE: state=open path=$T/alpha generation=`；第二次點擊：只有 `[APP:WS_TAB_ACTIVE: id=2 ix=1`，`NOT:` `WS_TAB_OPENED`、`WORKSPACE: state=open`。每一步之後 `wtitle` = `alpha — snip-sync`。e、f：`NOT:` 任何 `[APP:WS_TAB_`、任何 `[APP:WORKSPACE:`。`$T/alpha` 的日誌顯示沒有再出現 `READY_REPOS`。每一步 `clip` 與 `git -C "$T/alpha" status --porcelain` 不變。
- **截圖**：`before.png`、`a-after.png`、`b-after.png`、`c-after.png`、`d-menu.png`（選單與最近開啟列）、`d-after.png`、`e-after.png`、`f-after.png`、`d2-menu.png`、`d2-after.png`。

#### WT03 巢狀資料夾各有自己的分頁

- **前置狀態**：S1，目前 `ws-src`。
- **操作**：(1) `OPEN($B/ws-src/repo01)`（它在已開著的 `ws-src` 底下）。(2) `OPEN($T/alpha)`，然後 `OPEN($T/alpha/sub)`（alpha 底下的一般資料夾，不是 repo）。(3) `Cmd+1`，`Cmd+2`，`Cmd+3`，`Cmd+4`，每次 `wtitle`。
- **預期畫面**：分頁依序 `ws-src`、`repo01`、`alpha`、`sub`，四個分頁各自獨立，不因為路徑包含而合併。`sub` 的工作台是「這個資料夾裡沒有 Git 儲存庫」的空狀態；其他三個正常載入。
- **比對**：(1)：`L:` `[APP:WS_TAB_OPENED: id=2 count=2`、`[APP:WS_TAB_ACTIVE: id=2 ix=1`、`[APP:WORKSPACE: state=open path=$B/ws-src/repo01 generation=`、`[APP:READY_REPOS: 1]`。(2)：`WS_TAB_OPENED: id=3 count=3`、`id=4 count=4`，各自的 `WORKSPACE: state=open path=` 是 `$T/alpha`、`$T/alpha/sub`；`sub` 另有 `[APP:CHANGES_EMPTY: state=no_repository]` 或 `[APP:LOG_EMPTY: state=no_repository]`（視目前左軌）與 `[APP:READY_REPOS: 0]`。(3) 每次只有一行 `WS_TAB_ACTIVE`，`ix` 依序 0、1、2、3，標題依序 `ws-src — snip-sync`、`repo01 — snip-sync`、`alpha — snip-sync`、`sub — snip-sync`。全程 `NOT:` `WS_TAB_CLOSED`。
- **截圖**：`before.png`、`open-repo01.png`、`open-alpha-sub.png`（四個分頁的分頁列）、`sub-empty.png`。

#### WT04 不存在的路徑不開分頁

- **前置狀態**：S3，目前 `beta`（`ix=2`）。
- **操作**：`btn-workspace-menu` → `btn-open-workspace` → 輸入 `$T/no-such-dir` → `btn-workspace-open-confirm`。`shot after`。再按 Escape 關掉選單。
- **預期畫面**：分頁列仍是三個，目前仍是 `beta`；狀態列是「找不到工作區資料夾：」接上輸入的路徑（轉錄進 `screen_text`；不得是原始 key `workspace_bad_path`）。
- **比對**：`NOT:` `WS_TAB_OPENED`、`WS_TAB_ACTIVE`、`WS_TAB_CLOSED`、任何 `[APP:WORKSPACE:`。`wtitle` = `beta — snip-sync`。`$T/no-such-dir` 沒有被建立（`test ! -e`）。
- **截圖**：`before.png`、`after.png`。

### 4.2 「+」與空分頁（WT05–WT08）

#### WT05 「+」開空分頁，工作區選單已打開

- **前置狀態**：S3，目前 `beta`（`ix=2`）。
- **操作**：`pt ws-tab-new`，點。`shot after`。`hv` 到新分頁 `ws-tab:3` 停 2 秒 `shot hover-tab`。
- **預期畫面**：分頁列四個分頁，最後一個是空分頁：標籤「新分頁」、資料夾圖示、目前分頁底色；「+」仍在最右。工作台區域是空工作區，且工作區選單已經打開（看得到「開啟資料夾」「開啟工作區」等列，`btn-workspace-menu` 顯示「未開啟工作區」）。標題恰好是 `snip-sync`（沒有 ` — `）。`hover-tab` 的 tooltip 是「新分頁」。
- **比對**：`L:` `[APP:WS_TAB_OPENED: id=4 count=4`、`[APP:WS_TAB_ACTIVE: id=4 ix=3`。`NOT:` 任何 `[APP:WORKSPACE:`。`ws-tab:3`、`ws-tab-close:3`、`ws-tab-state:3:local` 有 `CTRL_BOUNDS`；`btn-open-workspace` 有新的 `CTRL_BOUNDS`。`wtitle` = `snip-sync`（完整字串，不是 `新分頁 — snip-sync`）。`beta` 分頁沒有印任何 `COPY_`／`PASTE_`／`WORKSPACE` 行。`NOT:` `WS_TAB_OPENED`、`WS_TAB_ACTIVE` 這兩行含 ` ws_tab=`。
- **截圖**：`before.png`、`after.png`、`hover-tab.png`。

#### WT06 在空分頁開工作區：就地填入

- **前置狀態**：WT05 結束的狀態（S3 加一個空分頁，`id=4 ix=3`，目前是它，選單開著）。
- **操作**：`OPEN($T/gamma)`（選單已開，從 `btn-open-workspace` 開始）。`shot after`。
- **預期畫面**：分頁還是四個；`ix=3` 的標籤由「新分頁」變成 `gamma`，仍是資料夾圖示；工作台載入 gamma；標題 `gamma — snip-sync`。
- **比對**：`L:` `[APP:WORKSPACE: state=open path=$T/gamma generation=`、`[APP:READY_REPOS: 1]`。`NOT:` `WS_TAB_OPENED`、`WS_TAB_ACTIVE`、`WS_TAB_CLOSED`。`wtitle` = `gamma — snip-sync`。`ws-tab-state:3:local` 仍是最新的狀態探針。
- **截圖**：`before.png`、`after.png`。

#### WT07 空分頁要開的工作區已經開著：切過去，空分頁消失

- **前置狀態**：S4（`[ws-src 1, alpha 2, beta 3, gamma 4]`，目前 `gamma`），然後點「+」：`[…, 新分頁 id=5 ix=4]`，目前是它，選單開著。
- **操作**：`OPEN($T/alpha)`。`shot after`。
- **預期畫面**：空分頁不見了，分頁列回到四個 `ws-src`、`alpha`、`beta`、`gamma`，目前是 `alpha`（`ix=1`）。標題 `alpha — snip-sync`。沒有出現第二個 alpha。
- **比對**：`L:` `[APP:WS_TAB_ACTIVE: id=2 ix=1`、`[APP:WS_TAB_CLOSED: id=5 count=4`（順序：先切換，再移除空分頁）。`NOT:` `WS_TAB_OPENED`、`[APP:WORKSPACE:`（空分頁沒有工作區可關，alpha 沒有重開）、`WS_TAB_ACTIVE: id=5`。`ws-tab:4` 之後有 `CTRL_GONE`。`wtitle` = `alpha — snip-sync`。`WS_TAB_CLOSED` 之後沒有別的 `WS_TAB_ACTIVE`（目前分頁位置在空分頁之前，不需要再切）。
- **截圖**：`before.png`（空分頁加選單）、`after.png`。

#### WT08 兩個空分頁；切走再回來

- **前置狀態**：S3，目前 `beta`。
- **操作**：(1) 點「+」兩次（空分頁 `id=4 ix=3`、`id=5 ix=4`）。(2) `Cmd+1` 切到 `ws-src`，再 `Cmd+4` 切回第一個空分頁。(3) 在第一個空分頁 `OPEN($T/gamma)` 填入。
- **預期畫面**：(1) 兩個「新分頁」，各有 ×。(2) 切走再切回，空分頁的工作區選單仍然開著（分頁的所有狀態都保留；`leave_tab` 只收掉右鍵選單與拖曳），`btn-open-workspace` 有新的 `CTRL_BOUNDS`。(3) `ix=3` 變成 `gamma`，`ix=4` 仍是「新分頁」。
- **比對**：(1) `WS_TAB_OPENED: id=4 count=4`、`id=5 count=5`，各有 `WS_TAB_ACTIVE`。(2) `WS_TAB_ACTIVE: id=1 ix=0`、`id=4 ix=3`。(3) 只有 `WORKSPACE: state=open path=$T/gamma`，`NOT:` `WS_TAB_OPENED`。標題：空分頁是 `snip-sync`，填入後 `gamma — snip-sync`。
- **截圖**：`before.png`、`two-empty.png`、`switch-back-empty.png`、`filled.png`。

### 4.3 快捷鍵（WT10–WT16）

#### WT10 Cmd+W 關目前分頁

- **前置狀態**：S4，目前改成 `alpha`（`Cmd+2`，`ix=1`）。
- **操作**：(1) `Cmd+W`。(2) `Cmd+3`（現在的第三個，`gamma`），`Cmd+W`。(3) `Cmd+1`，`Cmd+W`。每個步驟之間 `mark`，之後各 `shot`。
- **預期畫面**：(1) `alpha` 消失，分頁列 `ws-src`、`beta`、`gamma`，目前是原本在它右邊的 `beta`（`ix=1`）。(2) `gamma`（最右）消失，目前變成左鄰 `beta`（`ix=1`）。(3) `ws-src`（最左）消失，目前是右鄰 `beta`（`ix=0`）。標題依序 `beta — snip-sync`、`beta — snip-sync`、`beta — snip-sync`。
- **比對**：(1) `L:` `[APP:WORKSPACE: state=closed generation=`、`[APP:WS_TAB_CLOSED: id=2 count=3`、`[APP:WS_TAB_ACTIVE: id=3 ix=1`。(2) 先有 `[APP:WS_TAB_ACTIVE: id=4 ix=2`（Cmd+3），然後 `WORKSPACE: state=closed`、`[APP:WS_TAB_CLOSED: id=4 count=2`、`[APP:WS_TAB_ACTIVE: id=3 ix=1`。(3) 先有 `[APP:WS_TAB_ACTIVE: id=1 ix=0`，然後 `WORKSPACE: state=closed`、`[APP:WS_TAB_CLOSED: id=1 count=1`、`[APP:WS_TAB_ACTIVE: id=3 ix=0`。三步的 `WORKSPACE: state=closed` 都沒有 ` ws_tab=`（被關的分頁關閉時是目前分頁）。每一步 `NOT:` `PASTE_BUSY`、`QUIT`。`$T/alpha`、`$T/gamma`、`$B/ws-src` 的 `git status --porcelain` 前後相同。
- **截圖**：`before.png`、`after-1.png`、`after-2.png`、`after-3.png`。

#### WT11 Cmd+Shift+W 同樣只關目前分頁

- **前置狀態**：S3，目前 `beta`（`ix=2`，最右）。
- **操作**：`Cmd+Shift+W`。`shot after`。
- **預期畫面**：`beta` 消失；分頁列 `ws-src`、`alpha`，目前是左鄰 `alpha`（`ix=1`）。標題 `alpha — snip-sync`。
- **比對**：`L:` `[APP:WORKSPACE: state=closed generation=`、`[APP:WS_TAB_CLOSED: id=3 count=2`、`[APP:WS_TAB_ACTIVE: id=2 ix=1`。`WORKSPACE: state=closed` 那一行不帶 ` ws_tab=`（被關的分頁關閉時是目前分頁）；`NOT:` `QUIT`。`ws-src`、`alpha` 沒有被關（`NOT:` `WS_TAB_CLOSED: id=1`、`WS_TAB_CLOSED: id=2`）。
- **截圖**：`before.png`、`after.png`。

#### WT12 工作區選單的「關閉工作區」關目前分頁

- **前置狀態**：S3，目前改成 `alpha`（`Cmd+2`，`ix=1`）。
- **操作**：`pt btn-workspace-menu`，點；`pt btn-close-workspace`，點。`shot after`。
- **預期畫面**：`alpha` 分頁消失，分頁列 `ws-src`、`beta`，目前是 `beta`（`ix=1`）。App 沒有退到「歡迎畫面」。
- **比對**：`L:` `[APP:WORKSPACE: state=closed generation=`、`[APP:WS_TAB_CLOSED: id=2 count=2`、`[APP:WS_TAB_ACTIVE: id=3 ix=1`。`NOT:` `QUIT`。`wtitle` = `beta — snip-sync`。
- **截圖**：`before.png`、`menu-open.png`、`after.png`。

#### WT13 Cmd+Shift+[ 與 Cmd+Shift+]：上一個、下一個，會環繞

- **前置狀態**：S4，目前 `gamma`（`ix=3`）。
- **操作**：依序按 `Cmd+Shift+]`、`Cmd+Shift+]`、`Cmd+Shift+[`、`Cmd+Shift+[`、`Cmd+Shift+[`，每次之後 `wtitle` 與 `shot`。
- **預期畫面**：目前分頁依序是 `ws-src`（`ix=0`，從最右環繞到最左）、`alpha`、`ws-src`、`gamma`（從最左環繞到最右）、`beta`。
- **比對**：每次只有一行新的 `WS_TAB_ACTIVE`：`id=1 ix=0`、`id=2 ix=1`、`id=1 ix=0`、`id=4 ix=3`、`id=3 ix=2`。標題依序 `ws-src — snip-sync`、`alpha — snip-sync`、`ws-src — snip-sync`、`gamma — snip-sync`、`beta — snip-sync`。`NOT:` `WS_TAB_OPENED`、`WS_TAB_CLOSED`、`WORKSPACE`。
- **截圖**：`before.png`、`step-1.png` 到 `step-5.png`。

#### WT14 Cmd+{ 與 Cmd+}

- **前置狀態**：S4，目前 `gamma`（`ix=3`）。
- **操作**：用鍵盤工具送出字元 `}`（Cmd 加 `}`），再送 `{`（Cmd 加 `{`），各一次，每次之後 `wtitle` 與 `shot`。
- **預期畫面**：`}` 等於下一個（從最右環繞到 `ws-src`），`{` 等於上一個（回到 `gamma`）。
- **比對**：`}` 之後 `[APP:WS_TAB_ACTIVE: id=1 ix=0`；`{` 之後 `[APP:WS_TAB_ACTIVE: id=4 ix=3`。`TODO(verify)`：美式鍵盤上 Cmd+Shift+] 與 Cmd+} 是同一組實體按鍵，WT13 已經涵蓋；工具若只能送實體按鍵、分不出，證據欄寫「同 WT13，無法區分」，這一格判 `pass` 的條件仍是兩行 `WS_TAB_ACTIVE` 都對。
- **截圖**：`before.png`、`next.png`、`prev.png`。

#### WT15 Cmd+1 到 Cmd+9，含超出範圍

- **前置狀態**：S3（三個分頁），目前 `beta`（`ix=2`）。
- **操作**：依序按 `Cmd+1`、`Cmd+2`、`Cmd+3`、`Cmd+3`（再按一次）、`Cmd+4`、`Cmd+5`、`Cmd+6`、`Cmd+7`、`Cmd+8`、`Cmd+9`。每按一次 `mark`，等 1 秒，`wtitle`。最後 `shot after`。再 `OPEN($T/gamma)` 成為第四個分頁（它自動成為目前分頁），依序按 `Cmd+1`、`Cmd+4`、`Cmd+5`。
- **預期畫面**：`Cmd+1` 到 `Cmd+3` 依序切到 `ws-src`、`alpha`、`beta`。第二次 `Cmd+3` 沒有變化。`Cmd+4` 到 `Cmd+9` 沒有變化，目前分頁仍是 `beta`。開第四個分頁後 `Cmd+1` 切到 `ws-src`，`Cmd+4` 切到 `gamma`，`Cmd+5` 沒有變化。
- **比對**：`Cmd+1`：`L:` `[APP:WS_TAB_ACTIVE: id=1 ix=0`；`Cmd+2`：`id=2 ix=1`；`Cmd+3`：`id=3 ix=2`。第二次 `Cmd+3`、`Cmd+4` 到 `Cmd+9`：`NOT:` 任何 `WS_TAB_ACTIVE`，標題不變。第四個分頁後：`Cmd+1` → `id=1 ix=0`，`Cmd+4` → `id=4 ix=3`，`Cmd+5` 沒有行。全程 `NOT:` `WS_TAB_CLOSED`、`QUIT`；超出範圍沒有環繞到最後一個分頁。
- **截圖**：`before.png`、`cmd1.png`、`cmd2.png`、`cmd3.png`、`cmd9-noop.png`、`cmd4-with4.png`。

#### WT16 Ctrl+Tab 還是移動焦點，不切分頁

- **前置狀態**：S3，目前 `alpha`（`Cmd+2`，`ix=1`）。
- **操作**：按 `Ctrl+Tab`，`shot after-1`；再按 `Ctrl+Shift+Tab`，`shot after-2`。（macOS 上是 Control 鍵，不是 Cmd。）
- **預期畫面**：目前分頁仍是 `alpha`、標題不變；鍵盤焦點在工作台內移到下一個／上一個可聚焦的區域（以焦點環或反白的變化轉錄）。
- **比對**：`L:` `[APP:FOCUS: next]`，然後（第二次）`[APP:FOCUS: prev]`。`NOT:` 任何 `WS_TAB_ACTIVE`。`wtitle` 不變。
- **截圖**：`before.png`、`after-1.png`、`after-2.png`。

### 4.4 關閉分頁（WT20–WT23）

#### WT20 × 關目前分頁

- **前置狀態**：S3，目前 `beta`（`ix=2`）。
- **操作**：`hv` 到 `ws-tab-close:2` 停 2 秒，`shot hover-close`。`pt ws-tab-close:2`，點。`shot after`。
- **預期畫面**：`hover-close`：tooltip「關閉分頁 ⌘W」，× 的底色變成 hover 色。點下去之後 `beta` 消失，目前變成左鄰 `alpha`（`ix=1`）。
- **比對**：`L:` `[APP:WORKSPACE: state=closed generation=`、`[APP:WS_TAB_CLOSED: id=3 count=2`、`[APP:WS_TAB_ACTIVE: id=2 ix=1`。`WORKSPACE: state=closed` 那一行不帶 ` ws_tab=`。`wtitle` = `alpha — snip-sync`。點 × 的按下不會穿透成一次切換；它本來就是目前分頁，所以這一格不能證明這件事，WT21 才證明。
- **截圖**：`before.png`、`hover-close.png`、`after.png`。

#### WT21 × 關背景分頁（`ws_tab` 標記）

- **前置狀態**：S3，目前 `beta`（`ix=2`）。
- **操作**：(1) `pt ws-tab-close:0`（`ws-src`，背景分頁），點。`shot after-1`。(2) 現在是 `[alpha id2, beta id3]`，目前 `beta`（`ix=1`）；`Cmd+1` 切到 `alpha`，等 1 秒後 `mark`，然後 `pt ws-tab-close:1`（`beta`，背景分頁，在目前分頁右邊），點。`shot after-2`。
- **預期畫面**：(1) `ws-src` 消失，目前分頁仍是 `beta`，只是位置從 `ix=2` 變成 `ix=1`；標題不變 `beta — snip-sync`；**沒有**切換動作。(2) `beta` 消失，目前分頁仍是 `alpha`（`ix=0`），標題 `alpha — snip-sync`。
- **比對**：(1) `L:` `[APP:WORKSPACE: state=closed generation=<G> ws_tab=1]`（有 ` ws_tab=1`，因為關閉時它是背景分頁）、`[APP:WS_TAB_CLOSED: id=1 count=2`（root 印的，不帶 ` ws_tab=`；`NOT:` 該行含 ` ws_tab=`）。`NOT:` `WS_TAB_ACTIVE`；這一段所有帶 ` ws_tab=` 的行都是 ` ws_tab=1`（`newlog | grep -o ' ws_tab=[0-9]*' | sort -u` 只有一種）。`LIFECYCLE` 行（`intent=close-workspace`）也帶 ` ws_tab=1`。(2) `L:` `WORKSPACE: state=closed generation=<G> ws_tab=3`、`[APP:WS_TAB_CLOSED: id=3 count=1`（同樣不帶 ` ws_tab=`）；`NOT:` `WS_TAB_ACTIVE`。每一段 `wtitle` 在關閉前後相同。
- **截圖**：`before.png`、`after-1.png`、`after-2.png`。

#### WT22 關掉最後一個分頁：只剩「+」，App 還在

- **前置狀態**：S1，目前 `ws-src`（只有一個分頁）。
- **操作**：`Cmd+W`。等 5 秒。`shot after`。`wtitle`。`kill -0 "$PID"; echo $?`。
- **預期畫面**：視窗內只有分頁列（高 34）與「+」；分頁列上沒有任何分頁；下面是一片空的框架底色，沒有工作區選單、沒有歡迎畫面、沒有左軌。標題恰好是 `snip-sync`。程序還活著。
- **比對**：`L:` `[APP:WORKSPACE: state=closed generation=`、`[APP:WS_TAB_CLOSED: id=1 count=0`。`NOT:` `WS_TAB_ACTIVE`、`QUIT`、`phase=drained intent=quit`。`ws-tab:0`、`ws-tab-close:0`、`btn-workspace-menu`、`rail-project` 都有 `CTRL_GONE`；`ws-tab-new` 沒有 `CTRL_GONE`。`kill -0` 結束碼 0。`wtitle` = `snip-sync`。
- **截圖**：`before.png`、`after.png`。

#### WT23 沒有分頁時：快捷鍵不動作，「+」恢復

- **前置狀態**：WT22 結束的狀態（沒有分頁，只有「+」）。
- **操作**：(1) 依序按 `Cmd+W`、`Cmd+Shift+W`、`Cmd+1`、`Cmd+Shift+]`、`Cmd+Shift+[`，每次等 1 秒。(2) `pt ws-tab-new`，點。`shot after-plus`。(3) `OPEN($T/alpha)`（選單已開）。`shot after-open`。(4) **不要**按 `Cmd+Shift+O`（會開原生對話框；誤按就 Escape）。
- **預期畫面**：(1) 畫面沒有變化。(2) 一個「新分頁」（空分頁），選單打開；標題 `snip-sync`。(3) 空分頁變成 `alpha`；標題 `alpha — snip-sync`。
- **比對**：(1) `NOT:` 任何 `WS_TAB_`、`[APP:WORKSPACE:`、`QUIT`。(2) `L:` `[APP:WS_TAB_OPENED: id=2 count=1`（`id` 接著上一個用掉的 id 往下，不重用 1）、`[APP:WS_TAB_ACTIVE: id=2 ix=0`；`btn-open-workspace` 有新的 `CTRL_BOUNDS`。(3) `L:` `[APP:WORKSPACE: state=open path=$T/alpha generation=`、`[APP:READY_REPOS: 1]`；`NOT:` `WS_TAB_OPENED`。
- **截圖**：`before.png`、`after-keys.png`、`after-plus.png`、`after-open.png`。

### 4.5 標籤（WT30–WT33）

#### WT30 很長的資料夾名稱：省略號與 hover 完整路徑

- **前置狀態**：S1，目前 `ws-src`。`$LONGN` 是 `very-long-workspace-folder-name-` 重複四次再加 `end`（約 130 字元）。
- **操作**：`OPEN("$T/$LONGN")`（輸入完整路徑，截圖確認輸入框）。`shot after`。`hv` 到 `ws-tab:1` 停 2 秒 `shot hover-tab`。`wtitle`。
- **預期畫面**：第二個分頁的標籤被截斷成一行、結尾是省略號 `…`；分頁寬度不超過 220 邏輯 px；左邊的資料夾圖示與右邊的 × 都完整看得到，沒有被標籤擠出去。`hover-tab` 的 tooltip 是完整路徑 `$T/$LONGN`（轉錄進 `screen_text`，一字不缺）。標題列用完整標籤（macOS 可能在標題列顯示時截斷，以 `wtitle` 為準）。
- **比對**：`L:` `[APP:WS_TAB_OPENED: id=2 count=2`、`[APP:WS_TAB_ACTIVE: id=2 ix=1`、`WORKSPACE: state=open path=$T/<LONGN> generation=`。最新 `ws-tab:1` 的 `w ≤ 220 × scale`（允許 +2 取整）；`ws-tab-close:1` 的 `x + w ≤ ws-tab:1` 的 `x + w`，並且 `x ≥ ws-tab:1` 的 `x`；`ws-tab-state:1:local` 有 `CTRL_BOUNDS`。`wtitle` 等於 `<LONGN> — snip-sync`（完整字串）。
- **截圖**：`before.png`、`after.png`、`hover-tab.png`。

#### WT31 很多分頁：橫向捲動，目前分頁捲進視野，「+」永遠看得到

- **前置狀態**：S1 之後依序 `OPEN($T/many/workspace-number-01)` 到 `OPEN($T/many/workspace-number-12)`，共 13 個分頁（`id` 1 到 13，`ix` 0 到 12），視窗 1080×720。
- **操作**：(1) 開完第 12 個之後 `shot after-open-12`。(2) `Cmd+1`，`shot cmd1`。(3) `Cmd+9`，`shot cmd9`。(4) 連按 `Cmd+Shift+[` 到回到 `ix=0`，再 `Cmd+Shift+[`（環繞到 `ix=12`），`shot wrap-last`。(5) 在分頁列上做橫向捲動（觸控板兩指橫向，或滑鼠滾輪加 Shift；`TODO(verify)`：看 computer-use 工具送不送得出橫向捲動；送不出時這一步寫 `not-run`，不影響 (1) 到 (4)）。往左捲到底、往右捲到底，各 `shot scroll-left`、`shot scroll-right`。(6) 在 `cmd1` 的狀態點一個只露出一半的分頁（若有）；`TODO(verify)`：點到的分頁整個切換，不是只捲動。
- **預期畫面**：13 個分頁放不下，分頁列出現橫向溢出（沒有換行、沒有第二列、沒有縮到看不見）。(1) 新開的第 13 個分頁（`workspace-number-12`）是目前分頁，完整在視野內，並且在「+」的左邊。(2) 最左的 `ws-src` 在視野內。(3) 第 9 個（`ix=8`，標籤 `workspace-number-08`）在視野內。(4) 環繞到最後一個時，最後一個分頁完整在視野內。全程「+」在最右邊，位置、大小不變，沒有被分頁蓋住。標題隨目前分頁。
- **比對**：開 12 個分頁各有 `WS_TAB_OPENED: id=<n> count=<n>`、`WS_TAB_ACTIVE: id=<n> ix=<n-1>`。(1) 最新 `ws-tab:12` 的 `x ≥ 0`、`x + w ≤ ws-tab-new` 的 `x`（兩者用最新一行 `CTRL_BOUNDS`）。(2) `Cmd+1` 之後 `ws-tab:0` 有新的 `CTRL_BOUNDS` 或維持可見，`x ≥ 0`。(3) `Cmd+9` 之後 `[APP:WS_TAB_ACTIVE: id=9 ix=8`，`ws-tab:8` 完整在視野：`x ≥ 0`，`x + w ≤ ws-tab-new.x`。(4) 環繞：`[APP:WS_TAB_ACTIVE: id=13 ix=12`，`ws-tab:12` 同樣完整在視野。全程 `NOT:` `CTRL_GONE: id=ws-tab-new`；`ws-tab-new` 的 `CTRL_BOUNDS` 若有新行，數字不變。完全捲出視野的分頁出現 `CTRL_GONE: id=ws-tab:<ix>`，只露出一部分的分頁，`CTRL_BOUNDS` 是看得見的部分；如實記錄，不判。標題依序符合目前分頁。
- **截圖**：`before.png`、`after-open-12.png`、`cmd1.png`、`cmd9.png`、`wrap-last.png`、`scroll-left.png`、`scroll-right.png`。

#### WT32 標籤重名：加父資料夾，關掉一個就還原

- **前置狀態**：S1，目前 `ws-src`。
- **操作**：(1) `OPEN($T/one/app)`，`Cmd+N` 走訪（`Cmd+1`、`Cmd+2`），每次 `wtitle` 與 `shot`。(2) `OPEN($T/two/app)`，走訪三個分頁，記下標題與分頁列標籤。(3) 關掉 `one/app`：`Cmd+2`，`Cmd+W`。走訪剩下兩個，記標題。(4) 重新 `OPEN($T/one/app)`。(5) 再 `OPEN($T/a/x/app)` 與 `OPEN($T/b/x/app)`，走訪全部。
- **預期畫面**：(1) 只有一個 `app` 時標籤是 `app`（標題 `app — snip-sync`）。(2) 兩個 `app` 重名：分頁列標籤變成 `one/app`、`two/app`，兩者都帶上一層。(3) 關掉 `one/app` 後，剩下的標籤退回 `app`。(4) 兩個又是 `one/app`、`two/app`。(5) 四個 `app` 的最終標籤：`one/app`、`two/app`、`a/x/app`、`b/x/app`（`x/app` 還是重名，再往上一層）。`ws-src` 與 `alpha` 之類不重名的標籤不受影響。標籤文字轉錄進 `screen_text`。
- **比對**：每個走訪步驟之後 `wtitle` 與當時分頁列標籤一致：(1) `app — snip-sync`；(2) `one/app — snip-sync`、`two/app — snip-sync`；(3) `app — snip-sync`；(4)(5) 對應的完整標籤。每個 `OPEN` 都有 `WS_TAB_OPENED` 與 `WS_TAB_ACTIVE`；(3) 的 `Cmd+W` 有 `WORKSPACE: state=closed`、`WS_TAB_CLOSED`。
- **截圖**：`before.png`、`one-app.png`、`dup-two.png`、`after-close-one.png`、`dup-again.png`、`four-app.png`。

#### WT33 空分頁的標籤與英文介面

- **前置狀態**：S3，目前 `beta`，再點「+」（空分頁 `id=4 ix=3`，目前是它）。
- **操作**：(1) `hv` 到 `ws-tab:3` 停 2 秒 `shot hover-zh`；`hv` 到 `ws-tab-close:3` 停 2 秒 `shot close-zh`；`hv` 到 `ws-tab-new` 停 2 秒 `shot plus-zh`。(2) 按 `Option+L` 切英文，再做同樣三個 hover，`shot hover-en`、`close-en`、`plus-en`。(3) `Cmd+1` 切到 `ws-src`，`shot other-tab-locale`，`hv` 到 `ws-tab:3` 停 2 秒 `shot other-tab-tab`，`hv` 到 `ws-tab-new` 停 2 秒 `shot other-tab-plus`，再 `Cmd+4` 切回空分頁，`shot back-en`。(4) 在空分頁再按 `Option+L` 切回中文。
- **預期畫面**：(1) 空分頁標籤「新分頁」，tooltip 依序是「新分頁」、「關閉分頁 ⌘W」、「開新的工作區分頁」。(2) 英文：標籤 `New tab`，tooltip `New tab`、`Close tab ⌘W`、`New workspace tab`。(3) `Option+L` 只切目前分頁的語言（每個分頁的 model 自己存）；分頁列自己的文字（空分頁的標籤「新分頁」／`New tab`、× 與「+」的 tooltip）跟著**目前分頁**的語言：切到 `ws-src`（中文）時是「新分頁」、「關閉分頁 ⌘W」、「開新的工作區分頁」，切回空分頁（英文）時又是 `New tab`、`Close tab ⌘W`、`New workspace tab`。(4) 回到中文。
- **比對**：`L:` 每次 `Option+L` 有新的 `[APP:LOCALE: …]` 行（`LOCALE` 行的值以 binary 為準）。`wtitle` 在空分頁時是 `snip-sync`。`NOT:` `WS_TAB_OPENED`、`WS_TAB_CLOSED`。
- **截圖**：`before.png`、`hover-zh.png`、`close-zh.png`、`plus-zh.png`、`hover-en.png`、`close-en.png`、`plus-en.png`、`other-tab-locale.png`、`other-tab-tab.png`、`other-tab-plus.png`、`back-en.png`。

### 4.6 遠端分頁（WT40–WT46）

這一節需要遠端規程的 Ubuntu 準備（見 1.4）。主機別名是 `ubuntu`，`$W` 是 Ubuntu 上本輪的目錄（含 `$SHA`）。每一格開始前 `ssh ubuntu "ls '$W/pids' | wc -l"` 記下 worker 數 `K0`。

#### WT40 遠端分頁：標籤 `host ▸ name`、已連線圖示

- **前置狀態**：S1，目前 `ws-src`。
- **操作**：`btn-workspace-menu`，點 `ubuntu` 那一列的 `remote-host:<ix>`（先看截圖確認名稱），依序點進 `snip-ui-run`、`<SHA>`、`gitws`（每一層的 `remote-folder:<n>` 以 `CTRL_BOUNDS` 為準），點 `btn-remote-open-here`。等載入完成。`shot after`。`hv` 到 `ws-tab:1` 停 2 秒 `shot hover-tab`。`wtitle`。
- **預期畫面**：兩個分頁，第二個標籤 `ubuntu ▸ gitws`，圖示是「遠端分支」圖示（強調色，不是資料夾），是目前分頁。標題 `ubuntu ▸ gitws — snip-sync`。`hover-tab` 的 tooltip 是 `ubuntu:/home/audichuang/snip-ui-run/<SHA>/gitws`（轉錄）。`ws-src` 的圖示仍是資料夾。
- **比對**：`L:` `[APP:WS_TAB_OPENED: id=2 count=2`、`[APP:WS_TAB_ACTIVE: id=2 ix=1`、`[APP:REMOTE_OPENED: ubuntu ▸ gitws generation=`（順序：worker 先在發出請求的分頁解析路徑，之後 root 開新分頁，新分頁排空並開啟，所以依序是這三行；用行號確認，三行都出現且順序對才算過）。最新的狀態探針是 `ws-tab-state:1:connected`（`ws-tab-state:0:local` 仍在）。`wtitle` = `ubuntu ▸ gitws — snip-sync`。`ssh ubuntu "ls '$W/pids' | wc -l"` 比 `K0` 多 1。
- **截圖**：`before.png`、`after.png`、`hover-tab.png`。

#### WT41 連線中的圖示

- **前置狀態**：S1，目前 `ws-src`。`ssh ubuntu "rm -f '$W/connect-delay'"` 確認沒有延遲檔。
- **操作**：(1) 點「+」（空分頁 `id=2 ix=1`，選單已開）。(2) 點 `remote-host:<ix>`（`ubuntu`）。**列出資料夾的這段時間**，空分頁就會顯示連線中（`tab_info` 對「沒有工作區、遠端忙碌」的分頁回報 `Connecting`），所以立刻連續 `shot browsing-1`、`browsing-2`（間隔約 1 秒）。(3) 逐層點進 `snip-ui-run/<SHA>/gitws`，點 `btn-remote-open-here`，立刻 `shot opening-1`。(4) 等載入完成，`shot connected`。(5) 退路（(2) 與 (3) 都沒抓到暫態，才做）：把 Ubuntu 的 wrapper 換成會延遲的版本（遠端規程 2.3 的 wrapper 多一行，`$W/connect-delay` 存在時先睡 6 秒），重做 (1) 到 (4)。這一格結束前一定 `ssh ubuntu "rm -f '$W/connect-delay'"`，並把 wrapper 還原：
  ```bash
  printf '#!/bin/sh\necho $$ > "%s/pids/$$"\n[ -f "%s/connect-delay" ] && sleep 6\nexport SNIP_E2E_PASTE_HOLD="%s/paste-hold"\nexec "%s/src/target/release/snip" "$@"\n' "$W" "$W" "$W" "$W" | ssh ubuntu 'cat > ~/.local/bin/snip && chmod +x ~/.local/bin/snip'
  ssh ubuntu 'sha256sum ~/.local/bin/snip' > "$RUN/snip-installed.sha"     # 收尾比對用：覆寫成新的 wrapper 的雜湊（用完不還原也行，收尾只刪雜湊相同的那份）
  ```
- **預期畫面**：連線中：空分頁的標籤是「新分頁」，圖示是「重新整理」圖示（灰），tooltip 是「新分頁 · 連線中」（沒有路徑時用的標籤）。連線完成後，空分頁被填入，變成 `ubuntu ▸ gitws` 與「遠端分支」圖示。
- **比對**：`ws-tab-state:1:connecting` 在日誌裡有過 `CTRL_BOUNDS`，之後才出現 `ws-tab-state:1:connected` 的 `CTRL_BOUNDS`（用行號確認先後），並且 `connecting` 那個 ID 有 `CTRL_GONE`（`TODO(verify)`：連線中的暫態可能是 `local`→`connecting`→`connected`，也可能中間還有 `local`，如實記錄整條序列）。`L:` `[APP:REMOTE_OPENED: ubuntu ▸ gitws generation=`；空分頁被填入，所以 `NOT:` 第二次 `WS_TAB_OPENED`。最後 `wtitle` = `ubuntu ▸ gitws — snip-sync`。`ssh ubuntu "test ! -e '$W/connect-delay'"` 成立。整條路都沒有抓到 `connecting`：這一格寫 `not-run`，證據寫「暫態沒抓到」。
- **截圖**：`before.png`、`browsing-1.png`、`browsing-2.png`、`opening-1.png`、`connected.png`。

#### WT42 連線失敗的圖示

- **前置狀態**：S1，目前 `ws-src`。`TODO(verify)`：「失敗」狀態要 worker 連上、但倉庫掃描失敗（`remote.scan_error`）。寫這份規程時沒有確定的觸發方式。建議的觸發：`ssh ubuntu "mkdir -p '$W/scanfail/r1' && git init -q '$W/scanfail/r1' && chmod 000 '$W/scanfail/r1/.git'"`，開 `$W/scanfail`。
- **操作**：在 `scanfail` 上做 WT40 的流程。等 60 秒。`shot after`、`hv` 到 `ws-tab:1` 停 2 秒 `shot hover-failed`。結束後 `ssh ubuntu "chmod -R u+rwx '$W/scanfail'; rm -rf '$W/scanfail'"`。
- **預期畫面**：遠端分頁圖示是警告圖示（錯誤色），tooltip 以完整路徑開頭、尾巴 ` · 連線失敗`。
- **比對**：最新的狀態探針是 `ws-tab-state:1:failed`（它的 `CTRL_BOUNDS`），`L:` `[APP:REMOTE_SCAN_FAILED: `（日誌行以 binary 為準）。60 秒內沒有出現 `failed` 狀態時：這一格寫 `not-run`，證據欄寫「無可靠觸發」，並附上實際看到的狀態探針與日誌；不要判 `fail`。
- **截圖**：`before.png`、`after.png`、`hover-failed.png`。

#### WT43 本機與遠端混合

- **前置狀態**：S1，之後 `OPEN` 遠端 `gitws`（`id=2`），再 `OPEN($T/alpha)`（`id=3`）：`[ws-src, ubuntu ▸ gitws, alpha]`，目前 `alpha`。
- **操作**：(1) `Cmd+2` 切到遠端分頁；`rail-changes`，選 Changes 的 `a.txt`（未暫存，遠端規程 X06 的 `new.txt` 也可，路徑以 `CTRL_BOUNDS` 為準），按 `Cmd+C`。(2) `Cmd+3` 切到 `alpha`，`Cmd+V`。看到預覽後按 Escape。(3) `Cmd+1`、`Cmd+2`、`Cmd+3` 輪流切，每次 `wtitle`。
- **預期畫面**：三個分頁依建立順序排列：資料夾圖示、遠端分支圖示、資料夾圖示。標題依序 `ws-src — snip-sync`、`ubuntu ▸ gitws — snip-sync`、`alpha — snip-sync`。遠端分頁切走再切回來後仍是已連線圖示，內容沒有重載。
- **比對**：(1) `[APP:COPY_DONE: copied=1]`（遠端的複製在 worker 端做）；`pbpaste` 的 SHA-256 等於遠端規程 X01／X06 該格的 oracle 清單（照遠端規程 4.6 的 `snip_payload_list` 比對）。(2) `[APP:PASTE_PREVIEW` 的 `dest=` 是 `$T/alpha`，`[APP:PASTE_CANCELLED]`；`git -C "$T/alpha" status --porcelain` 為空。(3) `L:` `WS_TAB_ACTIVE` 依序 `id=1 ix=0`、`id=2 ix=1`、`id=3 ix=2`，`NOT:` `READY_REPOS`、`DISCOVERY_PROGRESS`（切換不重新整理）。三個分頁各自的狀態探針：`ws-tab-state:0:local`、`ws-tab-state:1:connected`、`ws-tab-state:2:local`。
- **截圖**：`before.png`、`remote-copied.png`、`local-preview.png`、`switch-1.png`、`switch-2.png`、`switch-3.png`。

#### WT44 同一個遠端資料夾的不同拼法：切換，不新增

- **前置狀態**：WT43 的 `[ws-src, ubuntu ▸ gitws, alpha]`，目前 `alpha`（`ix=2`）。
- **操作**：依序用下面四種方式再開一次 `gitws`，每次之前先 `Cmd+3` 回到 `alpha`，`mark`：(a) `btn-workspace-menu`，在 `remote-path-input` 輸入 `/home/audichuang/snip-ui-run/<SHA>/gitws`（絕對路徑），`btn-remote-open`。(b) 輸入 `~/snip-ui-run/<SHA>/gitws`。(c) 輸入路徑結尾多一個斜線 `/home/audichuang/snip-ui-run/<SHA>/gitws/`。(d) 選單最上面的 `remote-recent:<n>`（先 `hv` 看 tooltip，是 `ubuntu:/home/audichuang/snip-ui-run/<SHA>/gitws` 那一列）。
- **預期畫面**：每一次之後分頁列仍是三個，目前切到 `ubuntu ▸ gitws`（`ix=1`），沒有第四個分頁。(b) 的 `~` 與 (c) 的結尾斜線：worker 回傳它解析後的真實路徑，所以一樣是切換；若開出第四個分頁，這一格判 `fail`，證據寫是哪一種拼法。
- **比對**：每次 `L:` `[APP:WS_TAB_ACTIVE: id=2 ix=1`；`NOT:` `WS_TAB_OPENED`、`WORKSPACE: state=`。`NOT:` `REMOTE_OPENED`（只有分頁真的開了遠端工作區才印；只是切換不印）。解析路徑的探測用短命的連線。`ssh ubuntu "ls '$W/pids' | wc -l"` 的增量：每一次新連線可能多一個 worker 紀錄，記下來；本格不判，`WT46` 才驗證收尾。每一次之後 `wtitle` = `ubuntu ▸ gitws — snip-sync`。
- **截圖**：`before.png`、`a-after.png`、`b-after.png`、`c-after.png`、`d-menu.png`、`d-after.png`。

#### WT45 同一台機器的兩個 Host 別名是兩個分頁

- **前置狀態**：S1。`~/.ssh/config` 必須已經有第二個指到同一台機器的別名（本輪**不改** `~/.ssh/config`）。檢查：`ssh -G ubuntu | grep -E '^(hostname|user|port) '` 與 `ssh -G <別名2> | grep -E '^(hostname|user|port) '` 相同。沒有第二個別名，這一格寫 `not-run`，證據欄寫「沒有第二個別名」。
- **操作**：用別名 `ubuntu` 開 `gitws`（WT40），再用別名 2 開同一個真實路徑的 `gitws`。`shot after`。
- **預期畫面**：三個分頁：`ws-src`、`ubuntu ▸ gitws`、`<別名2> ▸ gitws`，兩個遠端分頁各自是已連線圖示。
- **比對**：第二次 `L:` `[APP:WS_TAB_OPENED: id=3 count=3`、`[APP:WS_TAB_ACTIVE: id=3 ix=2`、`[APP:REMOTE_OPENED: <別名2> ▸ gitws generation=`。`NOT:` 把第二次當成切換（沒有 `WS_TAB_ACTIVE: id=2`）。`wtitle` = `<別名2> ▸ gitws — snip-sync`。`ssh ubuntu "ls '$W/pids' | wc -l"` 比開之前多 2。
- **截圖**：`before.png`、`after.png`。

#### WT46 關掉遠端分頁，worker 結束

- **前置狀態**：S1 之後 `OPEN` 遠端 `gitws`（`id=2 ix=1`），目前是它。`K1` = `ssh ubuntu "ls '$W/pids' | wc -l"`。
- **操作**：(1) `Cmd+W`。(2) 等 10 秒。(3) `ssh ubuntu "ps -o pid=,args= -u audichuang | grep -F '$W/src/target/release/snip serve --stdio' | grep -v grep"`。
- **預期畫面**：遠端分頁消失，目前是 `ws-src`，標題 `ws-src — snip-sync`。
- **比對**：`L:` `[APP:WORKSPACE: state=closed generation=`、`[APP:WS_TAB_CLOSED: id=2 count=1`、`[APP:WS_TAB_ACTIVE: id=1 ix=0`。10 秒內 (3) 沒有本輪這個分頁的 worker（`ps` 輸出的 PID 都不在這個分頁開啟時新增的 `$W/pids` 紀錄裡）。關閉遠端分頁會放掉它的 session 與 ssh 連線，worker 應在 10 秒內消失，這就是通過線；超過 10 秒仍在判 `fail`，並記錄秒數。收尾的 `stop_run_workers` 仍必須結束碼 0。
- **截圖**：`before.png`、`after.png`。

### 4.7 跨分頁複製與貼上（WT50–WT52）

共同前置：S1 之後 `OPEN($T/src-a)`（`id=2`）、`OPEN($T/dst-b)`（`id=3`）：`[ws-src, src-a, dst-b]`，目前 `dst-b`（`ix=2`）。專案樹列的 ID 以 binary 印出的為準（單一 repo 工作區是 `tree-row:<path>`，必要時先點 `repo-row:src-a` 展開；一般資料夾或多 repo 是 `ws-tree-row:<path>`）。

#### WT50 檔案模式：在 A 複製，在 B 貼上

- **前置狀態**：共同前置，目前改成 `src-a`（`Cmd+2`，`ix=1`）。`git -C "$T/dst-b" rev-parse HEAD` 記為 `H0`；`$T/dst-b` 的 `status --porcelain` 為空。先放 sentinel：`printf 'sentinel-%s' "$RANDOM" | pbcopy`，`clip` 記為 `C0`。
- **操作**：(1) 點 `tree-row:hello.txt`（選取），按 `Cmd+C`。`clip > "$CASE/clip-copy.sha"`。(2) `pt ws-tab:2`，點（切到 `dst-b`）。(3) `Cmd+V`。看到預覽，`shot preview`。(4) 點 `btn-apply`。`shot after-apply`。
- **預期畫面**：(2) 切到 `dst-b`，標題 `dst-b — snip-sync`。(3) 貼上面板列出一個新增的 `hello.txt`。(4) 套用完成，`dst-b` 的專案樹出現 `hello.txt`。
- **比對**：(1) `L:` `[APP:COPY_DONE: copied=1]`；`pbpaste` 含 `hello-a`，SHA 不等於 `C0`。(2) `L:` `[APP:WS_TAB_ACTIVE: id=3 ix=2`，`NOT:` 帶 ` ws_tab=` 的 `[APP:COPY_`／`[APP:PASTE_` 行（切換之後的複製與貼上都來自目前分頁）。(3) `L:` `[APP:PASTE_PREVIEW` 且該行含 `dest=$T/dst-b`；`btn-apply`、`paste-row:<ix>:hello.txt` 有 `CTRL_BOUNDS`；剪貼簿 SHA 仍等於複製當下。(4) `L:` `[APP:PASTE_APPLYING]`、`[APP:PASTE_DONE: created=1 overwritten=0 skipped=0 deleted=0 errors=0`。磁碟：`$T/dst-b/hello.txt` 的 SHA-256 等於 `printf 'hello-a' | shasum -a 256`（檔案模式不保留檔尾換行，來源刻意沒有換行）；`git -C "$T/dst-b" status --porcelain` 是 `?? hello.txt`；`git -C "$T/dst-b" rev-parse HEAD` 等於 `H0`；`git -C "$T/src-a" status --porcelain` 為空；剪貼簿 `clip` 等於 (1) 的值（貼上不改剪貼簿）。
- **截圖**：`before.png`、`copied.png`、`preview.png`、`after-apply.png`。

#### WT51 commit 模式：在 A 複製，在 B 貼上

- **前置狀態**：共同前置；WT50 的 `hello.txt` 先清掉（`git -C "$T/dst-b" clean -fdq`）；目前 `src-a`（`Cmd+2`）。`H0=$(git -C "$T/dst-b" rev-parse HEAD)`；`SRC7=$(git -C "$T/src-a" rev-parse --short=7 HEAD)`（`tabs commit one` 那個提交）。
- **操作**：(1) `rail-log`，點 `commit-row:$SRC7`，點 `btn-copy-commits`。`shot copied`；`clip > "$CASE/clip-copy.sha"`。(2) `pt ws-tab:2`，點（切到 `dst-b`）。(3) `Cmd+V`，`shot preview`。(4) 點 `btn-apply`。`shot after-apply`。
- **預期畫面**：(1) 通知「已複製 1 個 commit…」。(3) 面板有一個提交標頭 `paste-commit:0`（`tabs commit one`），列出新增的 `c1.txt`。(4) 完成。
- **比對**：(1) `L:` `[APP:COPY_COMMITS_DONE: commits=1]`、`[APP:TOAST: ok=true]`；`pbpaste` 第一行是 `// snip-sync commits v1`。(2) `L:` `[APP:WS_TAB_ACTIVE: id=3 ix=2`。(3) `[APP:PASTE_PREVIEW`，含 `dest=$T/dst-b`。(4) `L:` `[APP:PASTE_APPLYING]`、`[APP:PASTE_DONE: created=1 overwritten=0 skipped=0 deleted=0 errors=0 commits=1]`。Git：`git -C "$T/dst-b" rev-list --count "$H0"..HEAD` 是 `1`；`git -C "$T/dst-b" show -s --format='%an%x00%ae%x00%s' HEAD` 是 `t`、`t@t`、`tabs commit one`；`git -C "$T/dst-b" show --name-only --format= HEAD` 只有 `c1.txt`，內容 `c1`；`$T/src-a` 的 HEAD 與 status 不變；`clip` 等於 (1) 的 SHA。
- **截圖**：`before.png`、`copied.png`、`preview.png`、`after-apply.png`。

#### WT52 點分頁之後第一次 Cmd+C、Cmd+V 只作用在那個分頁

- **前置狀態**：共同前置，目前 `dst-b`。`$T/src-a`、`$T/dst-b` 都乾淨（`status --porcelain` 為空），剪貼簿放 sentinel（`C0`）。
- **操作**：(1) `Cmd+2`（`src-a`），點 `tree-row:hello.txt`（選取，不複製）。(2) 點 `pt ws-tab:2`（`dst-b`），點 `tree-row:base.txt`（選取，不複製）。(3) `mark`。點 `pt ws-tab:1`（`src-a`），**不要再點別處**，立刻按 `Cmd+C`。`clip > "$CASE/c3.sha"`；`pbpaste > "$CASE/c3.txt"`。(4) `mark`。點 `pt ws-tab:2`（`dst-b`），立刻按 `Cmd+C`。`pbpaste > "$CASE/c4.txt"`。(5) `mark`。點 `pt ws-tab:1`（`src-a`），立刻按 `Cmd+V`。`shot preview-src-a`。(6) 按 Escape。(7) `mark`。點 `pt ws-tab:2`（`dst-b`）。`shot dst-b-clean`。
- **預期畫面**：(3) 通知是複製成功；`src-a` 仍顯示它先前選取的 `hello.txt`。(5) 預覽面板出現在 `src-a`（內容是 `dst-b` 的 `base.txt` 要建立）。(6) 面板關閉。(7) `dst-b` 沒有貼上面板。
- **比對**：(3) `L:` `[APP:WS_TAB_ACTIVE: id=2 ix=1`、`[APP:COPY_DONE: copied=1]`；`NOT:` 帶 ` ws_tab=3` 的 `[APP:COPY_`／`[APP:PASTE_` 行；`c3.txt` 含 `hello-a`、不含 `base-b`。(4) `L:` `[APP:WS_TAB_ACTIVE: id=3 ix=2`、`[APP:COPY_DONE: copied=1]`；`NOT:` 帶 ` ws_tab=2` 的 `[APP:COPY_`／`[APP:PASTE_` 行；`c4.txt` 含 `base-b`、不含 `hello-a`。(5) `L:` `[APP:WS_TAB_ACTIVE: id=2 ix=1`、`[APP:PASTE_PREVIEW`（`dest=$T/src-a`）；`NOT:` ` ws_tab=3`。(6) `L:` `[APP:PASTE_CANCELLED]`；`git -C "$T/src-a" status --porcelain` 為空。(7) `NOT:` `PASTE_PREVIEW`、`PASTE_APPLYING`、` ws_tab=`；`btn-apply` 之後沒有新的 `CTRL_BOUNDS`（若有 `CTRL_GONE` 是 (6) 關面板留下的）。
- **截圖**：`before.png`、`c3.png`、`c4.png`、`preview-src-a.png`、`dst-b-clean.png`。

### 4.8 背景分頁的狀態（WT53–WT54）

#### WT53 背景分頁保留選取、捲動位置、面板寬度

- **前置狀態**：S3，目前 `ws-src`（`Cmd+1`），1080×720。
- **操作**：在 `ws-src`：(1) `btn-repo-selector` → `pick-repo:repo03`。(2) `rail-changes`，點一個變更列（`change-row@repo03:<source>:unrelated.txt`，ID 以 `CTRL_BOUNDS` 為準）。(3) 把 `splitter-left` 往右拖 80 邏輯 px（按住 `splitter-left` 的中心，水平拖曳，放開）。`shot ws-src-set`，記下最新 `splitter-left` 的 `x`、`SPLIT_RESIZED` 的 `left_w`。(4) 點 `ws-tab:2`（`beta`），在 `beta` 做點不同的事（`rail-log`），等 3 秒。(5) `mark`。點 `ws-tab:0`（`ws-src`）。`shot ws-src-back`。(6) `mark`。點 `ws-tab:2`（`beta`）。`shot beta-back`。
- **預期畫面**：(5) `ws-src` 和離開前一模一樣：`rail-changes` 還開著、repo 選擇器顯示 `repo03`、選取的變更列還是反白、左側面板寬度不變（`splitter-left` 的 x 與 (3) 相同，從截圖量）。(6) `beta` 還停在 `rail-log`。
- **比對**：(1) `L:` `[APP:REPO_SELECTING`（含 repo03）。(3) `L:` `[APP:SPLIT_RESIZED: Left left_w=`。(5) `L:` `[APP:WS_TAB_ACTIVE: id=1 ix=0`；`NOT:` `[APP:SPLIT_RESIZED`、`REPO_SELECTING`、`DISCOVERY_PROGRESS`、`READY_REPOS`。最新 `splitter-left` 的 `x` 等於 (3) 的值（切回後日誌若沒有新行，以最新一行與截圖量測為準，兩者都寫進 `action.json`）。(6) 同樣沒有重新載入的行。
- **截圖**：`before.png`、`ws-src-set.png`、`ws-src-back.png`、`beta-back.png`。

#### WT54 背景分頁不會在切回時重新整理；Cmd+R 才更新

- **前置狀態**：S3 之後把 `alpha` 當 A：目前 `alpha`，`rail-changes`（乾淨，`[APP:CHANGES_EMPTY: state=clean]`），`$T/alpha` 的 `status --porcelain` 是空的（`alpha/sub/s.txt` 先 `git -C "$T/alpha" add -A && git -C "$T/alpha" -c user.name=t -c user.email=t@t commit -qm sub`，讓它乾淨）。
- **操作**：(1) `Cmd+3`（`beta`）。(2) 外部修改：`printf 'bg-change' > "$T/alpha/readme.txt"`。等 5 秒。(3) `mark`。`Cmd+2`（回到 `alpha`）。等 3 秒。`shot after-back`。(4) `mark`。按 `Cmd+R`。等 3 秒。`shot after-refresh`。
- **預期畫面**：(3) `alpha` 的 Changes 仍是空的（乾淨狀態，沒有 `readme.txt` 這一列）。App 沒有檔案監看，切回不重新整理，只有 Cmd+R 會重新載入；若切回就看到 `readme.txt`，記錄並判 `fail`（與規格「切回不重新整理」不符），證據寫實際看到的日誌。(4) 變更列出現 `readme.txt`。
- **比對**：(3) `L:` `[APP:WS_TAB_ACTIVE: id=2 ix=1`；`NOT:` `DISCOVERY_PROGRESS`、`READY_REPOS`、`CHANGES_EMPTY: state=clean` 以外的新 changes 載入行；沒有 `change-row@…readme.txt` 的 `CTRL_BOUNDS`。(4) `L:` `[APP:DISCOVERY_PROGRESS:`、`[APP:READY_REPOS: 1]`；之後 `change-row@<repo>:unstaged:readme.txt` 有 `CTRL_BOUNDS`。`git -C "$T/alpha" status --porcelain` 是 ` M readme.txt`。
- **截圖**：`before.png`、`after-back.png`、`after-refresh.png`。

### 4.9 貼上寫入中（WT60–WT65）

這一節需要遠端規程的 Ubuntu 準備與 wrapper（`SNIP_E2E_PASTE_HOLD` 只存在 Ubuntu 的 worker）。本機貼上沒有暫停點，所以「貼上中」只在遠端分頁做得到確定的時機；本機版本見 WT65。hold 檔最多只擋 60 秒：每一格從建立 hold 起算 45 秒內做完，超過就重做這一格。

共同設定（每一格都要）：
```bash
ssh ubuntu "rm -f '$W/paste-hold'; rm -rf '$W/pastews/cut-dst'; mkdir -p '$W/pastews/cut-dst'"
(cd "$T/busy-src" && "$SNIP" copy . --stdout) > "$RUN/p-busy.txt" && pbcopy < "$RUN/p-busy.txt"     # 檔案模式 payload：new.txt、new2.txt（兩個新檔）
clip > "$RUN/p-busy.sha"
```
分頁：S1 之後 `OPEN($T/alpha)`（`id=2`），再開遠端 `$W/pastews/cut-dst`（`id=3`，標籤 `ubuntu ▸ cut-dst`），`[ws-src, alpha, ubuntu ▸ cut-dst]`，目前是遠端分頁（`ix=2`）。`Cmd+V` 看到預覽（`PASTE_PREVIEW`，兩個新增列），然後 `ssh ubuntu "touch '$W/paste-hold'"`，再點 `btn-apply`。這時 worker 在第一個檔案寫入後暫停。下面稱這個狀態為「寫入中」：日誌有 `[APP:PASTE_APPLYING]`、沒有 `PASTE_DONE`，`ws-tab-pasting:2` 有 `CTRL_BOUNDS`，分頁 tooltip 結尾是 ` · 貼上中`。

放開 hold 的方法：`ssh ubuntu "rm -f '$W/paste-hold'"`。

#### WT60 目前分頁寫入中：Cmd+W 與 × 都被拒絕

- **前置狀態**：「寫入中」，目前是遠端分頁（`ix=2`）。
- **操作**：(1) `mark`，`Cmd+W`，等 1 秒，`shot refused-cmdw`。(2) `mark`，`pt ws-tab-close:2`，點，等 1 秒，`shot refused-x`。(3) `hv` 到 `ws-tab:2` 停 2 秒，`shot hover-pasting`。(4) 放開 hold，等 `PASTE_DONE`，`shot after-done`。
- **預期畫面**：(1)(2) 分頁還在，沒有關；狀態列是「正在套用變更，已拒絕關閉、開啟與結束，以免寫入中斷。」（轉錄；不得是原始 key `workspace_busy_applying`）。分頁標籤右邊有「貼上」圖示（強調色）。(3) tooltip 是 `ubuntu:<路徑> · 貼上中`。(4) 貼上圖示消失。
- **比對**：(1)(2) 各 `L:` `[APP:PASTE_BUSY: refused=close-workspace]`、`[APP:LIFECYCLE: phase=refused intent=close-workspace reason=applying`；`NOT:` `WS_TAB_CLOSED`、`[APP:WORKSPACE: state=closed`。(3) 寫入中 `ws-tab-pasting:2` 有 `CTRL_BOUNDS`。(4) `L:` `[APP:PASTE_DONE: created=2 overwritten=0 skipped=0 deleted=0 errors=0`；之後 `CTRL_GONE: id=ws-tab-pasting:2`。磁碟：`ssh ubuntu "cd '$W/pastews/cut-dst' && sha256sum new.txt new2.txt"` 與「`ssh ubuntu "cd '$W/oracle/cut-dst' && snip paste --apply --stdin" < "$RUN/p-busy.txt"`（先 `mkdir -p '$W/oracle/cut-dst'`）之後同樣兩個檔的 `sha256sum`」相同。
- **截圖**：`before.png`、`refused-cmdw.png`、`refused-x.png`、`hover-pasting.png`、`after-done.png`。

#### WT61 背景分頁寫入中：標記、圖示，以及對它按 ×

分兩段，各自從共同設定重做一次（hold 只有 60 秒）。

- **前置狀態**：「寫入中」，目前先點 `ws-tab:0`（`ws-src`），讓遠端分頁成為背景分頁（日誌 `[APP:WS_TAB_ACTIVE: id=1 ix=0`）。
- **操作**：
  - 61a（背景分頁自己完成）：`shot a-pasting-in-background`。`mark`。放開 hold，等 5 秒。`shot a-after-done`。`hv` 到 `ws-tab:2` 停 2 秒，`shot a-hover`。
  - 61b（對背景分頁按 ×）：重做共同設定，回到「寫入中」並讓 `ws-src` 是目前分頁。`mark`。`pt ws-tab-close:2`，點（背景分頁的 ×），等 1 秒。`shot b-after-x`。放開 hold，等 `PASTE_DONE`。`shot b-after-done`。
- **預期畫面**：61a：背景期間貼上圖示在 `ubuntu ▸ cut-dst` 分頁上仍看得到；完成後圖示消失，目前分頁仍是 `ws-src`，沒有任何切換。61b：點 × 之後視窗切到遠端分頁（`ix=2` 成為目前分頁），狀態列是「正在套用變更，已拒絕關閉、開啟與結束，以免寫入中斷。」，分頁沒有關；完成後圖示消失。
- **比對**：61a：`L:` `[APP:PASTE_DONE: created=2 overwritten=0 skipped=0 deleted=0 errors=0` 且該行結尾 `]` 前帶 ` ws_tab=3`（背景分頁印的行；貼上工作以那個分頁的程式執行）；`CTRL_GONE: id=ws-tab-pasting:2`；`NOT:` `WS_TAB_ACTIVE`。磁碟：`new.txt`、`new2.txt` 的 `sha256sum` 同 WT60。61b：`L:`（依序）`[APP:WS_TAB_ACTIVE: id=3 ix=2`、`[APP:PASTE_BUSY: refused=close-workspace]`（沒有 ` ws_tab=`，因為此時已是目前分頁）；`NOT:` `WS_TAB_CLOSED`、`[APP:WORKSPACE: state=closed`。Cmd+W 永遠只關目前分頁，碰不到背景分頁，所以這一格不用快捷鍵。
- **截圖**：`before.png`、`a-pasting-in-background.png`、`a-after-done.png`、`a-hover.png`、`b-after-x.png`、`b-after-done.png`。

#### WT62 Cmd+Q：背景分頁寫入中，拒絕結束並切到那個分頁

- **前置狀態**：「寫入中」，目前先點 `ws-tab:0`（`ws-src`）。
- **操作**：`mark`。`Cmd+Q`。等 3 秒。`shot after-quit`。`kill -0 "$PID"; echo $?`。放開 hold，等 `PASTE_DONE`。
- **預期畫面**：App 沒有結束。視窗切到遠端分頁（`ix=2`），狀態列是「正在套用變更，已拒絕關閉、開啟與結束，以免寫入中斷。」，分頁上有貼上圖示。
- **比對**：`L:`（依序）`[APP:QUIT: deferred`、`[APP:WS_TAB_ACTIVE: id=3 ix=2`、`[APP:PASTE_BUSY: refused=quit]`、`[APP:LIFECYCLE: phase=refused intent=quit reason=applying`。`NOT:` 任何 `phase=drained intent=quit`（沒有任何分頁開始排空）、`WS_TAB_CLOSED`。`kill -0` 結束碼 0。放開 hold 後 `[APP:PASTE_DONE: created=2`。`QUIT: deferred` 與 `PASTE_BUSY: refused=quit` 各恰好一行（分頁自己的 Quit 處理只轉給 root，不印日誌）。
- **截圖**：`before.png`、`after-quit.png`。

#### WT63 Cmd+Q 與視窗關閉鈕：目前分頁寫入中

- **前置狀態**：「寫入中」，目前是遠端分頁（`ix=2`）。
- **操作**：(1) `mark`，`Cmd+Q`，等 2 秒，`shot refused-quit`。(2) `mark`，按視窗左上角的關閉鈕（紅色，`TODO(verify)`：以工具點螢幕座標，位置約在視窗框左上角往右 14、往下 14 邏輯點；或 `osascript -e "tell application \"System Events\" to tell (first application process whose unix id is $PID) to click button 1 of first window"`），等 2 秒，`shot refused-window-close`。(3) 放開 hold，等 `PASTE_DONE`；`mark`；再 `Cmd+W`（現在可以關）。`shot after-close`。
- **預期畫面**：(1)(2) App 沒有結束，視窗還在，狀態列同 WT60。(3) 貼上完成後 `Cmd+W` 關掉遠端分頁，目前變成左鄰 `alpha`。
- **比對**：(1)(2) 各 `L:` `[APP:QUIT: deferred`、`[APP:PASTE_BUSY: refused=quit]`（各恰好一行，同 WT62）；`NOT:` `WS_TAB_ACTIVE`（它已經是目前分頁）、`phase=drained intent=quit`、`WS_TAB_CLOSED`；`kill -0` 結束碼 0。(3) `L:` `[APP:WORKSPACE: state=closed generation=`、`[APP:WS_TAB_CLOSED: id=3 count=2`、`[APP:WS_TAB_ACTIVE: id=2 ix=1`。
- **截圖**：`before.png`、`refused-quit.png`、`refused-window-close.png`、`after-close.png`。

#### WT65 本機貼上寫入中（盡力而為，不納入閘門）

- **前置狀態**：S1 之後 `OPEN($T/alpha)`，目前 `alpha`。`TODO(verify)`：本機沒有貼上暫停點，寫入視窗只有毫秒。為了拉長，建 `$RUN/big-src` 並放 3000 個小檔（`mkdir -p "$RUN/big-src"; for i in $(seq 1 3000); do printf 'x' > "$RUN/big-src/f$i.txt"; done`），複製成 payload（`(cd "$RUN/big-src" && "$SNIP" copy . --stdout) | pbcopy`，總量遠低於 32 MiB），貼進 `$T/alpha`。
- **操作**：預覽之後點 `btn-apply`，立刻按 `Cmd+W`，立刻再按 `Cmd+Q`。
- **預期畫面**：`TODO(verify)`：寫入太快時兩個按鍵都在貼上完成之後才到，分頁會被關掉或程式退出。
- **比對**：只有觀察到 `[APP:PASTE_APPLYING]` 之後、`[APP:PASTE_DONE:` 之前出現 `[APP:PASTE_BUSY: refused=close-workspace]` 或 `refused=quit` 時，這一格才有意義，判 `pass`。沒有抓到時間窗：寫 `not-run`，證據欄寫「本機寫入太快，沒有暫停點」。**不要**判 `fail`。
- **截圖**：`before.png`、`after.png`。

### 4.10 結束（WT70–WT72）

#### WT70 Cmd+Q：多個分頁全部排空後才結束

- **前置狀態**：S4（四個分頁），目前 `gamma`（`ix=3`）。沒有貼上進行中。
- **操作**：`mark`。`Cmd+Q`。等程序結束（最多 15 秒）。所以視窗關閉後無法再截圖，所以按之前先 `shot before-quit`，按之後只讀日誌與 exit code。
- **預期畫面**：視窗關閉，程序結束。
- **比對**：`L:` `[APP:QUIT: deferred`；四個分頁各有一行 `[APP:LIFECYCLE: phase=drained intent=quit jobs=0`，其中**三行**（背景分頁 `id=1`、`id=2`、`id=3`）結尾 `]` 前帶 ` ws_tab=<id>`，目前分頁 `gamma`（`id=4`）那一行沒有（`newlog | grep 'phase=drained intent=quit' | wc -l` 是 4，`grep -c ' ws_tab='` 在這幾行裡是 3）。`NOT:` `PASTE_BUSY`。程序結束：`kill -0 "$PID"` 失敗；`$RUN/app-gate-b-exit.json` 的 `exit_code` 是 0；10 秒內結束。`pgrep -fl snip-desktop-native` 沒有輸出，這一輪出現過的 git 子程序都不在。`QUIT: deferred` 恰好一行，且不帶 ` ws_tab=`。
- **截圖**：`before-quit.png`。

#### WT71 視窗關閉鈕與 Cmd+Q 相同

- **前置狀態**：S3，目前 `beta`，沒有貼上進行中。
- **操作**：`mark`。按視窗左上角的關閉鈕（同 WT63 (2)）。等程序結束。
- **預期畫面**：視窗關閉，程序結束（不是只關掉目前分頁）。
- **比對**：同 WT70：`[APP:QUIT: deferred`（恰好一行），三個分頁各有 `phase=drained intent=quit jobs=0`（兩行帶 ` ws_tab=`），exit code 0，沒有殘留。`NOT:` 只有一個分頁被關而程序還在（`kill -0` 失敗）。
- **截圖**：`before-close.png`。

#### WT72 沒有分頁時 Cmd+Q

- **前置狀態**：S1 之後 `Cmd+W`（只剩「+」，同 WT22 的狀態）。
- **操作**：`mark`。`Cmd+Q`。等程序結束。
- **預期畫面**：程序結束。
- **比對**：`L:` `[APP:QUIT: deferred`（恰好一行；沒有分頁時 root 持有焦點，Cmd+Q 會被接收）。接著恰好一行 root 印的 `[APP:LIFECYCLE: phase=drained intent=quit jobs=0 inflight=0 queued=0 leaked=0 generation=0]`（沒有分頁可排空，root 代為回報；不帶 ` ws_tab=`）。exit code 0；`$RUN` 之後不需要再 `finish` 時，最後一次這樣結束後直接跑 `finish`。
- **截圖**：`before-quit.png`（只有「+」的視窗）。

### 4.11 主題（WT80–WT81）

分頁列的顏色來自 `theme.rs` 的調色盤：

| 項目 | dark | light |
|---|---|---|
| 分頁列底色（`header_bg`） | `#26282c` | `#e9eaee` |
| 分頁列下緣分隔線（`divider`） | `#33353b` | `#dddfe4` |
| 目前分頁底色（`range_bg`） | `#233558` | `#e3ebfe` |
| 分頁 hover 底色（`hover_bg`） | `#2e2f30` | `#ededed` |
| 非目前分頁文字（`text_muted`） | `#9fa2a8` | `#5f6269` |
| 已連線圖示（`accent`） | `#3871e1` | `#3871e1` |
| 失敗圖示（`error`） | `#f57e84` | `#c54e58` |

取樣用 Pillow：`prepare` 需要 `SNIP_NATIVE_PYTHON` 有 Pillow 才能跑 native 驗收，這裡沿用同一個 Python（`$SNIP_NATIVE_PYTHON` 或 `python3`）。

```bash
pix() {   # pix <png> <螢幕點 x> <螢幕點 y> <scale>：印該點的 RGB 十六進位（螢幕點，截圖是整個主螢幕）
  "${SNIP_NATIVE_PYTHON:-python3}" -I - "$1" "$2" "$3" "$4" <<'PY'
from PIL import Image
import sys
im = Image.open(sys.argv[1]).convert("RGB")
s = float(sys.argv[4])
print("%02x%02x%02x" % im.getpixel((int(float(sys.argv[2]) * s), int(float(sys.argv[3]) * s))))
PY
}
```

取樣點：分頁底色取分頁左緣往右 3 邏輯 px、垂直置中；分頁列底色取視窗左緣往右 2 邏輯 px、同一個 y；hover 底色在 `hv` 之後取同一個分頁的同一個點。`TODO(verify)`：視窗必須在主螢幕上且 `screencapture` 的原點與螢幕點一致，否則先用 `hover-tab` 的游標位置校準。

#### WT80 dark：分頁列對比、目前與非目前、hover

- **前置狀態**：S3，目前 `alpha`（`ix=1`），`SNIP_THEME=dark`（預設），1080×720。
- **操作**：(1) `shot dark-base`。(2) 對 `ws-tab:1`（目前）、`ws-tab:0`（非目前）各取一點的底色，以及分頁列底色。(3) `hv` 到 `ws-tab:0` 停 2 秒，`shot dark-hover-tab`，取它的底色；`hv` 到 `ws-tab-close:0` 停 2 秒，`shot dark-hover-close`；`hv` 到 `ws-tab-new` 停 2 秒，`shot dark-hover-plus`。
- **預期畫面**：分頁列底色與工作台的 header 同色，下緣有一條分隔線。目前分頁有明顯的藍色底（`#233558`），文字比非目前分頁亮；非目前分頁文字偏灰（`#9fa2a8`）。hover 非目前分頁時底色變成 `#2e2f30`，× 與「+」hover 時有底色。標籤、圖示、× 都清楚可讀，沒有被截成看不見。
- **比對**：取樣值與上表相符（每個通道誤差 ≤ 2）：目前分頁 `#233558`、分頁列 `#26282c`、hover `#2e2f30`；寫進 `action.json`。目前與分頁列的底色不同，非目前分頁在非 hover 時與分頁列底色相同（沒有 hover 底）。另外用肉眼判斷「看得出哪個是目前分頁」，看不出來判 `ui-defect`，證據寫截圖。`L:` 無日誌要求。
- **截圖**：`before.png`、`dark-base.png`、`dark-hover-tab.png`、`dark-hover-close.png`、`dark-hover-plus.png`。

#### WT81 light：同樣檢查

- **前置狀態**：Cmd+Q 結束上一個 App（exit code 0）、日誌改名、把 `$RUN/environment.json` 的 `launch_env.SNIP_THEME` 改成 `light`：
  ```bash
  python3 -I -c 'import json,sys; p=sys.argv[1]; d=json.load(open(p)); d["launch_env"]["SNIP_THEME"]="light"; json.dump(d,open(p,"w"),indent=2)' "$RUN/environment.json"
  ```
  然後照 1.2 重新 `launch`、`resize 1080 720`，S3，目前 `alpha`。本格結束後把 `SNIP_THEME` 改回 `dark`（若後面還有格子）。
- **操作**：同 WT80，截圖名稱換成 `light-*`。
- **預期畫面**：淺色調色盤下，分頁列底色 `#e9eaee`、目前分頁 `#e3ebfe`、hover `#ededed`、非目前文字 `#5f6269`。目前分頁與分頁列的底色差異很小（紅綠通道只差 6 與 1，藍色差 16）：以截圖用肉眼判斷看得出哪個是目前分頁，文字顏色的差異（目前分頁用主要文字色、其他用 `#5f6269`）一併計入；hover 底色 `#ededed` 與分頁列 `#e9eaee` 幾乎一樣，hover 時是否看得出變化：如實記錄，`TODO(verify)`（對比不足時記為 `ui-defect`，附取樣值）。
- **比對**：取樣值與上表相符（每個通道誤差 ≤ 2），寫進 `action.json`。其他同 WT80。
- **截圖**：`before.png`、`light-base.png`、`light-hover-tab.png`、`light-hover-close.png`、`light-hover-plus.png`。

### 4.12 視窗大小（WT90–WT91）

#### WT90 900×600：分頁列版面，「+」永遠看得到

- **前置狀態**：S4（四個分頁），目前 `gamma`，dark。
- **操作**：`python3 "$REPO/scripts/real_ui_round.py" resize 900 600 --run "$RUN"`（VIEWPORT 與視窗內容區都是 900×600）。(1) `shot narrow-4`。(2) 把標籤加長：關掉一個，開 `$T/$LONGN` 與 `$T/many/workspace-number-01` 到 `-06`（共 10 個分頁：4 個減 1 個再加 7 個）。(3) `shot narrow-10`。(4) `Cmd+1`、`Cmd+9`，各 `shot`。
- **預期畫面**：(1) 4 個分頁放得下時沒有溢出。(3) 分頁列橫向溢出，沒有換行，分頁列高度仍是 34，「+」仍在最右邊、完整看得到、沒有被分頁蓋住。(4) 目前分頁都在視野內。
- **比對**：`resize` 印出 `[OK] Viewport confirmed at <900×scale>x<600×scale>`。最新 `ws-tab-new`：`w ≥ 1`、`h ≥ 1`、`x + w ≤ Wp`（`Wp = 900 × scale`）、`y + h ≤ 34 × scale`。每個畫出的 `ws-tab:<ix>` 同樣 `x + w ≤ ws-tab-new.x`。無 `CTRL_COVERED: id=ws-tab-new`、無 `CTRL_COVERED: id=ws-tab:`、無 `CTRL_DUPLICATE`（視窗寬度至少 700 時 App 會稽核）。`Cmd+1` 之後 `[APP:WS_TAB_ACTIVE: id=1 ix=0`，`Cmd+9` 之後 `ix=8`（第 10 個分頁在 `Cmd+9` 之外，用 `Cmd+Shift+[` 環繞到最後一個看），各自完整在視野內。
- **截圖**：`before.png`、`narrow-4.png`、`narrow-10.png`、`narrow-cmd1.png`、`narrow-cmd9.png`。

#### WT91 700×500 與更窄：分頁列仍可用

- **前置狀態**：WT90 的狀態（十個分頁）。
- **操作**：`resize 700 500`，`shot w700`；`resize 560 500`（低於 700，App 不再稽核 `CTRL_COVERED`），`shot w560`。在兩種寬度各點一次 `ws-tab-new`（再關掉那個空分頁：`Cmd+W`）與 `Cmd+Shift+]`。
- **預期畫面**：700 寬：分頁列溢出但「+」在最右、完整可點。560 寬：「+」仍然看得到、可點；分頁列高度不變。標籤被省略號截斷，不會把「+」擠出視窗。
- **比對**：`resize` 都印 `[OK] Viewport confirmed`。700 寬：同 WT90 的 bounds 規則加上無 `CTRL_COVERED`／`CTRL_DUPLICATE`。560 寬：`ws-tab-new` 的 `x + w ≤ Wp`、`w ≥ 1`、`h ≥ 1`。點「+」有 `WS_TAB_OPENED`、`WS_TAB_ACTIVE`；`Cmd+W` 之後 `WS_TAB_CLOSED`（空分頁，沒有 `WORKSPACE`）。`TODO(verify)`：窄到 560 時 `resize` 能否做到（macOS 視窗最小寬度）；`resize` 失敗就在證據欄寫實際最小寬度，並以那個寬度判。
- **截圖**：`before.png`、`w700.png`、`w560.png`。

### 4.13 標題

#### WT95 視窗標題跟著目前分頁

- **前置狀態**：S4（`[ws-src, alpha, beta, gamma]`），目前 `gamma`。
- **操作**：依序做下面每一步，每步之後 `wtitle` 並記錄：`Cmd+1`、`Cmd+2`、`Cmd+Shift+]`、點 `ws-tab:3`、點「+」（空分頁）、`OPEN($T/one/app)`（填入空分頁）、`Cmd+W`（關掉它）、關到只剩一個分頁、再 `Cmd+W`（沒有分頁）。
- **預期畫面**：標題依序是 `ws-src — snip-sync`、`alpha — snip-sync`、`beta — snip-sync`、`gamma — snip-sync`、`snip-sync`（空分頁）、`app — snip-sync`、（關掉之後的目前分頁標題）、…、最後 `snip-sync`（沒有分頁）。
- **比對**：每一步 `wtitle` 與預期逐字相同（em dash U+2014，前後各一個半形空格；以 `wtitle | od -c` 或 `python3 -c` 比對 `"—"`）。每一步也看 `shot` 的標題列。
- **截圖**：`before.png`、`step-1.png` 起到 `step-9.png`。

## 5. 完整性

| ID | 驗證 | 通過線 |
|---|---|---|
| I-config | 同本機規程 I-config：第一次啟動前與最後一個 App 結束後，真實設定資料夾的檔案數、SHA-256、mtime、大小全部相同 | `diff` 兩個 `.sha` 與兩個 `.mtime` 沒有輸出；`$RUN/config/recent-workspaces.json` 存在 |
| I-clip | 最後一次 `finish` 之後剪貼簿還原成開跑前的內容 | `finish` 的輸出說明剪貼簿雜湊相同 |
| I-leftover | 每一次啟動前 `pgrep -fl snip-desktop-native` 沒有輸出；遠端格結束後 Ubuntu 沒有本輪殘留（遠端規程 I03、第 6 節收尾，且 `~/.local/bin/snip` 的 wrapper 雜湊比對用 WT41 之後的那一份） | 全部成立 |
| I-fixture | `$T` 下每個 repo 的 `git status --porcelain` 在整輪結束時只有這份規程自己做過的改動（WT54 的 `readme.txt`、WT50 貼進的檔案）；`$B` 的 fixture 沒有被改 | 與預期相同 |
| I-rtk | 遠端格有碰 Ubuntu 時，`rtk_snapshot` 的 before 與 after 相同（遠端規程 I01） | 相同 |

收尾：全部格子做完後，`python3 "$REPO/scripts/real_ui_round.py" finish --run "$RUN"`；遠端格另按遠端規程第 6 節收尾（先 `stop_run_workers`、還原 wrapper、`rm -rf "$W"`）。`$RUN` 保留，裡面是證據。

## 6. 結論與計分表

`scorecard.md` 開頭三行：

```text
受測 SHA:
執行檔 SHA-256:
分頁閘門: 打開 | 關閉
```

判定只有 `pass`、`ui-defect`、`fail`、`not-run`（意義同本機規程第 1 節）。

- **閘門**：WT00、WT01 到 WT08、WT10 到 WT16、WT20 到 WT23、WT30 到 WT33、WT40、WT43、WT44、WT46、WT50 到 WT54、WT60 到 WT63、WT70 到 WT72、WT80、WT81、WT90、WT91、WT95、I-config、I-clip、I-leftover 全部是 `pass`，才寫「分頁閘門關閉」。
- **不納入閘門**：WT41（連線中，抓不到暫態時 `not-run`）、WT42（失敗圖示沒有可靠觸發時 `not-run`）、WT45（沒有第二個別名時 `not-run`）、WT65（本機沒有暫停點）。這四格要填，但 `not-run` 不使閘門打開；判成 `fail` 或 `ui-defect` 時閘門打開。
- 遠端格（WT40、WT43、WT44、WT46、WT60 到 WT63）缺 Ubuntu 準備時整組 `not-run`，證據寫「缺 Ubuntu 準備」，閘門打開。
- 有任何 `ui-defect`、`fail` 或 `not-run`（上面的例外除外），第一句就寫「分頁閘門打開」，並列出那些 ID。
- 每個不是 `pass` 的格子先分清楚是規程錯、工具做不到，還是產品缺陷（同遠端規程第 7 節）：規程錯就修這份規程；工具做不到就維持 `not-run` 並寫下可行的做法；產品缺陷先寫會失敗的測試再修產品。

複製到 `$RUN/scorecard.md` 後填寫：

```text
受測 SHA:
執行檔 SHA-256:
分頁閘門:

| ID | 判定 | 證據 |
|---|---|---|
| WT00 | | |
| WT01 | | |
| WT02 | | |
| WT03 | | |
| WT04 | | |
| WT05 | | |
| WT06 | | |
| WT07 | | |
| WT08 | | |
| WT10 | | |
| WT11 | | |
| WT12 | | |
| WT13 | | |
| WT14 | | |
| WT15 | | |
| WT16 | | |
| WT20 | | |
| WT21 | | |
| WT22 | | |
| WT23 | | |
| WT30 | | |
| WT31 | | |
| WT32 | | |
| WT33 | | |
| WT40 | | |
| WT41 | | |
| WT42 | | |
| WT43 | | |
| WT44 | | |
| WT45 | | |
| WT46 | | |
| WT50 | | |
| WT51 | | |
| WT52 | | |
| WT53 | | |
| WT54 | | |
| WT60 | | |
| WT61 | | |
| WT62 | | |
| WT63 | | |
| WT65 | | |
| WT70 | | |
| WT71 | | |
| WT72 | | |
| WT80 | | |
| WT81 | | |
| WT90 | | |
| WT91 | | |
| WT95 | | |
| I-config | | |
| I-clip | | |
| I-leftover | | |
| I-fixture | | |
| I-rtk | | |
```

## 7. 尚未驗證的事項（`TODO(verify)` 總表）

做完一輪後，把每一項的實際結果補進 `scorecard.md` 末尾，並回頭修這份規程。

1. 橫向捲動工具是否做得到（WT31）。
2. `Cmd+{` 與 `Cmd+}` 與 `Cmd+Shift+[`、`Cmd+Shift+]` 在工具上是否可區分（WT14）。
3. 連線中的 wrapper 延遲是否落在握手之前（WT41）；失敗狀態的可靠觸發方式（WT42）。
4. 本機貼上沒有暫停點（WT65）。
5. 視窗關閉鈕的點法（System Events 或座標）（WT63、WT71）。
6. light 主題下目前分頁與 hover 的對比是否足夠（WT81）。
7. 視窗最小寬度（WT91）。
