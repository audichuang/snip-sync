# 遠端節點真實 UI 驗收規程（Mac mini master ↔ Ubuntu worker）

狀態：操作規程。給會操作滑鼠的 agent（Codex）照著點真實 macOS 視窗。產品行為以 [spec.md 第 8 節](spec.md) 為準。點擊方法、座標換算、判定用詞沿用 [real-ui-operator-protocol.md](real-ui-operator-protocol.md)，本文只寫遠端節點多出來的部分。

這份規程只回答一件事：從 Mac mini 的桌面 App 點過去，瀏覽 Ubuntu 上的專案，怎樣才算正確。

## 0. 範圍

- **受測**：master 是 Mac mini 上的桌面 App（GUI），另有一段用 `snip remote`（CLI master）交叉驗證。worker 是 Ubuntu 上的 CLI `snip worker`，不開 GUI。
- **SSH 不是受測功能**。產品沒有「經 SSH 管理 worker」的功能。master 和 worker 之間走 Tailscale 上的 TLS 1.3，靠配對碼與指紋 pin 互信。本規程只在準備階段用 ssh：在 Ubuntu 上編譯、建 fixture、啟動和重啟 worker，以及在 worker 端讀檔案當 oracle。
- **本輪受測含唯讀 Git 檢視**。寫入、rename、stage、commit、discard、以及為複製而勾選仍不在範圍內；複製、貼上、加入儲存庫路徑、為複製而勾選在遠端工作區都要拒絕，這也是受測項目。
- **不能碰使用者自己的 worker 服務**：Ubuntu 的 systemd user unit `snip-worker.service` 和 Mac mini 的 LaunchAgent `com.audichuang.snip-worker`（都在 47821 埠）不屬於這一輪，不可停止、重啟或改設定。測試 worker 一律用 47899；`pkill` 的 pattern 只能比對本輪的 `$W/src/target/release/snip`。
- 不改產品程式，不 commit 這一輪的產出。

## 1. 機器與受測版本

| 角色 | 機器 | Tailscale IP | 怎麼到 |
|---|---|---|---|
| master | Mac mini `AudideMac-mini` | 100.118.97.71 | 本機 |
| worker | Ubuntu `audichuang-desktop`（x86_64） | 100.95.28.19 | `ssh ubuntu`（LAN 192.168.31.65） |

- 受測 SHA 沒有另外指定時，用 `origin/develop`。它必須包含 `ffcbb04`（#79 遠端節點第一刀）和 `64b6fde`（#81 連線排隊），用 `git merge-base --is-ancestor` 檢查，缺一個就不開跑。
- Ubuntu 上 `/home/linuxbrew/.linuxbrew/bin/snip` 是使用者常駐的 worker 服務 binary（`snip-worker.service`，47821 埠），不是受測對象；不要使用、替換或改動它。本輪的 worker 一律用從受測 SHA 編出來的 binary。
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
just remote-e2e-ssh ubuntu --listen 100.95.28.19:47899
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

# Git 檢視測試 fixture
echo TOPSECRET > "$W/git-secret.txt"
G="git -c user.name=t -c user.email=t@t"
g_init() {
  $G init -b main "$1" 2>/dev/null || { $G init "$1" && (cd "$1" && $G checkout -B main 2>/dev/null || true); }
}
cd "$W"
mkdir -p gitws/plain gitws/broken/.git outer/inner plainws
echo plain > gitws/plain/file.txt
echo 'plain text' > plainws/hello.txt

g_init gitws/alpha
printf 'commit 1 a\n' > gitws/alpha/a.txt
(cd gitws/alpha && $G add a.txt && $G commit -q -m "first commit")
printf 'commit 2 a\n' > gitws/alpha/a.txt
printf 'commit 2 b\n' > gitws/alpha/b.txt
(cd gitws/alpha && $G add a.txt b.txt && $G commit -q -m "second commit")
printf 'commit 2 a modified\n' > gitws/alpha/a.txt
printf 'new file content\n' > gitws/alpha/new.txt
printf 'staged file content\n' > gitws/alpha/staged.txt
(cd gitws/alpha && $G add staged.txt)
ln -s "$W/git-secret.txt" gitws/alpha/link-to-secret 2>/dev/null || true

g_init gitws/beta
printf 'beta content\n' > gitws/beta/b.txt
(cd gitws/beta && $G add b.txt && $G commit -q -m "beta commit")

