# 遠端節點真實 UI 驗收規程（Mac mini master ↔ Ubuntu worker）

狀態：操作規程。給會操作滑鼠的 agent（Codex）照著點真實 macOS 視窗。產品行為以 [spec.md 第 8 節](spec.md) 為準。點擊方法、座標換算、判定用詞沿用 [real-ui-operator-protocol.md](real-ui-operator-protocol.md)，本文只寫遠端節點多出來的部分。

這份規程只回答一件事：從 Mac mini 的桌面 App 點過去，瀏覽 Ubuntu 上的專案，怎樣才算正確。

## 0. 範圍

- **受測**：master 是 Mac mini 上的桌面 App（GUI），另有一段用 `snip remote`（CLI master）交叉驗證。worker 是 Ubuntu 上的 CLI `snip worker`，不開 GUI。
- **SSH 不是受測功能**。產品沒有「經 SSH 管理 worker」的功能。master 和 worker 之間走 Tailscale 上的 TLS 1.3，靠配對碼與指紋 pin 互信。本規程只在準備階段用 ssh：在 Ubuntu 上編譯、建 fixture、啟動和重啟 worker，以及在 worker 端讀檔案當 oracle。
- **本切片只做瀏覽與預覽**。寫入、rename、stage、git、遠端 diff 不在範圍。複製、貼上、Git 檢視在遠端工作區都要拒絕，這也是受測項目。
- 不改產品程式，不 commit 這一輪的產出。

## 1. 機器與受測版本

| 角色 | 機器 | Tailscale IP | 怎麼到 |
|---|---|---|---|
| master | Mac mini `AudideMac-mini` | 100.118.97.71 | 本機 |
| worker | Ubuntu `audichuang-desktop`（x86_64） | 100.95.28.19 | `ssh ubuntu`（LAN 192.168.31.65） |

- 受測 SHA 沒有另外指定時，用 `origin/develop`。它必須包含 `ffcbb04`（#79 遠端節點第一刀）和 `64b6fde`（#81 連線排隊），用 `git merge-base --is-ancestor` 檢查，缺一個就不開跑。
- Ubuntu 上 `/home/linuxbrew/.linuxbrew/bin/snip` 是 0.4.0，**沒有 `worker` 指令，不能用**。worker 一律用本輪從受測 SHA 編出來的 binary。
- 受測專案：Ubuntu 上的 `~/research/rtk`。這是真實的 Rust 專案，HEAD `87a6c69`，tracked 檔 414 個，工作樹乾淨。它有中文 README（`README_zh.md`）、200 KB 以上的原始碼（`src/hooks/init.rs` 238 KB）、`.git/`、`target/`，還有 8 MB 的二進位檔 `target/release/rtk`。**它只讀不寫**：第 7 節 I01 會驗證這一輪沒有動到它。
- 邊界案例另外放在 fixture 資料夾 `edge`（第 2.3 節），不放進 rtk。

## 2. 開跑

下面的命令區塊都用 bash 執行（這台 Mac 的預設 shell 是 fish；先執行 `bash`，或用 `bash -c`）。cargo 在 `$HOME/.cargo/bin`，代理程式的 shell 不一定有這條 PATH，請自己加上。

### 2.1 第 0 步閘門：CLI 端到端

在受測 SHA 的乾淨 checkout 上執行：

```bash
export PATH=$HOME/.cargo/bin:$PATH
git merge-base --is-ancestor ffcbb04 HEAD && git merge-base --is-ancestor 64b6fde HEAD
cargo build --release -p snip-cli --locked
just remote-e2e-ssh ubuntu
```

最後一行必須是 `== N passed, 0 failed`，結束碼 0。這一步一次確認編譯、Tailscale 連得到、Ubuntu 防火牆沒擋、TLS 與配對都正常。**沒過就不開始點 GUI**，計分表全部寫 `not-run`，證據欄寫「第 0 步閘門失敗」並附上輸出。這一步只有失敗時才算產品問題，因為它不涉及 GUI。

### 2.2 本輪目錄與 Mac 端建置