g_init outer
printf 'outer committed\n' > outer/committed.txt
(cd outer && $G add committed.txt && $G commit -q -m "outer commit")
printf 'outer dirty\n' > outer/outer-dirty.txt
printf 'inner file\n' > outer/inner/file.txt
EOF
```

這段已在 `b41817f` 上試跑過：Ubuntu 編譯約 20 秒（有快取時），fixture 全部建立成功，`manydir` 有 1200 個檔案。試跑時從 Mac 用 `snip remote` 讀過：`escape.txt` 和 `escape-dir` 都回「Path leaves the workspace」，`target/release/rtk` 回「Preview exceeds 1 MiB」，`exact-1MiB.txt` 完整讀到 1048576 bytes，`.git/HEAD` 讀得到（專案樹不列 `.git`，但指名路徑仍可讀）。

rtk 的基準（I01 用；包含 HEAD、status、`.git/index` 的 sha256 與檔案時間戳；worker 端 oracle 與驗證命令均需加 `GIT_OPTIONAL_LOCKS=0`。真實專案不為此 touch 檔案；「stat-dirty 的 index 不被改寫」由 `scripts/remote_e2e.sh` 的 git views 段在 fixture 上驗證）。

定義快照函式（具備防覆寫保護）：

```bash
rtk_snapshot() {
  local out="$1"
  if [ -e "$out" ]; then
    echo "rtk_snapshot: $out 已存在，拒絕覆寫" >&2
    return 1
  fi
  ssh ubuntu 'cd ~/research/rtk && GIT_OPTIONAL_LOCKS=0 git --no-optional-locks rev-parse HEAD && GIT_OPTIONAL_LOCKS=0 git --no-optional-locks status --porcelain=v1 -z | sha256sum && (command -v sha256sum >/dev/null && sha256sum < .git/index || shasum -a 256 < .git/index) | cut -c1-64 && find . -newer .git/HEAD -not -path "./.git/*" | wc -l' > "$out"
}
```

開跑時僅執行一次，建立 `rtk-before.txt` 與時間戳 marker：

```bash
rtk_snapshot "$RUN/rtk-before.txt"
ssh ubuntu "[ -e '$W/rtk-marker' ] || touch '$W/rtk-marker'"
```

**警告：上述基準建立區塊僅在開跑時執行一次，收尾時絕不可重新執行（否則會覆寫基準並更新 marker 導致完整性失效）；收尾驗證請直接依照第 5 節 I01 執行。**

### 2.4 啟動 worker（每次都用這個函式）

```bash
start_worker() {   # $1 = 設定資料夾名稱；其餘參數 = 要分享的資料夾；環境變數 WORKER_EXTRA = 額外的 worker 旗標（例如 --max-protocol 1）
  local cfg=$1; shift
  local shares=""; for d in "$@"; do shares="$shares --share '$d'"; done
  ssh -o BatchMode=yes ubuntu "bash -s" <<SH
cd '$W'
pkill -f '$W/src/target/release/snip worker' 2>/dev/null
for i in \$(seq 1 20); do pgrep -f '$W/src/target/release/snip worker' >/dev/null || break; sleep 0.5; done
SNIP_CONFIG_DIR='$W/$cfg' SNIP_DEVICE_NAME=ubuntu-ui nohup '$W/src/target/release/snip' worker $WORKER_EXTRA $shares --listen 100.95.28.19:47899 > worker.log 2>&1 < /dev/null &
echo \$! > '$W/worker.pid'
for i in \$(seq 1 40); do grep -q 'pairing code' worker.log && break; grep -q rror worker.log && break; sleep 0.5; done
cat worker.log
SH
}
start_worker wcfg "$W/edge" /home/audichuang/research/rtk "$W/gitws" "$W/plainws" "$W/outer/inner" | tee "$RUN/worker-start-1.log"
```

這個函式已經試跑過。有四個陷阱，不要改掉：

- 一定要有 `< /dev/null`，否則 ssh 會一直等背景的 worker，不會返回。
- 要等舊的 worker 真的結束才啟動新的，否則會出現 `Address already in use`。
- `pkill -f` 要放在 `bash -s` 的 stdin 裡執行。如果直接寫成 `ssh ubuntu 'pkill -f "snip worker"'`，pattern 會比對到執行它的那個 bash 自己，連 ssh 連線一起被殺掉（exit 255）。pattern 必須包含 `$W/src/target/release/snip`，不可寫成會比對到使用者服務 binary 的寬鬆字串（例如 `pkill snip`）。
- start_worker 會把 worker 的 PID 寫進 `$W/worker.pid`，R37、G11 與第 6 節收尾都靠它停 worker；不要用 pkill 取代。

輸出要有 `snip-sync worker listening on 100.95.28.19:47899`、`fingerprint XXXX-XXXX-XXXX-XXXX`、五行 `sharing …`，以及 `pairing code ABCD-EFGH (valid 10 minutes; restart for a new one)`。

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
2. **專案樹的兩套列家族與日誌（依工作區形狀決定）**：
   - **單一 repo 工作區（工作區資料夾本身就是 Git 儲存庫，例如 `rtk`）**：呈現方式與本機單一 repo 完全一致，以 `repo-row:<名稱>` 標頭列（附帶 `repo-chevron:<名稱>`）為頂層，其下的檔案／目錄列使用無前綴家族：`tree-row:<相對路徑>`、`tree-chevron:<相對路徑>`、截斷標記 `tree-marker:<資料夾>`、更多列 `tree-view-more:<資料夾>`（整棵樹截斷列為 `tree-view-more:`）、錯誤列 `tree-retry:<資料夾>`、非 UTF-8 名稱 `tree-invalid:<suffix>`。日誌為 `TREE_PAGE: rel= kind= children= has_more= selected=`、`TREE_FILE_SELECTED`。
   - **多 repo 或純資料夾工作區（工作區資料夾本身不是開啟的 repo，例如 `edge`、`plainws`、`outer/inner`）**：工作區資料夾本身的樹使用 `ws-` 前綴家族：`ws-tree-row:<相對路徑>`、`ws-tree-chevron:<相對路徑>`、截斷標記 `ws-tree-marker:<資料夾>`、更多列 `ws-tree-view-more:<資料夾>`（整棵樹截斷列為 `ws-tree-view-more:`）、錯誤列 `ws-tree-retry:<資料夾>`、非 UTF-8 名稱 `ws-tree-invalid:<suffix>`。日誌為 `WS_TREE_PAGE: rel= kind= children= has_more= selected=`、`WS_FILE_SELECTED`。多 repo 資料夾中探索到的 repo（例如 `edge` 裡的 `nested`）顯示為 `repo-row:<名稱>` 標頭列（附帶 `repo-chevron:<名稱>`），不是 `ws-tree-row`；若 repo summary 發生錯誤（例如 `nested` 僅含空 `.git`），nested 列顯示警告標記；開啟 `edge` 時會自動選取 nested，因此最初即顯示其錯誤；點擊已展開且已選取的 `repo-row:nested` 僅會將其收合（預覽不變）；重新展開（第二次點擊）會重新選取並顯示 Git 錯誤。絕不能當作純資料夾展開（不得有 `WS_TREE_PAGE: rel=nested`）。
   - `TREE_TOGGLED`／`WS_TREE_TOGGLED` 只在勾選時印，遠端模式不能勾選，所以展開不會有它。
3. **沒有日誌 tag 的訊息**：`remote_unsupported`（遠端工作區只支援瀏覽、預覽與唯讀的 Git 檢視）顯示在狀態列；`remote_pair_missing`（請輸入位址與配對碼）與工作區清單讀取失敗顯示在工作區選單裡的紅字，不在狀態列。這些格子的通過線是：截圖裡在上述位置有那段文字、剪貼簿 sentinel 的 SHA 不變、1 秒內沒有新的 `COPY_*` 或 `PASTE_*` 行。**不要因為少一行日誌就判 `fail`。**
4. **預覽成功與失敗的分法**（`apply_source_preview`）：成功時日誌是 `PREVIEW_LOADING: <path>` 接著 `PREVIEW_LOADED: <path>`。worker 拒絕、二進位、過大時，只有 `PREVIEW_LOADING`，**沒有** `PREVIEW_LOADED`，預覽區顯示錯誤文字。失敗格的通過線：有 `PREVIEW_LOADING`、沒有 `PREVIEW_LOADED`、截圖裡是錯誤文字而不是檔案內容、下一次點擊 App 仍有反應。
5. **指紋比對**：`REMOTE_PAIRED: fp=` 是 16 個十六進位字元。worker 印出的是 `XXXX-XXXX-XXXX-XXXX`。去掉 `-`、不分大小寫比較，兩者必須相同。選單列顯示的是 `XXXX-XXXX-XXXX-XXXX`。
6. **文字輸入**：先點 `remote-addr-input` 或 `remote-code-input` 的 bounds，再用鍵盤輸入，然後截圖確認輸入框裡的字。貼上文字用 Cmd+V 時，要確認焦點在輸入框裡；否則 Cmd+V 是 App 的貼上。
7. **worker 端 oracle**：worker 上檔案的內容和清單，用 `ssh ubuntu` 讀，存成檔再算 SHA-256。不要拿 App 自己的輸出去驗證 App。
8. **擷取 tooltip 方法**：將滑鼠移至目標列的 bounds 中心 hover，等待約 1 秒，再進行截圖。

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
| Changes 空狀態 | `changes-empty` |
| Log 空狀態 | `log-empty` |

| 日誌 | 意思 |
|---|---|
| `[APP:REMOTE_PAIRED: name=<名稱> fp=<16 hex>]` | 配對成功。之後 App 會自動列出它的工作區 |
| `[APP:REMOTE_PAIR_FAILED: <訊息>]` | 配對失敗 |
| `[APP:REMOTE_WORKSPACES: count=N]` | 列出工作區成功（失敗時沒有這行，只在選單顯示紅字） |
| `[APP:REMOTE_OPENED: <worker> ▸ <工作區> generation=N]` | 開啟遠端工作區 |
| `[APP:TREE_ROW_REFUSED: not-utf8]` | 點了名稱不是 UTF-8 的列，無法開啟 |
| `[APP:CHANGES_EMPTY: state=…]` | Changes 空狀態改變：`no_workspace`、`scanning`、`loading`、`no_repository`、`scan_failed`、`no_match`、`clean`、`clean_partial`（乾淨，但至少一個 repo 沒讀完整，不等於完整乾淨，R32b 等步驟仍以 `state=clean` 判定） |
| `[APP:LOG_EMPTY: state=…]` | Log 空狀態改變：`no_workspace`、`scanning`、`loading`、`no_repository`、`failed`、`empty` |
| `[APP:E2E_REPO: name=<名稱> ok=true/false …]` | 儲存庫掃描或載入狀態 |
| `[APP:CHANGES_LOADED: <名稱> files=N]` | Changes 面板儲存庫變更清單載入完成 |
| `[APP:REPO_LOADED: <名稱> files=N]` | 單一儲存庫變更載入完成 |
| `[APP:REMOTE_WORKER: state=…]`、`[APP:REMOTE_PAIRING: state=open]` | 本機當 worker（本規程不測） |

## 4. 測案

每格都要截圖（點擊前、點擊後各一張），存在 `$RUN/<ID>/`，再加一份 `action.json`，內容是 bounds、scale、算出的螢幕點和新的日誌行。操作者在寫入任何檔案至該處前必須先 `mkdir -p "$RUN/<ID>"`（R02、R03、R13、R25、R26 會透過 shell 重新導向寫入該處）。

### 4.1 配對（R01–R05 連續做完，要在配對碼過期前）

| ID | 動作 | 通過線 |
|---|---|---|
| R01 | 點 `btn-workspace-menu` | 有 `btn-remote-pair-new`、`btn-remote-worker-toggle` 的 bounds；沒有任何 `remote-worker:*`（全新的設定資料夾）。截圖有「遠端節點」區塊 |
| R02 | 本格遵循第 3 節規則 3（沒有日誌 tag 的訊息）：點擊前依剪貼簿 sentinel 說明寫入 sentinel；點 `btn-remote-pair-new`，兩個欄位都空著，點 `btn-remote-pair` | 出現 `remote-addr-input`、`remote-code-input`；選單截圖在配對表單處以紅字顯示「請輸入位址與配對碼」；沒有 `REMOTE_PAIRED`／`REMOTE_PAIR_FAILED`；儲存 `R02/sentinel-before.sha` 與 `R02/sentinel-after.sha` 且雜湊不變，1 秒內沒有新的 `COPY_*` 或 `PASTE_*` 行 |
| R03 | 位址輸入 `100.95.28.19:47899`，配對碼輸入錯的 `AAAA-AAAA`，點 `btn-remote-pair` | 一行新的 `REMOTE_PAIR_FAILED`；選單裡是紅字錯誤；「未記錄配對」之證據必須在配對失敗後立即存檔：執行 `ls -l "$RUN/master-config"` 與 `cat "$RUN/master-config/remote-workers.json"`（若存在），存為 `R03/workers-json.txt`（檔案不存在或內容未含此 worker）。缺少該存檔者本格判 `not-run` |
| R04 | 配對碼改成 worker 印出的那組（位址用 `100.95.28.19:47899`，要帶埠，因為測試 worker 不在預設埠），點 `btn-remote-pair` | 出現 `REMOTE_PAIRED: name=ubuntu-ui fp=…`，fp 和 worker 指紋一致（第 3 節規則 5）；接著 `REMOTE_WORKSPACES: count=5`；`remote-workers.json` 有一筆 `ubuntu-ui`。按鈕在配對時會短暫顯示「配對中…」，截到就附上，截不到不影響判定 |
| R05 | 截圖選單；hover 擷取 tooltip（將滑鼠移至目標列 bounds 中心，等待約 1 秒再截圖） | `remote-worker:0` 那一列顯示 `ubuntu-ui` 與位址；當第二行文字被截斷時，hover 該列顯示 tooltip，其內容包含完整位址與完整指紋（`XXXX-XXXX-XXXX-XXXX`），指紋必須與 R04 一致（第 3 節規則 5）；底下五列 `remote-workspace:0..4` 依序為 `edge`、`rtk`、`gitws`、`plainws`、`inner`（依 worker 啟動時傳入的順序排列），各自附有 Ubuntu 上的路徑；路徑太長被截斷時，hover 該工作區列，tooltip 顯示完整路徑（擷取方式同上） |

### 4.2 瀏覽真實專案 rtk

| ID | 動作 | 通過線 |
|---|---|---|
| R06 | 前置條件：切換前，`local-ws` 的專案樹必須正顯示 `tree-row:local.txt`（必要時打開 `rail-project`）並記錄其最新 `CTRL_BOUNDS`；只有先取得過 bounds 的 ID 才能斷言 GONE。若切換前不可見，本格判 `not-run`，絕不可判 `pass`。<br>動作：點顯示 `rtk` 的那一列 `remote-workspace:<wx>` | `REMOTE_OPENED: ubuntu-ui ▸ rtk`；選單關閉；左上角顯示 `ubuntu-ui ▸ rtk`；狀態列顯示「已開啟遠端工作區 …」；`tree-row:local.txt` 那一列有 `CTRL_GONE` |
| R07 | 讀根目錄 | 頂層顯示 `repo-row:rtk` 標頭列；其下根目錄的 `tree-row:*` 集合等於 `ssh ubuntu 'ls -A ~/research/rtk'` 的結果去掉 `.git`（專案樹刻意不列 `.git`，和本機模式一致），`target` 要在；資料夾排在前面，同類照名稱排序 |
| R08 | 點 `tree-row:src`（或 `tree-chevron:src`）展開 | `TREE_PAGE: rel=src … children=N`，N 等於 `ls -A src \| wc -l`；子列的 ID 是 `tree-row:src/<名稱>` |
| R09 | 點 `tree-row:src/main.rs` | `TREE_FILE_SELECTED`、`PREVIEW_LOADING`、`PREVIEW_LOADED: src/main.rs`；預覽前 20 行和 `ssh ubuntu 'head -20 ~/research/rtk/src/main.rs'` 一致（截圖比對）；有 Rust 語法上色；預覽上方的路徑列是 `ubuntu-ui ▸ rtk › src › main.rs`，不是 `snip-remote://…` |
| R10 | 點 `tree-row:README_zh.md` | `PREVIEW_LOADED`；中文正常顯示，沒有豆腐字或亂碼 |
| R11 | 展開 `src/hooks`，點 `tree-row:src/hooks/init.rs`（238 KB），在預覽裡捲到最後 | `PREVIEW_LOADED`；最後一行和 `tail -1` 一致；捲動時 App 不卡 |
| R12 | 展開 `target`、`release`，點 `tree-row:target/release/rtk`（8 MB 二進位） | 依第 3 節規則 4 判失敗格：顯示二進位或超過 1 MiB 的錯誤都算對；沒有亂碼文字；5 秒內可以點下一列 |
| R13 | 看根目錄，再執行 `mkdir -p "$RUN/R13" && target/debug/snip remote cat 1 rtk .git/HEAD > "$RUN/R13/head.txt"; cat "$RUN/R13/head.txt"` | 樹裡沒有 `tree-row:.git` 列；CLI 印出 `ref: refs/heads/…`（`.git` 只是不列在樹裡，指名路徑仍可讀）。證據：截圖顯示完整根目錄清單（必要時捲動，註明清單完整），加上從 App 日誌 grep `CTRL_BOUNDS` 得到的 `tree-row:` ID 原始清單證明無 `tree-row:.git`，以及儲存的 CLI 輸出 `R13/head.txt`。缺少存檔 `R13/head.txt` 者判 `not-run` |
| R14 | 把 `src` 收合再展開（點 `tree-chevron:src`） | 第二次也有 `TREE_PAGE: rel=src`，清單和 R08 相同 |

### 4.3 邊界案例 edge

| ID | 動作 | 通過線 |
|---|---|---|
| R15 | 從工作區選單切到 `edge`（先截圖確認列名） | `REMOTE_OPENED: ubuntu-ui ▸ edge`；rtk 的列（`tree-row:*`、`repo-row:rtk`）都有 `CTRL_GONE`；edge 的根目錄由 `ws-tree-row:*` 列加上 `repo-row:nested` 標頭列組成，合起來與 `ls -A "$W/edge"` 一致，非 UTF-8 那個名稱除外（見 R27） |
| R16 | 依序展開 `src`、`deep`，點 `中文 有空白.txt` | `PREVIEW_LOADED: src/deep/中文 有空白.txt`；顯示 `深層 檔案 ✓ 🦀` |
| R17 | 點 `empty.txt` | `PREVIEW_LOADED`；預覽是空的，不是錯誤，也不是上一個檔案的內容 |
| R18 | 點 `crlf.txt` | `PREVIEW_LOADED`；兩行 `line1`、`line2`，沒有顯示 `^M` 或多出空行 |
| R19 | 點 `exact-1MiB.txt` | `PREVIEW_LOADED`（剛好 1 MiB 要能預覽）；就算 App 的長行顯示另外截斷，也只能出現本機同一規則的截斷提示 |
| R20 | 點 `over-1MiB.txt` | 失敗格：錯誤文字提到超過 1 MiB |
| R21 | 點 `blob.bin` | 失敗格：二進位或非 UTF-8，無法預覽 |
| R22 | 點 `latin.txt` | 失敗格：同 R21 |
| R23 | 點 `manydir` 展開，再反覆點最新的 `ws-tree-view-more:manydir` 或 `ws-tree-view-more:`，直到沒有更多列；執行 oracle 命令：`mkdir -p "$RUN/R23" && target/debug/snip remote ls 1 edge manydir > "$RUN/R23/cli-ls.txt" 2> "$RUN/R23/cli-ls.err"; wc -l < "$RUN/R23/cli-ls.txt"` | CLI 每行一項，stdout 存入 `R23/cli-ls.txt`，`wc -l` 印出 1000（`R23/cli-ls.err` 包含 `(listing truncated)` 截斷提示）；GUI 日誌 `WS_TREE_PAGE: rel=manydir … children=N`，N ≤ 1000（GUI 的記憶體預算可能收得比 1000 少，記下實際值）；每點一次，畫面都出現新的 `ws-tree-row:manydir/…`；最後有 `ws-tree-marker:manydir`，顯示「[目錄未完整列出: 已截斷]」；捲到底不卡。缺少存檔 `R23/cli-ls.txt` 者判 `not-run` |
| R24 | 點 `escape.txt` | 失敗格：錯誤文字說路徑離開了工作區；`top-secret-c0ffee` 不出現在截圖，也不在 `app-*.log` 裡（`grep -c top-secret "$RUN"/app-*.log` 要是 0） |
| R25 | 執行 oracle 命令：`mkdir -p "$RUN/R25" && target/debug/snip remote ls 1 edge > "$RUN/R25/cli-ls.txt"`；在 App 點 `escape-dir` | 拒絕。指向分享外的資料夾 symlink 列成檔案列（CLI 每行一項，在 `R25/cli-ls.txt` 中印為 `escape-dir`，沒有尾端 `/`）；點下去是失敗格，錯誤文字是「Path leaves the workspace」。**不能**列出 `/etc` 的內容（截圖裡沒有 `passwd`、`hostname`）。缺少存檔 `R25/cli-ls.txt` 者判 `not-run` |
| R26 | 點 `inner-link`；對照 R25 保存之 oracle `"$RUN/R25/cli-ls.txt"`（或執行 `mkdir -p "$RUN/R26" && target/debug/snip remote ls 1 edge > "$RUN/R26/cli-ls.txt"`） | 指向分享內的資料夾 symlink 列成資料夾（CLI 在 `cli-ls.txt` 中每行一項，印為 `inner-link/`，帶有尾端 `/`）：可以展開，列出 `deep`、`main.rs`；點 `inner-link/main.rs` 能預覽。顯示成檔案列或無法展開，判 `fail`。缺少 `cli-ls.txt` 存檔（R25 或 R26 的）者判 `not-run` |
| R27 | 找到非 UTF-8 名稱那一列 | 它的 ID 是 `ws-tree-invalid:<suffix>`，名稱用替代字元顯示；點它有一行新的 `TREE_ROW_REFUSED: not-utf8`，狀態列顯示「檔名不是有效的 UTF-8，無法開啟或預覽」；App 不崩潰，選取與預覽維持原樣 |
| R28 | 依序展開 `a/b/c/d/e`，點 `leaf.txt` | 每層各有一行 `WS_TREE_PAGE`；顯示 `deepest` |
| R29 | 看 `nested` 和 `.hidden` | `nested` 視為 repo（僅含空 `.git`，見 R32），顯示為 `repo-row:nested` 標頭列（具錯誤狀態），不是可展開的資料夾，點擊不產生 `WS_TREE_PAGE: rel=nested`；`.hidden` 列出為 `ws-tree-row:.hidden`，可以預覽 |