```bash
export SHA=$(git rev-parse --short HEAD)
export RUN="$HOME/snip-sync-ui-runs/$(date +%Y-%m-%d)-$SHA-remote"
export W="/home/audichuang/snip-ui-run/$SHA"     # Ubuntu 上的本輪目錄
mkdir -p "$RUN"
cargo build -p snip-desktop-native -p snip-cli --locked
shasum -a 256 target/debug/snip-desktop-native target/debug/snip
```

設定資料夾兩端都要隔離，不碰真實的配對紀錄：

```bash
export SNIP_CONFIG_DIR="$RUN/master-config"      # GUI 與 CLI master 共用
mkdir -p "$SNIP_CONFIG_DIR"
REAL="$HOME/Library/Application Support/com.audichuang.snip-sync"
ls -la "$REAL" > "$RUN/real-config-before.txt" 2>&1
shasum -a 256 "$REAL"/* > "$RUN/real-config-before.sha" 2>/dev/null || true
```

本機要有一個小的工作區，讓 App 啟動時就有 `btn-workspace-menu`，也用來驗證離開遠端之後複製會恢復（R33）：

```bash
mkdir -p "$RUN/local-ws" && cd "$RUN/local-ws" && git init -q && echo local > local.txt \
  && git add . && git -c user.name=t -c user.email=t@t commit -qm init && cd -
```

### 2.3 Ubuntu 端：編譯與 fixture

```bash
git archive HEAD | ssh ubuntu "rm -rf '$W' && mkdir -p '$W/src' && tar -x -C '$W/src'"
ssh ubuntu "cd '$W/src' && export PATH=\$HOME/.cargo/bin:\$PATH && cargo build --release -p snip-cli --locked 2>&1 | tail -1"
ssh ubuntu "W='$W' bash -s" <<'EOF'
set -euo pipefail
mkdir -p "$W/edge/src/deep" "$W/edge/nested/.git" "$W/edge/manydir" "$W/edge/a/b/c/d/e"
cd "$W"
echo top-secret-c0ffee > secret.txt            # 在所有分享之外
cd edge
printf 'line1\r\nline2\r\n' > crlf.txt
: > empty.txt
echo '深層 檔案 ✓ 🦀' > 'src/deep/中文 有空白.txt'
echo 'fn main() { println!("hello from ubuntu"); }' > src/main.rs
echo 'deepest' > a/b/c/d/e/leaf.txt
printf '\000\001\002binary' > blob.bin
printf '\377\376latin' > latin.txt
head -c 1048576 /dev/zero | tr '\000' a > exact-1MiB.txt
head -c 1048577 /dev/zero | tr '\000' a > over-1MiB.txt
(cd manydir && for i in $(seq 1 1200); do : > "f$i"; done)
ln -s "$W/secret.txt" escape.txt               # 指向分享外的檔案
ln -s /etc escape-dir                          # 指向分享外的資料夾
ln -s "$W/edge/src" inner-link                 # 指向分享內
echo 'invalid name' > "$(printf 'bad\377name.txt')"   # 非 UTF-8 檔名
echo 'hidden' > .hidden
EOF
```

這段已在 `b41817f` 上試跑過：Ubuntu 編譯約 20 秒（有快取時），fixture 全部建立成功，`manydir` 有 1200 個檔案。試跑時從 Mac 用 `snip remote` 讀過：`escape.txt` 和 `escape-dir` 都回「Path leaves the workspace」，`target/release/rtk` 回「Preview exceeds 1 MiB」，`exact-1MiB.txt` 完整讀到 1048576 bytes，`.git/HEAD` 讀得到（專案樹不列 `.git`，但指名路徑仍可讀）。

rtk 的基準（I01 用）：

```bash
ssh ubuntu 'cd ~/research/rtk && git rev-parse HEAD && GIT_OPTIONAL_LOCKS=0 git status --porcelain=v1 -z | sha256sum && find . -newer .git/HEAD -not -path "./.git/*" | wc -l' > "$RUN/rtk-before.txt"
ssh ubuntu "touch '$W/rtk-marker'"
```

### 2.4 啟動 worker（每次都用這個函式）