### 4.4 拒絕寫入類操作（任一遠端工作區）

每格開始前先放 sentinel（第 3 節）。

| ID | 動作 | 通過線 |
|---|---|---|
| R30 | 選一個檔案列，按 Cmd+C，再點 `btn-copy` | 兩次都顯示「遠端工作區只支援瀏覽、預覽與唯讀的 Git 檢視」；sentinel 的 SHA 不變；沒有 `COPY_PREP`／`COPY_DONE` |
| R31 | 按 Cmd+V，再點 `btn-paste` | 兩次都顯示「遠端工作區只支援瀏覽、預覽與唯讀的 Git 檢視」；沒有 `PASTE_PREVIEW`／`PASTE_LOADING`；沒有出現貼上面板；`ssh ubuntu "find '$W/edge' -newer '$W/rtk-marker' \| wc -l"` 是 0 |
| R32 | 點 `rail-changes`、`rail-log` | 此時開著的是 `edge`（R15 切過去的）：點 `rail-changes`，`edge/nested` 只有空的 `.git` 資料夾，探索把它當 repo（`workspace.rs:540-548` `classify_git`），所以必須看到 `nested` 的錯誤列（Note），**不能**出現 `state=clean`，也沒有 `local-ws` 的任何列；點 `rail-log`，看到 `log-empty` 為 `failed`（或錯誤提示列含 `nested`），不是「沒有 commit」。**空畫面或 `state=clean` 判 `fail`** |
| R32b | 從工作區選單切回 `rtk`，點 `rail-changes` 與 `rail-log` | 點 `rail-changes` → `state=clean`（rtk 工作樹乾淨）；點 `rail-log` → 第一列是受測當天 `ssh ubuntu 'git -C ~/research/rtk rev-parse HEAD'` 的 commit。**空畫面判 `fail`**。R34 之前照原文先開回 `edge` |
| R33 | 用 Cmd+Shift+O（或工作區選單的最近工作區）打開本機的 `$RUN/local-ws`，選 `local.txt`，按 Cmd+C | 左上角不再有 `ubuntu-ui ▸`；`COPY_DONE`；`pbpaste` 拿到 snip-sync 的 payload（剪貼簿 SHA 和 sentinel 不同）。這格驗證離開遠端之後，遠端狀態有清乾淨 |

### 4.5 即時變化與 worker 生命週期

R34 之前，先重新開回 `edge`（R15 的步驟）。

| ID | 動作 | 通過線 |
|---|---|---|
| R34 | `ssh ubuntu "echo fresh-1 > '$W/edge/new.txt'"`，點 `btn-refresh` | 樹重新讀取；出現 `ws-tree-row:new.txt`，預覽顯示 `fresh-1` |
| R35 | `ssh ubuntu "echo fresh-2 > '$W/edge/new.txt'"`，點別的檔案再點回 `new.txt` | 顯示 `fresh-2`，不是快取的舊內容 |
| R36 | `ssh ubuntu "rm '$W/edge/new.txt'"`，點 `btn-refresh` | 重建後的根目錄 `WS_TREE_PAGE: rel= …` 行（Refresh 後根目錄清單抵達）之後，不再出現 `ws-tree-row:new.txt` 的 `CTRL_BOUNDS`（該列不在重建後的清單中）；最後一行 `PREVIEW_LOADING` 為 `new.txt` 且其後沒有 `PREVIEW_LOADED`；預覽改顯示指名 `new.txt` 的找不到檔案或讀取失敗錯誤，不再是 `fresh-2`；絕不可顯示其他 repo 的 Git 錯誤，`edge/nested`（錯誤 repo）絕不能搶佔預覽區 |
| R37 | 停掉 worker：`ssh ubuntu "kill \$(cat '$W/worker.pid')"`，在 App 點一個沒預覽過的檔案 | 失敗格；`PREVIEW_LOADING` 之後 10 秒內出現錯誤（master 連線逾時 2 秒、讀取 5 秒）；這段時間 App 沒有凍結（可以捲動、可以開選單）；接著按 Cmd+Shift+W 關掉工作區，要在 8 秒內完成 |
| R38 | `start_worker wcfg "$W/edge" /home/audichuang/research/rtk "$W/gitws" "$W/plainws" "$W/outer/inner"`；確認指紋和第一次一樣；在 App 選單點 `remote-worker:0`，開 `edge`，點一個檔案 | 不需要重新配對；`REMOTE_WORKSPACES: count=5`、`REMOTE_OPENED`、`PREVIEW_LOADED` |
| R39 | 取消分享：`start_worker wcfg /home/audichuang/research/rtk`（只分享 rtk）。App 不重開，直接點 `edge` 裡另一個檔案，再開選單點 `remote-worker:0` | 預覽被拒絕（失敗格）；選單只列出 `rtk`（`REMOTE_WORKSPACES: count=1`） |
| R40 | 換一張憑證：`start_worker wcfg-other "$W/edge"`（同一個位址，新的設定資料夾），在 App 選單點 `remote-worker:0` | 拒絕；選單顯示紅字，內容說明這不是已配對的 worker（內容含兩個指紋）；`remote-workers.json` 的指紋沒有被改成新的 |
| R41 | 復原：`start_worker wcfg "$W/edge" /home/audichuang/research/rtk "$W/gitws" "$W/plainws" "$W/outer/inner"`，點 `remote-worker:0` | `REMOTE_WORKSPACES: count=5` |
| R42 | Cmd+Q，等 exit code 0，用同一個 `SNIP_CONFIG_DIR` 重新啟動（寫進新的 `app-N.log`），開選單 | `remote-worker:0` 仍然是 `ubuntu-ui`，點它就能列出工作區，不需要重新配對 |
| R43 | 錯誤 5 次作廢：`start_worker wcfg "$W/edge" /home/audichuang/research/rtk "$W/gitws" "$W/plainws" "$W/outer/inner"` 拿新的配對碼 C。Cmd+Q，改用全新的 `SNIP_CONFIG_DIR="$RUN/master-config-r43"` 啟動 App（寫進新的 `app-N.log`）。用 `btn-remote-pair-new` 以錯碼配對 5 次，第 6 次用 C（位址同樣用 `100.95.28.19:47899`）。再 Cmd+Q，用原本的 `SNIP_CONFIG_DIR` 重新啟動，點 `remote-worker:0` | 5 行 `REMOTE_PAIR_FAILED`；第 6 次也是 `REMOTE_PAIR_FAILED`（碼已作廢）；換回原本的設定後，`remote-worker:0` 仍然能列出工作區。一定要用全新的 master：已配對的 master，worker 認得它的憑證，不看配對碼就放行 |
| R44 | 把視窗調成 900×600（從系統層設定，例如 System Events 設成 900×632；送給 App 的合成拖曳碰不到視窗框），開選單並打開配對表單 | 兩個輸入框和「配對」按鈕的 bounds 都 `w,h ≥ 1`，而且都在內容區裡面；選單可以捲動到最下面；焦點在輸入框時按 Escape，選單收起；再開選單，點 `btn-workspace-menu`，選單也會收起 |
| R45 | 先開著 `edge`，再點 `btn-remote-forget:0` | 開著的 `edge` 跟著關閉：左上角不再有 `ubuntu-ui ▸`，`ws-tree-row:*` 都有 `CTRL_GONE`；`remote-worker:0` 有 `CTRL_GONE`；`remote-workers.json` 不再有 `ubuntu-ui`；重開選單也不會再出現；忘記 worker 且工作區關閉後，Log 面板絕不可顯示掃描／搜尋狀態（如「正在搜尋 Git 儲存庫…」），必須顯示無工作區／空狀態（未開工作區時 `log-empty` 不為 `scanning`，關閉完成後印出 `[APP:LOG_EMPTY: state=no_workspace]`（若顯示 Changes 面板則印出 `[APP:CHANGES_EMPTY: state=no_workspace]`），且 `log-empty` probe 顯示文字「未開啟工作區。開啟一個 Git 儲存庫，或內含多個儲存庫的資料夾。」；短暫的 `loading` 或 `scanning` 不算通過） |