```bash
start_worker() {   # $1 = 設定資料夾名稱；其餘參數 = 要分享的資料夾
  local cfg=$1; shift
  local shares=""; for d in "$@"; do shares="$shares --share '$d'"; done
  ssh -o BatchMode=yes ubuntu "bash -s" <<SH
cd '$W'
pkill -f '$W/src/target/release/snip worker' 2>/dev/null
for i in \$(seq 1 20); do pgrep -f '$W/src/target/release/snip worker' >/dev/null || break; sleep 0.5; done
SNIP_CONFIG_DIR='$W/$cfg' SNIP_DEVICE_NAME=ubuntu-ui nohup '$W/src/target/release/snip' worker $shares --listen 100.95.28.19:47821 > worker.log 2>&1 < /dev/null &
for i in \$(seq 1 40); do grep -q 'pairing code' worker.log && break; grep -q rror worker.log && break; sleep 0.5; done
cat worker.log
SH
}
start_worker wcfg "$W/edge" /home/audichuang/research/rtk | tee "$RUN/worker-start-1.log"
```

這個函式已經試跑過。有三個陷阱，不要改掉：

- 一定要有 `< /dev/null`，否則 ssh 會一直等背景的 worker，不會返回。
- 要等舊的 worker 真的結束才啟動新的，否則會出現 `Address already in use`。
- `pkill -f` 要放在 `bash -s` 的 stdin 裡執行。如果直接寫成 `ssh ubuntu 'pkill -f "snip worker"'`，pattern 會比對到執行它的那個 bash 自己，連 ssh 連線一起被殺掉（exit 255）。

輸出要有 `snip-sync worker listening on 100.95.28.19:47821`、`fingerprint XXXX-XXXX-XXXX-XXXX`、兩行 `sharing …`，以及 `pairing code ABCD-EFGH (valid 10 minutes; restart for a new one)`。

**配對碼 10 分鐘內有效**。啟動 worker、讀配對碼、在 GUI 配對（R01–R05）要連續做完，中間不做別的。過期或用掉了，就再執行一次 `start_worker wcfg …` 拿新碼；同一個 `wcfg` 會保留指紋。

### 2.5 啟動 App

同一時間只開一個 App。每次啟動都寫進新的日誌檔（`app-1.log`、`app-2.log`…），不要用 `>` 蓋掉前一次。

```bash
export SNIP_NATIVE_E2E=1 SNIP_THEME=dark
./target/debug/snip-desktop-native --workspace "$RUN/local-ws" > "$RUN/app-1.log" 2>&1
```

`SNIP_CONFIG_DIR` 已在 2.2 export，App 的配對紀錄寫在 `$RUN/master-config/remote-workers.json`。

視窗用 1080×720。R40 另外在 900×600 再做一次配對表單。

### 2.6 `environment.json`

至少包含：受測 SHA、兩個 Mac binary 與 Ubuntu `snip` 的 SHA-256、第 0 步閘門的結果行、`$RUN`、`$W`、worker 的 listen 位址與指紋、每次 App 啟動的命令和日誌路徑、視窗邏輯尺寸與 `[APP:VIEWPORT]`、`backingScaleFactor`。

## 3. 怎麼點、怎麼判

點擊照 [real-ui-operator-protocol.md 第 3 節](real-ui-operator-protocol.md) 的七步做：從這一次的日誌取最新的 `CTRL_BOUNDS`，確認沒有更晚的 `CTRL_GONE`，換算 Retina 座標，點下去之後要有新的日誌行。判定也一樣只有 `pass`、`ui-defect`、`fail`、`not-run` 四種。

遠端節點多出來的規則：