### 4.6 CLI master 交叉驗證

R45 之前，或在 R45 之後重新配對一次（重啟 worker 拿新碼，用 GUI 配對）。

| ID | 動作 | 通過線 |
|---|---|---|
| C01 | `target/debug/snip remote workers`（同一個 `SNIP_CONFIG_DIR`） | 不用另外配對就列出 `1	ubuntu-ui	100.95.28.19:47899	<指紋>`，證明 GUI 和 CLI 共用配對紀錄 |
| C02 | `snip remote workspaces 1`、`snip remote ls 1 rtk src` | 工作區清單與 R05 在 GUI 上看到的五個工作區一致；`snip remote ls 1 rtk src` 的項目與個數和 GUI 上 `tree-row:src` 展開的子項目及 `TREE_PAGE` children 計數一致 |
| C03 | 對 rtk 抽 20 個 tracked 文字檔，比較 `snip remote cat 1 rtk <f> \| shasum -a 256` 和 `ssh ubuntu sha256sum` | 20/20 相同 |
| C04 | App 開著 rtk 的時候，同時跑 50 個平行的 `snip remote cat 1 rtk src/main.rs` | 50/50 正確；這段時間在 GUI 點檔案仍然能預覽（`PREVIEW_LOADED`） |

### 4.7 遠端唯讀 Git 檢視（G01–G12）

若 R45 已忘記 worker，進入 §4.7 之前需重新啟動 worker 並在 GUI 重新配對一次：

```bash
start_worker wcfg "$W/edge" /home/audichuang/research/rtk "$W/gitws" "$W/plainws" "$W/outer/inner"
```

在 GUI 完成配對，以確保 `gitws`、`plainws` 與 `outer/inner` 均已分享且完成配對。

| ID | 動作 | 通過線 |
|---|---|---|
| G01 | 從工作區選單開 `gitws`，點 `rail-changes` | 掃描發現儲存庫；Changes 列出 alpha 的四個項目（`a.txt` 改動、`staged.txt` 暫存、`new.txt` 未追蹤、`link-to-secret` 未追蹤符號連結），分組與 `git status --porcelain=v2` 的 oracle 一致（若點選 `link-to-secret` 預覽必須被拒絕）；beta 乾淨沒有列出；`broken`（空 `.git`）顯示為 Note 錯誤列；日誌有 `[APP:E2E_REPO: …]`，筆數與狀態與 oracle 一致 |
| G02 | 在 Changes 面板點 `a.txt` | 右側 diff 顯示 patch，內容包含 `commit 2 a modified` 改動行，不是空畫面或錯誤 |
| G03 | 點 `rail-log`，看跨 repo 合併歷史，再使用 Repository 下拉選單只選 `alpha` | 預設顯示跨 repo 的 merged log（包含 alpha 與 beta 的 commit）；在 Repository 篩選器只選取 alpha 後，commit 清單僅顯示 alpha 的 commit |
| G04 | 點選 alpha 的 HEAD commit | 右下方變更檔案清單與 worker 端 `git diff-tree --no-commit-id --name-only -r HEAD` 逐一相符 |
| G05 | 在 commit 變更檔案清單中點選其中一個檔案 | 右側預覽顯示該 commit 檔案的 diff |
| G06 | 點開分支／ref 選擇器 | 選擇器下拉清單正確列出 `main` 分支 |
| G07 | 歷史面板切換至 commit 樹瀏覽 | 可展開目錄、瀏覽 commit 樹節點與檢視 blob 檔案內容 |
| G08 | 從工作區選單開 `plainws` | 專案樹正常瀏覽檔案；點 `rail-changes` 顯示 `changes-empty` probe，日誌為 `[APP:CHANGES_EMPTY: state=no_repository]`；點 `rail-log` 顯示 `log-empty` probe，日誌為 `[APP:LOG_EMPTY: state=no_repository]`，提示「這個資料夾裡沒有 Git 儲存庫」，不是 clean 或 empty |
| G09 | 從工作區選單開 `outer/inner` | 判定為 `no_repository`，專案樹只列出 `inner` 的檔案，專案樹與變更清單絕不出現父 repo 的 `outer-dirty.txt` |
| G10 | 在 worker 端修改 beta 的檔案：`ssh ubuntu "echo beta-change >> '$W/gitws/beta/b.txt'"`，在 App 點 `btn-refresh`；hover `change-repo:unstaged:beta` 列截取 tooltip（方法見第 3 節規則 8） | 重新整理後 Changes 面板中 `beta` 出現變更列，即時反映 worker 上的檔案修改；hover repo 標頭列（例如 `change-repo:unstaged:beta`）時，tooltip 顯示 worker 名稱加上 worker 路徑（如 `ubuntu-ui:/home/.../gitws/beta`），絕不可露出 `snip-remote://` 內部識別路徑 |
| G11 | 停掉 worker（`ssh ubuntu "kill \$(cat '$W/worker.pid')"`），在 Changes 點選一個檔案（例如 `beta` 的 `b.txt`） | 預覽顯示連線失敗錯誤，絕不誤顯示為乾淨或空內容；預覽上方的路徑列（`breadcrumb`）必須指名所點選檔案所屬的 repo（點 `beta/b.txt` 時為 `beta`），不可誤顯示為先前開啟的 repo；App 不凍結 |
| G12 | 以受測 SHA 的 `snip worker --max-protocol 1` 啟動 worker，分享 `gitws` 與 `edge`：`WORKER_EXTRA='--max-protocol 1' start_worker wcfg "$W/edge" "$W/gitws"`，從工作區選單重新開啟 `gitws` | 開啟後專案樹檔案瀏覽與預覽立即正常運作，無需按重新整理；若專案樹卡在載入狀態、必須手動按 Refresh 才出現，判 `fail`；點 `rail-changes` 與 `rail-log` 皆顯示「版本太舊」錯誤（`remote_worker_too_old`），提示在 worker 上更新；底部狀態列也顯示同一則錯誤，不可停在「正在掃描儲存庫…」；絕不顯示成 clean 或 empty |