1. **用索引編號的 ID**。`remote-worker:<ix>`、`btn-remote-forget:<ix>`、`remote-workspace:<wx>` 用的是清單裡的序號，不是名稱。第 3 節「ID 要和路徑完全相等」的規則不適用於它們。要點某個工作區之前，先截圖確認那一列顯示的名稱。點了之後，用 `[APP:REMOTE_OPENED: <worker> ▸ <name> …]` 的 label 確認點到的是哪一個。label 不對，判 `fail`，證據欄寫 `identity-fail`。
2. **遠端工作區的樹用 `ws-` 前綴**：`ws-tree-row:<相對路徑>`、`ws-tree-chevron:<相對路徑>`、截斷標記 `ws-tree-marker:<資料夾>`、更多列 `ws-tree-view-more:<資料夾>`（整棵樹的截斷列是 `ws-tree-view-more:`）、錯誤列 `ws-tree-retry:<資料夾>`、非 UTF-8 名稱 `ws-tree-invalid:<suffix>`。日誌是 `WS_TREE_PAGE: rel= kind= children= has_more= selected=`、`WS_FILE_SELECTED`。`WS_TREE_TOGGLED` 只在勾選時印，遠端模式不能勾選，所以展開不會有它。
3. **只在狀態列顯示、沒有日誌 tag 的訊息**：`remote_unsupported`（遠端工作區目前只支援瀏覽與預覽）、`remote_pair_missing`（請輸入位址與配對碼）、工作區清單讀取失敗。這些格子的通過線是：截圖裡有那段文字、剪貼簿 sentinel 的 SHA 不變、1 秒內沒有新的 `COPY_*` 或 `PASTE_*` 行。**不要因為少一行日誌就判 `fail`。**
4. **預覽成功與失敗的分法**（`apply_source_preview`）：成功時日誌是 `PREVIEW_LOADING: <path>` 接著 `PREVIEW_LOADED: <path>`。worker 拒絕、二進位、過大時，只有 `PREVIEW_LOADING`，**沒有** `PREVIEW_LOADED`，預覽區顯示錯誤文字。失敗格的通過線：有 `PREVIEW_LOADING`、沒有 `PREVIEW_LOADED`、截圖裡是錯誤文字而不是檔案內容、下一次點擊 App 仍有反應。
5. **指紋比對**：`REMOTE_PAIRED: fp=` 是 16 個十六進位字元。worker 印出的是 `XXXX-XXXX-XXXX-XXXX`。去掉 `-`、不分大小寫比較，兩者必須相同。選單列顯示的是 `XXXX-XXXX-XXXX-XXXX`。
6. **文字輸入**：先點 `remote-addr-input` 或 `remote-code-input` 的 bounds，再用鍵盤輸入，然後截圖確認輸入框裡的字。貼上文字用 Cmd+V 時，要確認焦點在輸入框裡；否則 Cmd+V 是 App 的貼上。
7. **worker 端 oracle**：worker 上檔案的內容和清單，用 `ssh ubuntu` 讀，存成檔再算 SHA-256。不要拿 App 自己的輸出去驗證 App。

剪貼簿 sentinel：每個拒絕格開始前，`printf 'sentinel-%s' "$RANDOM" | pbcopy`，再用 `pbpaste | shasum -a 256` 記下雜湊。

### 3.1 遠端節點的控制項與日誌

| 位置 | ID |
|---|---|
| 工作區選單 | `btn-workspace-menu` |
| 本機當 worker | `btn-remote-worker-toggle`、`btn-remote-worker-pair`、`remote-worker-code` |
| 已配對的 worker | `remote-worker:<ix>`、`btn-remote-forget:<ix>` |
| 該 worker 分享的工作區 | `remote-workspace:<wx>` |
| 配對表單 | `btn-remote-pair-new`、`remote-addr-input`、`remote-code-input`、`btn-remote-pair` |
| 工具列 | `btn-copy`、`btn-paste`、`btn-refresh`；左軌 `rail-project`、`rail-changes`、`rail-log` |

| 日誌 | 意思 |
|---|---|
| `[APP:REMOTE_PAIRED: name=<名稱> fp=<16 hex>]` | 配對成功。之後 App 會自動列出它的工作區 |
| `[APP:REMOTE_PAIR_FAILED: <訊息>]` | 配對失敗 |
| `[APP:REMOTE_WORKSPACES: count=N]` | 列出工作區成功（失敗時沒有這行，只在選單顯示紅字） |
| `[APP:REMOTE_OPENED: <worker> ▸ <工作區> generation=N]` | 開啟遠端工作區 |
| `[APP:REMOTE_WORKER: state=…]`、`[APP:REMOTE_PAIRING: state=open]` | 本機當 worker（本規程不測） |

## 4. 測案

每格都要截圖（點擊前、點擊後各一張），存在 `$RUN/<ID>/`，再加一份 `action.json`，內容是 bounds、scale、算出的螢幕點和新的日誌行。

### 4.1 配對（R01–R05 連續做完，要在配對碼過期前）