## 5. 完整性（決定這一輪可不可信）

| ID | 檢查 | 通過線 |
|---|---|---|
| I01 | 結束時僅呼叫 `rtk_snapshot "$RUN/rtk-after.txt"`（絕不重跑 2.3 基準區塊，避免覆寫 before 或 touch marker），接著比對 `cmp "$RUN/rtk-before.txt" "$RUN/rtk-after.txt"`（或 `diff -u`），並執行 `ssh ubuntu "find ~/research/rtk -newer '$W/rtk-marker' -not -path '*/.git/*' \| wc -l"`。所有 git 指令一定要加 `GIT_OPTIONAL_LOCKS=0`（及 `--no-optional-locks`）：少了它，git 會建立又刪掉 `.git/index.lock`，`.git` 目錄的修改時間因此變新，下面的 `find` 就會算到它 | 和 `rtk-before.txt` 完全相同（HEAD、status、`.git/index` 的 sha256 與檔案時間戳均未改變，證明 Git 檢視未修改 index）；另外 `find` 結果是 0 |
| I02 | 同 2.2，重新列出 `$REAL` 並算雜湊 | 和 `real-config-before.*` 相同（這一輪沒有碰真實的配對紀錄） |
| I03 | `grep -c top-secret-c0ffee "$RUN"/app-*.log "$RUN"/*/action.json` | 全部是 0 |

## 6. 收尾

```bash
ssh ubuntu "kill \$(cat '$W/worker.pid') 2>/dev/null; rm -rf '$W'"
```

只刪 `$W`。只停本輪的 worker，不要停使用者的 `snip-worker.service` / `com.audichuang.snip-worker`（47821 埠）。不要動 `~/research/rtk`，也不要動 Ubuntu 上 linuxbrew 的 `snip`。Mac 上的 `$RUN` 保留，裡面是證據。

## 7. 計分

`scorecard.md` 列出 R01–R45、R32b、G01–G12、C01–C04、I01–I03，每格一個判定，並附證據路徑。

- **閘門**：第 0 步閘門通過，而且 R01–R45、R32b、G01–G12、C01–C04、I01–I03 全部是 `pass`，才寫「遠端節點真實 UI 閘門關閉」。
- 有任何 `ui-defect`、`fail` 或 `not-run`，第一句就寫「遠端節點真實 UI 閘門打開」，並列出那些 ID。
- I01 或 I02 不是 `pass`，整輪結果作廢：這一輪動到了真實資料，先報告，再處理其他格子。
- R27 的預期允許多種畫面。判 `pass` 時，證據欄要寫實際看到的是哪一種，下一輪才能收緊。

處理結果時，每個不是 `pass` 的格子先分清楚是哪一種錯，再動手。三種的修法不同，混在一起會修錯地方：

- **規程錯**：通過線和產品的設計不符，例如要求一個不會出現的日誌 tag、把刻意隱藏的 `.git` 當成必須列出、量測步驟自己弄髒了受測資料。修這份規程；產品不動。
- **工具做不到**：操作者的工具到不了驗證點，例如捲動或拖曳視窗沒有作用。該格維持 `not-run`；在規程寫下已驗證可行的做法，或換工具。產品不動。
- **產品缺陷**：從截圖和日誌確認後，先寫一個會失敗的測試，再修產品；需要的話，規程也補上對應的通過線。