| ID | 動作 | 通過線 |
|---|---|---|
| R01 | 點 `btn-workspace-menu` | 有 `btn-remote-pair-new`、`btn-remote-worker-toggle` 的 bounds；沒有任何 `remote-worker:*`（全新的設定資料夾）。截圖有「遠端節點」區塊 |
| R02 | 點 `btn-remote-pair-new`，兩個欄位都空著，點 `btn-remote-pair` | 出現 `remote-addr-input`、`remote-code-input`；狀態顯示「請輸入位址與配對碼」；沒有 `REMOTE_PAIRED`／`REMOTE_PAIR_FAILED` |
| R03 | 位址輸入 `100.95.28.19`，配對碼輸入錯的 `AAAA-AAAA`，點 `btn-remote-pair` | 一行新的 `REMOTE_PAIR_FAILED`；選單裡是紅字錯誤；`master-config/remote-workers.json` 不存在或沒有這台 |
| R04 | 配對碼改成 worker 印出的那組（位址不加埠，預設 47821），點 `btn-remote-pair` | 出現 `REMOTE_PAIRED: name=ubuntu-ui fp=…`，fp 和 worker 指紋一致（第 3 節規則 5）；接著 `REMOTE_WORKSPACES: count=2`；`remote-workers.json` 有一筆 `ubuntu-ui`。按鈕在配對時會短暫顯示「配對中…」，截到就附上，截不到不影響判定 |
| R05 | 截圖選單 | `remote-worker:0` 那一列顯示 `ubuntu-ui`、`100.95.28.19 · XXXX-XXXX-XXXX-XXXX`（指紋同 R04；存下來的位址不含預設埠）；底下兩列 `remote-workspace:0/1` 是 `edge`、`rtk`（試跑時是這個順序，順序不列入判定），各自附有 Ubuntu 上的完整路徑 |

### 4.2 瀏覽真實專案 rtk

| ID | 動作 | 通過線 |
|---|---|---|
| R06 | 點顯示 `rtk` 的那一列 `remote-workspace:<wx>` | `REMOTE_OPENED: ubuntu-ui ▸ rtk`；選單關閉；左上角顯示 `ubuntu-ui ▸ rtk`；狀態列顯示「已開啟遠端工作區 …」；`local.txt` 那一列有 `CTRL_GONE` |
| R07 | 讀根目錄 | 根目錄的 `ws-tree-row:*` 集合等於 `ssh ubuntu 'ls -A ~/research/rtk'` 的結果去掉 `.git`（專案樹刻意不列 `.git`，和本機模式一致），`target` 要在；資料夾排在前面，同類照名稱排序 |
| R08 | 點 `ws-tree-row:src` 展開 | `WS_TREE_PAGE: rel=src … children=N`，N 等於 `ls -A src \| wc -l`；子列的 ID 是 `ws-tree-row:src/<名稱>` |
| R09 | 點 `ws-tree-row:src/main.rs` | `WS_FILE_SELECTED`、`PREVIEW_LOADING`、`PREVIEW_LOADED: src/main.rs`；預覽前 20 行和 `ssh ubuntu 'head -20 ~/research/rtk/src/main.rs'` 一致（截圖比對）；有 Rust 語法上色；預覽上方的路徑列是 `ubuntu-ui ▸ rtk › src › main.rs`，不是 `snip-remote://…` |
| R10 | 點 `ws-tree-row:README_zh.md` | `PREVIEW_LOADED`；中文正常顯示，沒有豆腐字或亂碼 |
| R11 | 展開 `src/hooks`，點 `init.rs`（238 KB），在預覽裡捲到最後 | `PREVIEW_LOADED`；最後一行和 `tail -1` 一致；捲動時 App 不卡 |
| R12 | 展開 `target`、`release`，點 `target/release/rtk`（8 MB 二進位） | 依第 3 節規則 4 判失敗格：顯示二進位或超過 1 MiB 的錯誤都算對；沒有亂碼文字；5 秒內可以點下一列 |
| R13 | 看根目錄，再執行 `target/debug/snip remote cat 1 rtk .git/HEAD` | 樹裡沒有 `.git` 列；CLI 印出 `ref: refs/heads/…`（`.git` 只是不列在樹裡，指名路徑仍可讀） |
| R14 | 把 `src` 收合再展開 | 第二次也有 `WS_TREE_PAGE: rel=src`，清單和 R08 相同 |

### 4.3 邊界案例 edge

| ID | 動作 | 通過線 |
|---|---|---|
| R15 | 從工作區選單切到 `edge`（先截圖確認列名） | `REMOTE_OPENED: ubuntu-ui ▸ edge`；rtk 的列都有 `CTRL_GONE`；根目錄列和 `ls -A "$W/edge"` 一致，非 UTF-8 那個名稱除外（見 R27） |
| R16 | 依序展開 `src`、`deep`，點 `中文 有空白.txt` | `PREVIEW_LOADED: src/deep/中文 有空白.txt`；顯示 `深層 檔案 ✓ 🦀` |
| R17 | 點 `empty.txt` | `PREVIEW_LOADED`；預覽是空的，不是錯誤，也不是上一個檔案的內容 |
| R18 | 點 `crlf.txt` | `PREVIEW_LOADED`；兩行 `line1`、`line2`，沒有顯示 `^M` 或多出空行 |
| R19 | 點 `exact-1MiB.txt` | `PREVIEW_LOADED`（剛好 1 MiB 要能預覽）；就算 App 的長行顯示另外截斷，也只能出現本機同一規則的截斷提示 |
| R20 | 點 `over-1MiB.txt` | 失敗格：錯誤文字提到超過 1 MiB |
| R21 | 點 `blob.bin` | 失敗格：二進位或非 UTF-8，無法預覽 |
| R22 | 點 `latin.txt` | 失敗格：同 R21 |
| R23 | 點 `manydir` 展開，再反覆點最新的 `ws-tree-view-more:manydir` 或 `ws-tree-view-more:`，直到沒有更多列 | `snip remote ls` 回 1000 項；`WS_TREE_PAGE: rel=manydir … children=N`，N ≤ 1000（GUI 的記憶體預算可能收得比 1000 少，記下實際值）；每點一次，畫面都出現新的 `ws-tree-row:manydir/…`；最後有 `ws-tree-marker:manydir`，顯示「[目錄未完整列出: 已截斷]」；捲到底不卡 |
| R24 | 點 `escape.txt` | 失敗格：錯誤文字說路徑離開了工作區；`top-secret-c0ffee` 不出現在截圖，也不在 `app-*.log` 裡（`grep -c top-secret "$RUN"/app-*.log` 要是 0） |
| R25 | 點 `escape-dir` | 拒絕。指向分享外的資料夾 symlink 列成檔案列（CLI `snip remote ls 1 edge` 印 `escape-dir`，沒有尾端 `/`）；點下去是失敗格，錯誤是「Path leaves the workspace」。**不能**列出 `/etc` 的內容（截圖裡沒有 `passwd`、`hostname`） |
| R26 | 點 `inner-link` | 指向分享內的資料夾 symlink 列成資料夾（CLI `snip remote ls 1 edge` 印 `inner-link/`）：可以展開，列出 `deep`、`main.rs`；點 `inner-link/main.rs` 能預覽。顯示成檔案列或無法展開，判 `fail` |
| R27 | 找到非 UTF-8 名稱那一列 | 它的 ID 是 `ws-tree-invalid:<suffix>`，名稱用替代字元顯示；點它不能讓 App 崩潰，也不能預覽到別的檔案的內容（錯誤或「無法預覽」都算對）。把實際行為寫進證據欄 |
| R28 | 依序展開 `a/b/c/d/e`，點 `leaf.txt` | 每層各有一行 `WS_TREE_PAGE`；顯示 `deepest` |
| R29 | 看 `nested` 和 `.hidden` | `nested` 是資料夾列，可以展開；它只有 `.git`，所以展開後是空的（`WS_TREE_PAGE: rel=nested … children=0`）；`.hidden` 有列出，可以預覽 |

### 4.4 拒絕寫入類操作（任一遠端工作區）

每格開始前先放 sentinel（第 3 節）。

| ID | 動作 | 通過線 |
|---|---|---|
| R30 | 選一個檔案列，按 Cmd+C，再點 `btn-copy` | 兩次都顯示「遠端工作區目前只支援瀏覽與預覽」；sentinel 的 SHA 不變；沒有 `COPY_PREP`／`COPY_DONE` |
| R31 | 按 Cmd+V，再點 `btn-paste` | 同一段狀態文字；沒有 `PASTE_PREVIEW`／`PASTE_LOADING`；沒有出現貼上面板；`ssh ubuntu "find '$W/edge' -newer '$W/rtk-marker' \| wc -l"` 是 0 |
| R32 | 點 `rail-changes`、`rail-log` | 不顯示本機 repo 的資料，也不執行 git（spec：Git 檢視要拒絕）。顯示拒絕訊息或空畫面都算對；顯示 `local-ws` 的變更或 commit 判 `fail`。截圖並寫下實際畫面 |
| R33 | 用 Cmd+Shift+O（或工作區選單的最近工作區）打開本機的 `$RUN/local-ws`，選 `local.txt`，按 Cmd+C | 左上角不再有 `ubuntu-ui ▸`；`COPY_DONE`；`pbpaste` 拿到 snip-sync 的 payload（剪貼簿 SHA 和 sentinel 不同）。這格驗證離開遠端之後，遠端狀態有清乾淨 |

### 4.5 即時變化與 worker 生命週期

R34 之前，先重新開回 `edge`（R15 的步驟）。

| ID | 動作 | 通過線 |
|---|---|---|
| R34 | `ssh ubuntu "echo fresh-1 > '$W/edge/new.txt'"`，點 `btn-refresh` | 樹重新讀取；出現 `ws-tree-row:new.txt`，預覽顯示 `fresh-1` |
| R35 | `ssh ubuntu "echo fresh-2 > '$W/edge/new.txt'"`，點別的檔案再點回 `new.txt` | 顯示 `fresh-2`，不是快取的舊內容 |
| R36 | `ssh ubuntu "rm '$W/edge/new.txt'"`，點 `btn-refresh` | `ws-tree-row:new.txt` 出現 `CTRL_GONE` |
| R37 | 停掉 worker：`ssh ubuntu "kill \$(cat '$W/worker.pid')"`，在 App 點一個沒預覽過的檔案 | 失敗格；`PREVIEW_LOADING` 之後 10 秒內出現錯誤（master 連線逾時 2 秒、讀取 5 秒）；這段時間 App 沒有凍結（可以捲動、可以開選單）；接著按 Cmd+Shift+W 關掉工作區，要在 8 秒內完成 |
| R38 | `start_worker wcfg "$W/edge" /home/audichuang/research/rtk`；確認指紋和第一次一樣；在 App 選單點 `remote-worker:0`，開 `edge`，點一個檔案 | 不需要重新配對；`REMOTE_WORKSPACES: count=2`、`REMOTE_OPENED`、`PREVIEW_LOADED` |
| R39 | 取消分享：`start_worker wcfg /home/audichuang/research/rtk`（只分享 rtk）。App 不重開，直接點 `edge` 裡另一個檔案，再開選單點 `remote-worker:0` | 預覽被拒絕（失敗格）；選單只列出 `rtk`（`REMOTE_WORKSPACES: count=1`） |
| R40 | 換一張憑證：`start_worker wcfg-other "$W/edge"`（同一個位址，新的設定資料夾），在 App 選單點 `remote-worker:0` | 拒絕；選單顯示紅字，內容說明這不是已配對的 worker（內容含兩個指紋）；`remote-workers.json` 的指紋沒有被改成新的 |
| R41 | 復原：`start_worker wcfg "$W/edge" /home/audichuang/research/rtk`，點 `remote-worker:0` | `REMOTE_WORKSPACES: count=2` |
| R42 | Cmd+Q，等 exit code 0，用同一個 `SNIP_CONFIG_DIR` 重新啟動（寫進新的 `app-N.log`），開選單 | `remote-worker:0` 仍然是 `ubuntu-ui`，點它就能列出工作區，不需要重新配對 |
| R43 | 錯誤 5 次作廢：`start_worker wcfg "$W/edge" /home/audichuang/research/rtk` 拿新的配對碼 C。Cmd+Q，改用全新的 `SNIP_CONFIG_DIR="$RUN/master-config-r43"` 啟動 App（寫進新的 `app-N.log`）。用 `btn-remote-pair-new` 以錯碼配對 5 次，第 6 次用 C。再 Cmd+Q，用原本的 `SNIP_CONFIG_DIR` 重新啟動，點 `remote-worker:0` | 5 行 `REMOTE_PAIR_FAILED`；第 6 次也是 `REMOTE_PAIR_FAILED`（碼已作廢）；換回原本的設定後，`remote-worker:0` 仍然能列出工作區。一定要用全新的 master：已配對的 master，worker 認得它的憑證，不看配對碼就放行 |
| R44 | 把視窗調成 900×600，開選單並打開配對表單 | 兩個輸入框和「配對」按鈕的 bounds 都 `w,h ≥ 1`，而且都在內容區裡面；選單可以捲動到最下面 |
| R45 | 先開著 `edge`，再點 `btn-remote-forget:0` | 開著的 `edge` 跟著關閉：左上角不再有 `ubuntu-ui ▸`，`ws-tree-row:*` 都有 `CTRL_GONE`；`remote-worker:0` 有 `CTRL_GONE`；`remote-workers.json` 不再有 `ubuntu-ui`；重開選單也不會再出現 |

### 4.6 CLI master 交叉驗證

R45 之前，或在 R45 之後重新配對一次（重啟 worker 拿新碼，用 GUI 配對）。

| ID | 動作 | 通過線 |
|---|---|---|
| C01 | `target/debug/snip remote workers`（同一個 `SNIP_CONFIG_DIR`） | 不用另外配對就列出 `1	ubuntu-ui	100.95.28.19	<指紋>`，證明 GUI 和 CLI 共用配對紀錄 |
| C02 | `snip remote workspaces 1`、`snip remote ls 1 rtk src` | 結果和 R05、R08 在 GUI 上看到的一致 |
| C03 | 對 rtk 抽 20 個 tracked 文字檔，比較 `snip remote cat 1 rtk <f> \| shasum -a 256` 和 `ssh ubuntu sha256sum` | 20/20 相同 |
| C04 | App 開著 rtk 的時候，同時跑 50 個平行的 `snip remote cat 1 rtk src/main.rs` | 50/50 正確；這段時間在 GUI 點檔案仍然能預覽（`PREVIEW_LOADED`） |

## 5. 完整性（決定這一輪可不可信）

| ID | 檢查 | 通過線 |
|---|---|---|
| I01 | 結束時用同一行命令重新產生三項，寫進 `rtk-after.txt`。`git status` 一定要加 `GIT_OPTIONAL_LOCKS=0`：少了它，git 會建立又刪掉 `.git/index.lock`，`.git` 目錄的修改時間因此變新，下面的 `find` 就會算到它 | 和 `rtk-before.txt` 完全相同；另外 `ssh ubuntu "find ~/research/rtk -newer '$W/rtk-marker' -not -path '*/.git/*' \| wc -l"` 是 0 |
| I02 | 同 2.2，重新列出 `$REAL` 並算雜湊 | 和 `real-config-before.*` 相同（這一輪沒有碰真實的配對紀錄） |
| I03 | `grep -c top-secret-c0ffee "$RUN"/app-*.log "$RUN"/*/action.json` | 全部是 0 |

## 6. 收尾

```bash
ssh ubuntu "kill \$(cat '$W/worker.pid') 2>/dev/null; rm -rf '$W'"
```

只刪 `$W`。不要動 `~/research/rtk`，也不要動 Ubuntu 上 linuxbrew 的 `snip`。Mac 上的 `$RUN` 保留，裡面是證據。

## 7. 計分

`scorecard.md` 列出 R01–R45、C01–C04、I01–I03，每格一個判定，並附證據路徑。

- **閘門**：第 0 步閘門通過，而且 R01–R45、C01–C04、I01–I03 全部是 `pass`，才寫「遠端節點真實 UI 閘門關閉」。
- 有任何 `ui-defect`、`fail` 或 `not-run`，第一句就寫「遠端節點真實 UI 閘門打開」，並列出那些 ID。
- I01 或 I02 不是 `pass`，整輪結果作廢：這一輪動到了真實資料，先報告，再處理其他格子。
- R27、R32 的預期本來就允許多種畫面。判 `pass` 時，證據欄要寫實際看到的是哪一種，下一輪才能收緊。
