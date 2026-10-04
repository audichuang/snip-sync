# 遠端工作區真實 UI 驗收規程（Mac mini master ↔ Ubuntu，經 ssh）

狀態：操作規程。給會操作滑鼠的 agent（Codex）照著點真實 macOS 視窗。產品行為以 [spec.md 第 8 節](spec.md) 為準。點擊方法、座標換算、判定用詞沿用 [real-ui-operator-protocol.md](real-ui-operator-protocol.md)，本文只寫遠端工作區多出來的部分。

這份規程只回答一件事：從 Mac mini 的桌面 App 經 ssh 打開 Ubuntu 上的資料夾，所有操作是不是**和打開本機資料夾完全一樣**。

## 0. 範圍

- **受測**：master 是 Mac mini 上的桌面 App（GUI），另有一段用 `snip remote`（CLI master）交叉驗證。App 從 `~/.ssh/config` 列主機，點主機時執行 `ssh ubuntu snip serve --stdio`；Ubuntu 上不常駐任何 worker。
- **ssh 是受測功能**：主機清單、逐層選資料夾、最近開啟、重開 App 自動接回、對方沒有 snip 或版本太舊時的錯誤，都要測。
- **和本機相同是通過線**：瀏覽、預覽、Changes、Log、右鍵「複製」與 Cmd+C、貼上（檔案模式與 commit 模式），在遠端工作區要和本機工作區有一樣的結果。每一格的 oracle 都是「在 Ubuntu 上用同一個 `snip` 對同一個資料夾做同一件事」的輸出，不是 App 自己的輸出。
- **仍然拒絕的操作**：加入儲存庫路徑、「在 Finder 中顯示」。按了顯示 `remote_unsupported`，什麼都不寫。
- **不能碰使用者自己的東西**：
  - Ubuntu 的 `/home/linuxbrew/.linuxbrew/bin/snip` 與 Mac 的 `/opt/homebrew/bin/snip` 不使用、不替換。
  - Ubuntu 的 systemd user unit `snip-worker.service`（若還在）不停止、不重啟、不改設定。
  - `~/.ssh/config` 不改。Mac 的真實設定資料夾不碰（第 2.2 節隔離）。
  - `~/research/rtk` 只讀（第 5 節 I01 驗證）。
- 不改產品程式，不 commit 這一輪的產出。

## 1. 機器與受測版本

| 角色 | 機器 | 怎麼到 |
|---|---|---|
| master | Mac mini `AudideMac-mini` | 本機 |
| worker | Ubuntu `audichuang-desktop`（x86_64） | `ssh ubuntu`（`~/.ssh/config` 的 `Host ubuntu`，金鑰登入） |

- 受測 SHA 沒有另外指定時，用 `origin/develop`。它必須包含遠端貼上與 worker 端變更選取（協定 5）：`grep -q 'PROTOCOL_MAX: u32 = 5' crates/remote/src/proto.rs`，不符就不開跑。
- **受測的 worker binary 怎麼被選到**：產品在對方執行 `snip serve --stdio` 時，依序找 PATH 上的 `snip`、`~/.local/bin/snip`…。Ubuntu 的非互動 ssh PATH 第一項是 `~/.local/bin`，所以本輪把受測 SHA 編出來的 `snip` 暫時放在 `~/.local/bin/snip`，產品就會走真實的 ssh 路徑選到它。開跑前那個位置必須不存在（2.3 會檢查），收尾一定要刪掉（第 6 節，I03 驗證）。**不要用 `SNIP_REMOTE_EXEC`**，那會繞過要測的 ssh 路徑。
- 受測專案：Ubuntu 上的 `~/research/rtk`。真實的 Rust 專案，有中文 README（`README_zh.md`）、200 KB 以上的原始碼（`src/hooks/init.rs`）、`.git/`、`target/`，還有 8 MB 的二進位檔 `target/release/rtk`。**它只讀不寫**。
- 其他情境放在 fixture（第 2.3 節），全部在 Ubuntu 的 `$W` 底下。

## 2. 開跑

下面的命令區塊都用 bash 執行（這台 Mac 的預設 shell 是 fish；先執行 `bash`，或用 `bash -c`）。cargo 在 `$HOME/.cargo/bin`，代理程式的 shell 不一定有這條 PATH，請自己加上。

### 2.1 第 0 步閘門：CLI 端到端

在受測 SHA 的乾淨 checkout 上執行：

```bash
export PATH=$HOME/.cargo/bin:$PATH
grep -q 'PROTOCOL_MAX: u32 = 5' crates/remote/src/proto.rs
cargo build --release -p snip-cli --locked
just remote-e2e-ssh ubuntu
```

最後一行必須是 `== N passed, 0 failed`，結束碼 0。這一步確認編譯、ssh 金鑰登入、worker 啟動、複製與貼上的位元組比對都正常。**沒過就不開始點 GUI**，計分表全部寫 `not-run`，證據欄寫「第 0 步閘門失敗」並附上輸出。

### 2.2 本輪目錄與 Mac 端建置

```bash
export SHA=$(git rev-parse --short HEAD)
export RUN="$HOME/snip-sync-ui-runs/$(date +%Y-%m-%d)-$SHA-remote"
export W="/home/audichuang/snip-ui-run/$SHA"     # Ubuntu 上的本輪目錄
mkdir -p "$RUN"
cargo build -p snip-desktop-native -p snip-cli --locked
shasum -a 256 target/debug/snip-desktop-native target/debug/snip
export SNIP="$PWD/target/debug/snip"
```

設定資料夾隔離，不碰真實的最近開啟與遠端紀錄：

```bash
export SNIP_CONFIG_DIR="$RUN/master-config"      # GUI 與 CLI master 共用
mkdir -p "$SNIP_CONFIG_DIR"
REAL="$HOME/Library/Application Support/com.audichuang.snip-sync"
ls -la "$REAL" > "$RUN/real-config-before.txt" 2>&1
shasum -a 256 "$REAL"/* > "$RUN/real-config-before.sha" 2>/dev/null || true
grep -qE '^Host( .*)? ubuntu( |$)' ~/.ssh/config || { echo "~/.ssh/config 沒有 Host ubuntu" >&2; exit 1; }
```

Mac 端的本機 fixture：

```bash
G="git -c user.name=t -c user.email=t@t"
# 本機工作區：App 啟動時開它；L 段把遠端內容貼到這裡。
mkdir -p "$RUN/local-ws" && (cd "$RUN/local-ws" && git init -q && echo local > local.txt && git add . && $G commit -qm init)
# 檔案模式的貼上來源（P 段）。
mkdir -p "$RUN/paste-src/sub"
printf 'pasted\tTab 中文 ✓ 🦀\n' > "$RUN/paste-src/a.txt"
printf 'crlf\r\nsecond\r\n' > "$RUN/paste-src/sub/crlf.txt"
printf 'no newline at the end' > "$RUN/paste-src/sub/noeol.txt"
printf 'new file\n' > "$RUN/paste-src/new.txt"
# 超過 8 MiB 的貼上來源（P11）：30 個 300 KB 的檔案。
mkdir -p "$RUN/paste-big" && for i in $(seq 1 30); do head -c 300000 /dev/zero | tr '\000' "$(printf "\\x$(printf %x $((97 + i % 26)))")" > "$RUN/paste-big/f$i.txt"; done
# 指向 .git 的惡意 payload（P07）。
printf '// FILE: target/.git/hooks/pre-commit\n#!/bin/sh\necho owned\n' > "$RUN/git-payload.txt"
```

### 2.3 Ubuntu 端：編譯、放置受測 binary、fixture

```bash
# 兩個名稱都要查：`test -e` 對懸空的 symlink 是假，必須再加 `-L`。
# snip.uirun-$SHA 是本輪 S11 專用的備份名，開跑前也要不存在。
ssh ubuntu '[ ! -e ~/.local/bin/snip ] && [ ! -L ~/.local/bin/snip ] && [ ! -e ~/.local/bin/snip.uirun-'"$SHA"' ] && [ ! -L ~/.local/bin/snip.uirun-'"$SHA"' ]' || { echo "~/.local/bin/snip 或本輪備份名已存在，不開跑" >&2; exit 1; }
git archive HEAD | ssh ubuntu "rm -rf '$W' && mkdir -p '$W/src' && tar -x -C '$W/src'"
ssh ubuntu "cd '$W/src' && export PATH=\$HOME/.cargo/bin:\$PATH && cargo build --release -p snip-cli --locked 2>&1 | tail -1"
ssh ubuntu "mkdir -p ~/.local/bin && cp '$W/src/target/release/snip' ~/.local/bin/snip && sh -c 'command -v snip' && snip --version"
ssh ubuntu 'sha256sum ~/.local/bin/snip' > "$RUN/snip-installed.sha"   # 收尾比對用：只刪本輪放的 binary
```

最後一行必須印出 `/home/audichuang/.local/bin/snip` 和受測版本。印出 linuxbrew 的路徑就停下：產品不會選到受測 binary。

Ubuntu 端 fixture（`$WS` 是 worker 上的 snip，下面的 oracle 都用它）：

```bash
ssh ubuntu "W='$W' bash -s" <<'EOF'
set -euo pipefail
G="git -c user.name=t -c user.email=t@t"
g_init() { $G init -q -b main "$1"; }
cd "$W"
echo top-secret-c0ffee > secret.txt            # 在所有工作區之外

# edge：檔名、大小、編碼、symlink 的邊界
mkdir -p edge/src/deep edge/nested/.git edge/manydir edge/a/b/c/d/e
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
ln -s "$W/secret.txt" escape.txt
ln -s /etc escape-dir
ln -s "$W/edge/src" inner-link
echo 'invalid name' > "$(printf 'bad\377name.txt')"
echo 'hidden' > .hidden
cd "$W"

# gitws：多 repo（alpha 有四種變更、beta 乾淨、一個空 .git）
mkdir -p gitws/plain gitws/broken/.git
echo plain > gitws/plain/file.txt
g_init gitws/alpha
printf 'commit 1 a\n' > gitws/alpha/a.txt
(cd gitws/alpha && $G add a.txt && $G commit -q -m "first commit")
printf 'commit 2 a\n' > gitws/alpha/a.txt
printf 'commit 2 b\n' > gitws/alpha/b.txt
mkdir -p gitws/alpha/dir && printf 'in dir\n' > gitws/alpha/dir/c.txt
(cd gitws/alpha && $G add -A && $G commit -q -m "second commit")
printf 'commit 2 a modified\n' > gitws/alpha/a.txt
printf 'new file content\n' > gitws/alpha/new.txt
printf 'staged file content\n' > gitws/alpha/staged.txt
printf 'dir change\n' > gitws/alpha/dir/c.txt
(cd gitws/alpha && $G add staged.txt)
g_init gitws/beta
printf 'beta content\n' > gitws/beta/b.txt
(cd gitws/beta && $G add b.txt && $G commit -q -m "beta commit")

# brokenws：像從另一台機器同步來的資料夾。一個正常 repo、
# 三個 .git 指向不存在路徑的 worktree、一個空 .git。
mkdir -p brokenws
g_init brokenws/good
printf 'good\n' > brokenws/good/g.txt
(cd brokenws/good && $G add . && $G commit -q -m "good commit")
printf 'good dirty\n' > brokenws/good/g.txt
for n in 1 2 3; do
  mkdir -p "brokenws/wt-gone-$n"
  echo "gitdir: /Users/someone/GoogleDrive/cat/main/.git/worktrees/wt-gone-$n" > "brokenws/wt-gone-$n/.git"
  echo "kept $n" > "brokenws/wt-gone-$n/kept.txt"
done
mkdir -p brokenws/empty-git/.git

# plainws 與 outer/inner：不是 repo
mkdir -p plainws outer/inner
echo 'plain text' > plainws/hello.txt
g_init outer
printf 'outer committed\n' > outer/committed.txt
(cd outer && $G add committed.txt && $G commit -q -m "outer commit")
printf 'outer dirty\n' > outer/outer-dirty.txt
printf 'inner file\n' > outer/inner/file.txt

# pastews：貼上的目的地
mkdir -p pastews/plain
g_init pastews/target
printf 'old\n' > pastews/target/a.txt
printf 'keep me\n' > pastews/target/keep.txt
(cd pastews/target && $G add . && $G commit -q -m "target base")
# commit 模式：commit-src 有兩個新 commit，commit-dst 停在它們的前一個
g_init pastews/commit-src
printf 'base\n' > pastews/commit-src/base.txt
(cd pastews/commit-src && $G add . && $G commit -q -m base)
git clone -q pastews/commit-src pastews/commit-dst
(cd pastews/commit-src && printf 'one\n' > a.txt && $G add . && $G commit -q -m "replay one" \
  && printf 'two\n' > a.txt && mkdir -p d && printf 'b\n' > d/b.txt && $G add . && $G commit -q -m "replay two")
EOF
```

本機 commit 模式的來源（P09）：把 `commit-src` 的 base 帶回 Mac，再加一個本機 commit。

```bash
ssh ubuntu "cd '$W/pastews/commit-src' && git bundle create - --all" > "$RUN/commit-src.bundle"
git clone -q "$RUN/commit-src.bundle" "$RUN/commit-local" && (cd "$RUN/commit-local" && git reset -q --hard HEAD~2 \
  && printf 'from mac\n' > mac.txt && git add . && git -c user.name=t -c user.email=t@t commit -qm "from mac")
```

rtk 的基準（I01 用）。定義快照函式，開跑時只執行一次：

```bash
rtk_snapshot() {
  local out="$1"
  [ -e "$out" ] && { echo "rtk_snapshot: $out 已存在，拒絕覆寫" >&2; return 1; }
  ssh ubuntu 'cd ~/research/rtk && GIT_OPTIONAL_LOCKS=0 git --no-optional-locks rev-parse HEAD && GIT_OPTIONAL_LOCKS=0 git --no-optional-locks status --porcelain=v1 -z | sha256sum && sha256sum < .git/index | cut -c1-64 && find . -newer .git/HEAD -not -path "./.git/*" | wc -l' > "$out"
}
rtk_snapshot "$RUN/rtk-before.txt"
```

**收尾時絕不可重跑這一段**，否則會覆寫基準；收尾直接照第 5 節 I01。

worker 端 oracle 的寫法：在 Ubuntu 上用同一個受測 `snip` 做同一件事，例如 `ssh ubuntu "cd '$W/gitws/alpha' && snip copy a.txt --stdout" > "$RUN/X01/oracle.txt"`。內容比較一律用 SHA-256。

### 2.4 啟動 App

同一時間只開一個 App。每次啟動都寫進新的日誌檔（`app-1.log`、`app-2.log`…），不要用 `>` 蓋掉前一次。**不設 `SNIP_REMOTE_EXEC`**。

```bash
export SNIP_NATIVE_E2E=1 SNIP_THEME=dark
./target/debug/snip-desktop-native --workspace "$RUN/local-ws" > "$RUN/app-1.log" 2>&1
```

視窗用 1080×720。S12 另外在 900×600 再看一次選單。

### 2.5 `environment.json`

至少包含：受測 SHA、Mac 兩個 binary 與 Ubuntu `~/.local/bin/snip` 的 SHA-256、第 0 步閘門的結果行、`$RUN`、`$W`、每次 App 啟動的命令和日誌路徑、視窗邏輯尺寸與 `[APP:VIEWPORT]`、`backingScaleFactor`。

## 3. 怎麼點、怎麼判

點擊照 [real-ui-operator-protocol.md 第 3 節](real-ui-operator-protocol.md) 的七步做：從這一次的日誌取最新的 `CTRL_BOUNDS`，確認沒有更晚的 `CTRL_GONE`，換算 Retina 座標，點下去之後要有新的日誌行。判定只有 `pass`、`ui-defect`、`fail`、`not-run` 四種。

遠端工作區多出來的規則：

1. **用索引編號的 ID**。`remote-host:<ix>`、`remote-folder:<n>`、`remote-recent:<n>` 用的是清單裡的序號，不是名稱。點之前先截圖確認那一列顯示的名稱；點了之後，用 `[APP:REMOTE_HOST_LISTED: … path=<路徑>]` 或 `[APP:REMOTE_OPENED: <主機> ▸ <名稱> …]` 確認點到的是哪一個。不對就判 `fail`，證據欄寫 `identity-fail`。
2. **專案樹的兩套列家族**（和本機一樣，依工作區形狀決定）：
   - **單一 repo 工作區**（工作區資料夾本身就是 repo，例如 `rtk`）：頂層是 `repo-row:<名稱>`，其下是 `tree-row:<相對路徑>`、`tree-chevron:`、`tree-view-more:`、`tree-retry:`、`tree-invalid:`；日誌 `TREE_PAGE`、`TREE_FILE_SELECTED`。
   - **多 repo 或純資料夾工作區**（例如 `edge`、`gitws`、`brokenws`、`plainws`）：工作區自己的樹用 `ws-tree-row:`、`ws-tree-chevron:`、`ws-tree-view-more:`、`ws-tree-retry:`、`ws-tree-invalid:`；日誌 `WS_TREE_PAGE`、`WS_FILE_SELECTED`。探索到的 repo 顯示為 `repo-row:<名稱>`。
3. **沒有日誌 tag 的訊息**：`remote_unsupported`（「遠端工作區還不支援這個操作」）顯示在狀態列。這種格子的通過線是：截圖裡狀態列有那段文字、剪貼簿 sentinel 的 SHA 不變、worker 上沒有新檔案、1 秒內沒有新的 `COPY_*` 或 `PASTE_*` 行。**不要因為少一行日誌就判 `fail`。**
4. **預覽成功與失敗**：成功是 `PREVIEW_LOADING: <path>` 接著 `PREVIEW_LOADED: <path>`。拒絕、二進位、過大時只有 `PREVIEW_LOADING`，沒有 `PREVIEW_LOADED`，預覽區顯示錯誤文字，下一次點擊 App 仍有反應。
5. **複製的通過線**：有 `COPY_PREP` 與 `COPY_DONE: copied=N`（commit 是 `COPY_COMMITS_PREP` / `COPY_COMMITS_DONE: commits=N`）；`pbpaste | shasum -a 256` 等於 worker 端 oracle 的 SHA-256。
6. **貼上的通過線**：`PASTE_PREVIEW` → 面板 → 點 `btn-apply` → `PASTE_APPLYING` → `PASTE_DONE`；之後 worker 上每個寫入的檔案 SHA-256 等於「在 Ubuntu 上用受測 `snip paste --stdin` 對同一個 payload 貼到一份同樣的目的地副本」的結果，沒勾選的列不變。拒絕格：有 `PASTE_ERR`、`PASTE_STALE_DETECTED` 或 `PASTE_PLAN_REFUSED`，worker 上的檔案不變。
7. **文字輸入**：先點 `remote-path-input` 的 bounds，再用鍵盤輸入，截圖確認輸入框裡的字。Cmd+V 前要確認焦點在哪裡：焦點在輸入框是貼文字，不在是 App 的貼上。
8. **worker 端 oracle** 一律用 `ssh ubuntu` 讀，存成檔再算 SHA-256。不要拿 App 自己的輸出去驗證 App。
9. **tooltip**：滑鼠移到目標列 bounds 中心，等約 1 秒再截圖。

剪貼簿 sentinel：每個拒絕格和每個複製格開始前，`printf 'sentinel-%s' "$RANDOM" | pbcopy`，再用 `pbpaste | shasum -a 256` 記下雜湊。

### 3.1 控制項與日誌

| 位置 | ID |
|---|---|
| 工作區選單 | `btn-workspace-menu` |
| ssh 主機 | `remote-host:<ix>` |
| 資料夾瀏覽 | `remote-path`（目前路徑）、`remote-up`（上一層）、`remote-folder:<n>`（進入）、`btn-remote-open-here`（開啟這個資料夾） |
| 輸入路徑 | `remote-path-input`、`btn-remote-open` |
| 最近開啟 | `remote-recent:<n>`（在選單最上方的「最近開啟」，本機項目之後） |
| 重開失敗 | `workspace-closed-remote-error`（空工作區畫面上的紅字） |
| 工具列 | `btn-paste`、`btn-refresh`；左軌 `rail-project`、`rail-changes`、`rail-log` |
| 右鍵選單 | `menu-item:copy-files`（複製）、`menu-item:copy-path` |
| Changes | `change-row:<path>`、`change-header:<group>`、`change-dir:<key>`、`change-unreadable`（無法讀取的儲存庫節點）、`change-unreadable-toggle`、`changes-empty` |
| Log | `log-subject:<ix>`、`log-error`、`log-failed-feeds`（讀不到的 repo 橫幅）、`log-empty` |
| 貼上面板 | `paste-panel`、`paste-items`、`paste-row:<ix>:<path>`、`paste-include:<ix>:<path>`、`paste-overwrite:<ix>:<path>`、`paste-map:<prefix>`、`paste-map-pick:<…>`、`paste-commit:<n>`、`btn-apply`、`btn-cancel` |

| 日誌 | 意思 |
|---|---|
| `[APP:REMOTE_HOST_LISTED: host=<主機> folders=N path=<路徑>]` | 列出主機上某個資料夾的子資料夾 |
| `[APP:REMOTE_HOST_FAILED: host=<主機> <訊息>]` | 連不上或列不出 |
| `[APP:REMOTE_OPENED: <主機> ▸ <名稱> generation=N]` | 開啟遠端工作區 |
| `[APP:REMOTE_OPEN_FAILED: …]`、`[APP:REMOTE_REOPEN: …]` | 開啟失敗；啟動時重新連線 |
| `[APP:REMOTE_SCAN_FAILED: …]` | 掃描儲存庫失敗 |
| `[APP:CHANGES_EMPTY: state=…]`、`[APP:LOG_EMPTY: state=…]` | 空狀態（`clean`、`clean_partial`、`no_repository`、`scan_failed`、`failed` 等） |
| `[APP:E2E_REPO: name=<名稱> ok=true/false …]` | 儲存庫掃描結果 |
| `[APP:COPY_PREP: files=N]`、`[APP:COPY_DONE: copied=N]`、`[APP:COPY_COMMITS_DONE: commits=N]` | 複製 |
| `[APP:PASTE_PREVIEW …]`、`[APP:PASTE_APPLYING]`、`[APP:PASTE_DONE …]`、`[APP:PASTE_ERR: …]`、`[APP:PASTE_STALE_DETECTED …]` | 貼上 |

## 4. 測案

每格都要截圖（點擊前、點擊後各一張），存在 `$RUN/<ID>/`，再加一份 `action.json`，內容是 bounds、scale、算出的螢幕點和新的日誌行。寫入任何檔案前先 `mkdir -p "$RUN/<ID>"`。

### 4.1 ssh 主機與選專案（S01–S12，依序做）

| ID | 操作 | 通過線 |
|---|---|---|
| S01 | 點 `btn-workspace-menu` | 遠端區列出 `~/.ssh/config` 的主機（不含萬用字元），其中有 `ubuntu`；順序和 `snip remote hosts` 的輸出一致 |
| S02 | 點 `ubuntu` 那一列的 `remote-host:<ix>` | `REMOTE_HOST_LISTED: host=ubuntu … path=/home/audichuang`；`remote-folder:*` 的名稱集合等於 `ssh ubuntu 'cd ~ && ls -A'` 的資料夾去掉以 `.` 開頭的；看得到 `btn-remote-open-here`；沒有 `REMOTE_OPENED` |
| S03 | 依序點進 `snip-ui-run`、`<SHA>` | 每一次都是新的 `REMOTE_HOST_LISTED … path=<那一層>`，**不是**開啟工作區；`remote-path` 顯示目前路徑；`remote-path-input` 被填成目前路徑 |
| S04 | 點 `remote-up` | 回到上一層（`path=/home/audichuang/snip-ui-run`）；再點進 `<SHA>` |
| S05 | 點進 `gitws`，點 `btn-remote-open-here` | `REMOTE_OPENED: ubuntu ▸ gitws`；左上角麵包屑是 `ubuntu ▸ gitws`；專案樹列出 alpha、beta、broken、plain |
| S06 | 開選單 | 最上方「最近開啟」有 `ubuntu ▸ gitws`（`remote-recent:<n>`），排在本機的 `local-ws` 之後；第二行是完整路徑 |
| S07 | 從最近開啟點 `local-ws`，再點 `remote-recent:<n>`（gitws） | 先回到本機（麵包屑沒有 `ubuntu ▸`），再一次點擊就回到 `ubuntu ▸ gitws`，不用重新逐層選 |
| S08 | 在 `remote-path-input` 輸入 `~/snip-ui-run/<SHA>/edge`，點 `btn-remote-open` | `REMOTE_OPENED: ubuntu ▸ edge`；`~` 展開成對方的家目錄 |
| S09 | 在 `remote-path-input` 輸入 `~/snip-ui-run/<SHA>/no-such`，點 `btn-remote-open` | 失敗格：選單裡紅字說資料夾不存在；目前的工作區不變（仍是 edge） |
| S10 | Cmd+Q，等 exit code 0，用同一個 `SNIP_CONFIG_DIR` **不帶 `--workspace`** 重新啟動（寫進新的 `app-N.log`） | `REMOTE_REOPEN` 之後是 `REMOTE_OPENED: ubuntu ▸ edge`；啟動過程畫面不卡（重新連線在背景） |
| S11 | 對方沒有受測版本：`ssh ubuntu "mv ~/.local/bin/snip ~/.local/bin/snip.uirun-$SHA"`（本輪專屬備份名，不碰使用者既有的任何備份），在 App 選單點 `ubuntu`，打開 `pastews/plain`，按 Cmd+V 貼上任一 payload；做完立刻 `ssh ubuntu "mv ~/.local/bin/snip.uirun-$SHA ~/.local/bin/snip"`，並確認還原成功 | 這時產品會選到 linuxbrew 上較舊的 `snip`：瀏覽仍可用；貼上顯示「對方的 snip 版本太舊」之類的明確訊息（不是連線錯誤、不是空白），worker 上沒有新檔案。若 linuxbrew 沒有 `snip`，訊息要說對方沒有安裝 snip |
| S12 | 把視窗調成 900×600（System Events 設成 900×632），開選單並點進一層資料夾 | `remote-path`、`remote-up`、`btn-remote-open-here`、`remote-path-input`、`btn-remote-open` 的 bounds 都 `w,h ≥ 1` 且在內容區裡；長路徑被截斷時 hover 顯示完整路徑；做完調回 1080×720 |

### 4.2 瀏覽真實專案 rtk（R01–R08）

先在選單逐層選到 `/home/audichuang/research/rtk`，點 `btn-remote-open-here`。

| ID | 操作 | 通過線 |
|---|---|---|
| R01 | 讀根目錄 | 頂層是 `repo-row:rtk`；其下 `tree-row:*` 集合等於 `ssh ubuntu 'ls -A ~/research/rtk'` 去掉 `.git` |
| R02 | 點 `tree-row:src` 展開 | `TREE_PAGE: rel=src … children=N`，N 等於 `ls -A src \| wc -l` |
| R03 | 點 `tree-row:src/main.rs` | `PREVIEW_LOADED: src/main.rs`；預覽前 20 行和 `ssh ubuntu 'head -20 ~/research/rtk/src/main.rs'` 相同 |
| R04 | 點 `tree-row:README_zh.md` | `PREVIEW_LOADED`；中文正常顯示 |
| R05 | 展開 `src/hooks`，點 `init.rs`（238 KB），捲到最後 | 最後一行和 `tail -1` 一致；捲動不卡 |
| R06 | 展開 `target/release`，點 `rtk`（8 MB 二進位） | 失敗格：顯示二進位或超過 1 MiB；5 秒內可以點下一列 |
| R07 | 點 `rail-changes`、`rail-log` | Changes `state=clean`；Log 第一列的 subject 與 SHA 等於 `ssh ubuntu 'git -C ~/research/rtk log -1 --format="%h %s"'` |
| R08 | 把 `src` 收合再展開 | 第二次也有 `TREE_PAGE: rel=src`，清單和 R02 相同 |

### 4.3 邊界案例 edge（E01–E12）

從最近開啟或選單打開 `edge`。

| ID | 操作 | 通過線 |
|---|---|---|
| E01 | 依序展開 `src`、`deep`，點 `中文 有空白.txt` | `PREVIEW_LOADED`；顯示 `深層 檔案 ✓ 🦀` |
| E02 | 點 `empty.txt` | `PREVIEW_LOADED`；空白預覽，不是錯誤，也不是上一個檔案 |
| E03 | 點 `crlf.txt` | 兩行 `line1`、`line2`，沒有 `^M` 或多出空行 |
| E04 | 點 `exact-1MiB.txt` | `PREVIEW_LOADED` |
| E05 | 點 `over-1MiB.txt` | 失敗格：錯誤文字提到超過 1 MiB |
| E06 | 點 `blob.bin`、`latin.txt` | 失敗格：二進位或非 UTF-8 |
| E07 | 展開 `manydir`，反覆點最新的 `ws-tree-view-more:manydir`，直到沒有更多列 | 列數總和等於 1200；每次點擊都有新的 `WS_TREE_PAGE` |
| E08 | 點 `escape.txt`、`escape-dir` | 失敗格：錯誤說路徑離開了工作區；`top-secret-c0ffee` 不在截圖也不在 `app-*.log` |
| E09 | 點 `inner-link` | 當資料夾展開，列出 `deep/` 與 `main.rs` |
| E10 | 找到非 UTF-8 名稱那一列 | ID 是 `ws-tree-invalid:<suffix>`，名稱用替代字元；點它有 `TREE_ROW_REFUSED: not-utf8` |
| E11 | 依序展開 `a/b/c/d/e`，點 `leaf.txt` | 每層一行 `WS_TREE_PAGE`；顯示 `deepest` |
| E12 | 看 `nested` 和 `.hidden` | `nested`（空 `.git`）顯示為 `repo-row:nested` 錯誤列，不是可展開的資料夾；`.hidden` 照本機規則顯示 |

### 4.4 多 repo 加壞掉的 git：brokenws（B01–B07）

打開 `brokenws`。對照：`"$SNIP" remote repos ubuntu "$W/brokenws"` 會列出 `good` 一列正常、`wt-gone-1`、`wt-gone-2`、`wt-gone-3`、`empty-git` 四列 `error:`。

| ID | 操作 | 通過線 |
|---|---|---|
| B01 | 點 `rail-changes` | `good` 的 `g.txt` 列在最上面；清單最下面一列是 `change-unreadable`「⚠ 無法讀取的儲存庫 (4)」，預設收合（看不到逐個錯誤列）；數字等於對照命令的 `error:` 列數 |
| B02 | 點 `change-unreadable-toggle` | 展開後逐一列出每個 repo 的名稱與原因（`not a git repository` 之類），縮排在節點底下；再點一次收合 |
| B03 | 點 `good` 的 `g.txt` | 預覽顯示 diff，內容是 `good dirty`；壞掉的 repo 不影響 |
| B04 | 點 `rail-log` | `log-failed-feeds` 橫幅寫「N 個儲存庫無法讀取：」後面只列前 2 個名稱接「…」；hover 橫幅，tooltip 列出全部 N 個；Log 列表有 `good commit` |
| B05 | 點 `rail-project`，展開 `wt-gone-1`，點 `kept.txt` | `PREVIEW_LOADED`，顯示 `kept 1`：壞掉的 worktree 檔案照樣能瀏覽 |
| B06 | 在 B01 的 `change-unreadable` 上右鍵 | 沒有會失敗的「複製」：選單的複製是停用的，或複製後 `COPY_REFUSED`／狀態列說明沒有可複製的內容；剪貼簿 sentinel 不變 |
| B07 | `ssh ubuntu "echo fixed > '$W/brokenws/good/new.txt'"`，點 `btn-refresh` | `good` 多一列 `new.txt`；無法讀取的節點數字不變，仍收合 |

### 4.5 遠端唯讀 Git 檢視：gitws（G01–G10）

打開 `gitws`。

| ID | 操作 | 通過線 |
|---|---|---|
| G01 | 點 `rail-changes` | alpha 的變更與 `git -C alpha status --porcelain=v2` 一致：`a.txt`、`dir/c.txt` 改動，`staged.txt` 暫存，`new.txt` 未追蹤；beta 乾淨不列；`broken` 是一列錯誤（只有一個時不收合，留在最上方） |
| G02 | 點 `a.txt` | 右側 diff 包含 `commit 2 a modified` |
| G03 | 點 `rail-log`；用 Repository 篩選只選 alpha | 預設是 alpha 與 beta 合併的 log；篩選後只剩 alpha |
| G04 | 點 alpha 的 HEAD commit | 變更檔案清單等於 `git diff-tree --no-commit-id --name-only -r HEAD` |
| G05 | 點其中一個檔案 | 顯示該 commit 的 diff |
| G06 | 打開分支選擇器 | 列出 `main` |
| G07 | 切到 commit 樹瀏覽 | 可展開目錄、看 blob 內容 |
| G08 | 打開 `plainws`，看 Changes 與 Log | `CHANGES_EMPTY: state=no_repository`、`LOG_EMPTY: state=no_repository` |
| G09 | 打開 `outer/inner` | `no_repository`；絕不出現父 repo 的 `outer-dirty.txt` |
| G10 | `ssh ubuntu "echo beta-change >> '$W/gitws/beta/b.txt'"`，點 `btn-refresh` | beta 出現 `b.txt` 改動 |

### 4.6 遠端複製和本機一樣（X01–X10）

仍在 `gitws`。每格開始前寫 sentinel；oracle 在 Ubuntu 上執行，例如 `ssh ubuntu "cd '$W/gitws/alpha' && snip copy a.txt --stdout" | shasum -a 256`。

**先弄清楚 App 送出的是什麼**（對照程式碼 `menu.rs` 的 `change_row_targets` 與 `project_targets`）：Changes 的列（檔案、資料夾、repo、群組標頭）複製的是 **git 變更匯出**，payload 路徑帶變更標籤（`// file: [MODIFIED] a.txt`），來源是 working/staged；repo 列與群組標頭只複製**那一個群組**的列。專案樹的選取複製的是**檔案模式**（無變更標籤），root 是選取所在的 repo（`alpha`），路徑以 alpha 為根。所以 oracle 有兩種形狀：變更匯出比對 `snip copy --working/--staged --stdout` 輸出裡的**對應區塊**（一個區塊 = 一行 `// file: …` 到下個區塊前，含檔尾空行）；檔案模式比對整份 payload。

| ID | 操作 | oracle（在 `$W/gitws/alpha` 執行） |
|---|---|---|
| X01 | Changes 的 `a.txt`（未暫存）右鍵 →「複製」 | `snip copy --working --stdout` 輸出裡 `// file: [MODIFIED] a.txt` 的那一個區塊（不是 `snip copy a.txt`：純檔案複製沒有變更標籤） |
| X02 | Changes 的 `staged.txt`（暫存列）右鍵 →「複製」 | `snip copy --staged --stdout` 裡 `// file: [NEW] staged.txt` 的區塊（這份輸出只有它） |
| X03 | Changes 的 `dir` 資料夾列右鍵 →「複製」 | `snip copy --working --stdout` 裡 `// file: [MODIFIED] dir/c.txt` 的區塊（資料夾列複製該 repo 該群組底下的所有變更，這裡只有一個） |
| X04 | Changes 的 **Unstaged 群組裡的 alpha repo 列**右鍵 →「複製」 | repo 列只複製一個群組：`snip copy --working --stdout` 裡 alpha 的未暫存區塊（`[MODIFIED] a.txt`、`[MODIFIED] dir/c.txt`、`[NEW] new.txt`），**不含** `staged.txt` 的暫存區塊 |
| X05 | Changes 的 Unstaged 群組標頭右鍵 →「複製」 | 同 X04 的未暫存區塊整份（gitws 只有 alpha 有變更）；`COPY_DONE: copied=` 等於群組裡的列數 |
| X06 | 選 Changes 的 `new.txt`，按 Cmd+C | `snip copy --working --stdout` 裡 `// file: [NEW] new.txt` 的區塊 |
| X07 | 專案樹展開 alpha，Cmd 點選 `a.txt` 與 `b.txt`，右鍵 →「複製」 | 在 `$W/gitws/alpha` 執行 `snip copy a.txt b.txt --stdout`（選取以 alpha 為根，不是 `alpha/a.txt`） |
| X08 | 專案樹展開 alpha，右鍵 `dir` 子資料夾 →「複製」 | 在 `$W/gitws/alpha` 執行 `snip copy dir --stdout`（整個 alpha 是 repo 列，選單只有「複製路徑」，選不得，改用子資料夾） |
| X09 | Log 選 alpha 的 HEAD commit，在變更檔案清單對 `b.txt` 右鍵 →「複製」 | `snip copy --commit HEAD --stdout` 裡的 `b.txt`；內容是 commit 時的 `commit 2 b`，不是工作樹 |
| X10 | Log 選 alpha 的兩個 commit，點複製 commit | `snip copy --commits -n 2 --stdout`；`pbpaste` 第一行是 commit 模式的標記 |

每格都要：`COPY_DONE`（或 `COPY_COMMITS_DONE`），`pbpaste | shasum -a 256` 等於 oracle。

### 4.7 遠端貼上和本機一樣（P01–P12）

每格先建一份目的地副本給 oracle 用：`ssh ubuntu "rm -rf '$W/oracle' && cp -a '$W/pastews' '$W/oracle'"`，oracle 是 `ssh ubuntu "cd '$W/oracle/<同一個目的地>' && snip paste --apply <同樣的旗標> --stdin" < payload`，再比較 `$W/pastews/…` 與 `$W/oracle/…` 每個檔案的 SHA-256。

**正規化是刻意的**（spec 第 1 節）：檔案模式不保留檔尾換行與前後空行，CRLF 變成 LF。所以貼上的檔案**不會**逐位元組等於原始檔；通過線一律是「與獨立 CLI 往返的結果相同」——在 Ubuntu 上 `(cd <同一個來源> && snip copy … --stdout)` 產生同一份 payload，`snip paste --apply --stdin` 貼進 oracle 副本，兩邊的貼上結果逐檔案比 SHA-256。

| ID | 操作 | 通過線 |
|---|---|---|
| P01 | `(cd "$RUN/paste-src" && "$SNIP" copy . --stdout) > "$RUN/p-files.txt"`（`selection_from_paths` 以 cwd 解析路徑，必須在來源資料夾裡跑；`--repo` 只管引擎根）；命令成功（`$?` 為 0）才 `pbcopy < "$RUN/p-files.txt"`；打開 `pastews/plain`，按 Cmd+V | `PASTE_PREVIEW`；面板列出 `a.txt`、`new.txt`、`sub/crlf.txt`、`sub/noeol.txt`，全部是新增 |
| P02 | 點 `btn-apply` | `PASTE_DONE`；四個檔案與「在 Ubuntu 上對 paste-src 做同一個 `snip copy` 再 `snip paste` 進 oracle 副本」的結果逐檔相同。接受的正規化（spec 第 1 節）：`sub/crlf.txt` 變 LF、`sub/noeol.txt` 的檔尾換行狀態不保留——所以這兩個檔**不必**逐位元組等於原始檔，與 CLI 往返一致即通過 |
| P03 | 打開 `pastews/target`，同一個 payload 再按 Cmd+V | `a.txt` 標示為已存在、需要確認覆寫（`paste-overwrite`），其他是新增 |
| P04 | 取消勾選 `new.txt`（`paste-include`），勾選 `a.txt` 的覆寫，點 `btn-apply` | `a.txt` 被覆寫、`new.txt` 不存在、`keep.txt` 不變；與 oracle（`--overwrite`，事後刪掉 oracle 的 new.txt）相同 |
| P05 | 再按一次 Cmd+V，預覽出現後執行 `ssh ubuntu "echo changed-outside > '$W/pastews/target/a.txt'"`，再勾選覆寫、點 `btn-apply` | 拒絕：`PASTE_STALE_DETECTED` 或畫面說目的地已在外部修改；worker 上 `a.txt` 是 `changed-outside` |
| P06 | 做一個會產生前綴選擇的 payload（P01 的 payload 帶 `clipcode-root`，單一根，不會有前綴選項）：`sed -e '/^\/\/ clipcode-root:/d' -e 's|^// file: |// file: src/|' "$RUN/p-files.txt" > "$RUN/p-prefixed.txt" && pbcopy < "$RUN/p-prefixed.txt"`——沒有 `clipcode-root` 中繼資料、每個路徑都帶 `src/`，面板才會出現 `paste-map:src`；打開 `pastews`（多個資料夾），Cmd+V | `PASTE_PREVIEW`；面板出現 `paste-map:src` 要求為前綴選目的地，兩種選擇各做一次（先建兩份 oracle 副本）：**(a) 選 `target`**（剝掉前綴）：`btn-apply` 後寫入 `target/a.txt`、`target/sub/crlf.txt` 等；oracle 是把 payload 路徑的 `src/` 去掉（`sed 's|^// file: src/|// file: |'`）後用 `snip paste --apply --stdin` 貼進 oracle 副本的 `target`——本機 `snip paste` 沒有 `--in`，目的地用 `--repo` 指定。**(b) 選保持 `src/` 於原位（keep relative）**：寫入 `pastews/src/…`；oracle 是 p-prefixed.txt 原樣貼進 oracle 副本的 `pastews` 根（`--repo` 指向 oracle 的 `pastews`） |
| P07 | `pbcopy < "$RUN/git-payload.txt"`；打開 `pastews`，Cmd+V，若可套用就點 `btn-apply` | `target/.git/hooks/pre-commit` 那一列被拒絕或標成不可寫；`ssh ubuntu "test ! -e '$W/pastews/target/.git/hooks/pre-commit'"` 成立 |
| P08 | X10 的方式從遠端 `pastews/commit-src` 複製兩個 commit；打開 `pastews/commit-dst`，Cmd+V，`btn-apply` | `PASTE_DONE`；`git -C commit-dst log -2 --format=%s` 是 `replay two`、`replay one`，樹與 oracle（在 oracle 副本的 commit-dst 執行 `snip paste --apply --stdin`）相同 |
| P09 | `"$SNIP" copy --repo "$RUN/commit-local" --commits -n 1 --stdout \| pbcopy`；打開一份新的 `commit-dst` 副本（`ssh ubuntu "cd '$W/pastews' && rm -rf commit-dst2 && git clone -q commit-src commit-dst2 && git -C commit-dst2 reset -q --hard HEAD~2"`），Cmd+V，`btn-apply` | 本機的 commit 重播到遠端：`git -C commit-dst2 log -1 --format=%s` 是 `from mac`，`mac.txt` 內容是 `from mac` |
| P10 | 遠端貼回本機：在 `edge` 對 `src` 資料夾右鍵 →「複製」；打開本機 `local-ws`，Cmd+V，`btn-apply` | `local-ws/src/main.rs` 與 `local-ws/src/deep/中文 有空白.txt` 的 SHA-256 等於「Ubuntu 上 `cd '$W/edge' && snip copy src --stdout` 再 `snip paste --apply --stdin` 貼進一份副本」的結果（與 worker 上的原檔比會差正規化：檔尾換行不保留，spec 第 1 節） |
| P11 | `(cd "$RUN/paste-big" && "$SNIP" copy . --stdout) > "$RUN/p-big.txt"`，命令成功才 `pbcopy < "$RUN/p-big.txt"`（不要直接管線進 pbcopy：複製失敗也會蓋掉剪貼簿）；打開 `pastews/plain`，Cmd+V，`btn-apply` | `PASTE_DONE`；30 個檔案與本機來源相同 |
| P12 | 遠端到遠端：在 `gitws` 複製 alpha 的 `dir` 資料夾（X08）；打開 `pastews/plain`，Cmd+V，`btn-apply` | `plain/dir/` 下的檔案與獨立 CLI 往返的結果相同（在 Ubuntu 上 `cd '$W/gitws/alpha' && snip copy dir --stdout`，再 `snip paste --apply --stdin` 貼進 oracle 副本；正規化照 spec 第 1 節） |

### 4.8 拒絕的操作（N01–N02）

| ID | 操作 | 通過線 |
|---|---|---|
| N01 | 在遠端工作區用「加入儲存庫路徑」 | 規則 3：狀態列顯示 `remote_unsupported` 文字；沒有新的 repo |
| N02 | 在遠端檔案列右鍵找「在 Finder 中顯示」 | 沒有這個項目，或按了顯示 `remote_unsupported`；沒有開啟 Finder |

### 4.9 即時變化與連線中斷（L01–L05）

| ID | 操作 | 通過線 |
|---|---|---|
| L01 | 在 `edge`：`ssh ubuntu "echo fresh-1 > '$W/edge/new.txt'"`，點 `btn-refresh` | 出現 `ws-tree-row:new.txt`，預覽是 `fresh-1` |
| L02 | `ssh ubuntu "echo fresh-2 > '$W/edge/new.txt'"`，點別的檔案再點回 `new.txt` | 顯示 `fresh-2` |
| L03 | `ssh ubuntu "rm '$W/edge/new.txt'"`，點 `btn-refresh` | 重建後的根目錄沒有 `new.txt` |
| L04 | 中斷連線：`ssh ubuntu "pkill -f '/home/audichuang/.local/bin/snip serve'"`，在 App 點一個沒預覽過的檔案 | 這一次失敗或自動重連都可以，但 10 秒內一定有結果：錯誤文字，或 `PREVIEW_LOADED`；之後再點一次一定成功（App 會重新 ssh）；絕不顯示成空白或上一個檔案 |
| L05 | 貼上中斷：P01 的 payload 對 `pastews/plain` 按 Cmd+V，預覽出現後執行 L04 的 pkill，再點 `btn-apply` | 寫入成功，或者畫面說「無法確認貼上是否完成；請重新整理確認」；絕不說成功卻沒寫，也不說失敗卻寫了（用 `ssh ubuntu ls` 對照） |

### 4.10 CLI master 交叉驗證（C01–C04）

| ID | 命令 | 通過線 |
|---|---|---|
| C01 | `"$SNIP" remote hosts` | 包含 `ubuntu`，和 S01 的清單一致 |
| C02 | `"$SNIP" remote ls ubuntu "$W/gitws" alpha` | 和 App 專案樹 `alpha` 的子項目相同 |
| C03 | 對 rtk 抽 20 個 tracked 文字檔，比較 `"$SNIP" remote cat ubuntu '~/research/rtk' "<f>" \| shasum -a 256`（路徑要用引號：不加引號 `~/…` 會在 Mac 端展開成 Mac 的家目錄）和 `ssh ubuntu "sha256sum '~/research/rtk/<f>'"` | 20/20 相同 |
| C04 | App 開著 rtk 的時候，同時跑 20 個平行的 `"$SNIP" remote cat ubuntu '~/research/rtk' src/main.rs`（同樣加引號） | 20/20 正確；這段時間在 GUI 點檔案仍然能預覽 |

## 5. 完整性（決定這一輪可不可信）

| ID | 驗證 | 通過線 |
|---|---|---|
| I01 | `rtk_snapshot "$RUN/rtk-after.txt" && cmp "$RUN/rtk-before.txt" "$RUN/rtk-after.txt"` | 相同 |
| I02 | 同 2.2 重新列出 `$REAL` 並算雜湊 | 和 `real-config-before.*` 相同 |
| I03 | 第 6 節收尾後 `ssh ubuntu '[ ! -e ~/.local/bin/snip ] && [ ! -L ~/.local/bin/snip ] && [ ! -e ~/.local/bin/snip.uirun-$SHA ] && [ ! -L ~/.local/bin/snip.uirun-$SHA ] && sh -c "command -v snip"'` | `~/.local/bin/snip` 與本輪備份名都不存在（懸空 symlink 也算存在），`command -v snip` 回到 linuxbrew 的路徑 |
| I04 | `grep -c top-secret-c0ffee "$RUN"/app-*.log "$RUN"/*/action.json` | 全部是 0 |

## 6. 收尾

```bash
# 1) 本輪 worker 依 PID 收掉（argv 是 `snip serve --stdio`，不是絕對路徑），
#    並驗證已結束；沒有 PID 就略過。
ssh ubuntu "pgrep -u audichuang -f '^snip serve --stdio$'" > "$RUN/workers-at-cleanup.txt" || true
pids=$(tr '\n' ' ' < "$RUN/workers-at-cleanup.txt")
[ -n "$pids" ] && ssh ubuntu "kill $pids" || true
sleep 1
ssh ubuntu "pgrep -u audichuang -f '^snip serve --stdio$'" > "$RUN/workers-after-cleanup.txt" || true
[ ! -s "$RUN/workers-after-cleanup.txt" ] || { echo "還有 worker 殘留，收尾未完成" >&2; exit 1; }

# 2) S11 的備份若還在（S11 做到一半中斷）：先還原成 snip。
ssh ubuntu "if [ -e ~/.local/bin/snip.uirun-$SHA ] && [ ! -e ~/.local/bin/snip ]; then mv ~/.local/bin/snip.uirun-$SHA ~/.local/bin/snip; fi"

# 3) 只刪本輪放的 binary：hash 與安裝時記下的一致才刪；懸空 symlink 也先清掉。
want=$(cut -d' ' -f1 "$RUN/snip-installed.sha")
got=$(ssh ubuntu 'sha256sum ~/.local/bin/snip 2>/dev/null' | cut -d' ' -f1)
if [ -n "$got" ] && [ "$got" = "$want" ]; then
  ssh ubuntu 'rm -f ~/.local/bin/snip'
else
  echo "~/.local/bin/snip 已非本輪安裝的 binary，保留不刪" >&2
fi
ssh ubuntu "rm -f ~/.local/bin/snip.uirun-$SHA"

# 4) 本輪目錄。
ssh ubuntu "rm -rf '$W'"
```

只刪 `$W`、本輪放的 `~/.local/bin/snip`（hash 一致才刪）與本輪備份名
`~/.local/bin/snip.uirun-$SHA`。`snip.away` 或使用者自己的任何備份、
`~/research/rtk`、linuxbrew 的 `snip`、`snip-worker.service`、
`~/.ssh/config` 都不動。Mac 上的 `$RUN` 保留，裡面是證據。

## 7. 計分

`scorecard.md` 列出 S01–S12、R01–R08、E01–E12、B01–B07、G01–G10、X01–X10、P01–P12、N01–N02、L01–L05、C01–C04、I01–I04，每格一個判定，並附證據路徑。

- **閘門**：第 0 步閘門通過，而且上面全部是 `pass`，才寫「遠端工作區真實 UI 閘門關閉」。
- 有任何 `ui-defect`、`fail` 或 `not-run`，第一句就寫「遠端工作區真實 UI 閘門打開」，並列出那些 ID。
- I01、I02 或 I03 不是 `pass`，整輪結果作廢：這一輪動到了真實資料，先報告，再處理其他格子。

每個不是 `pass` 的格子先分清楚是哪一種錯，再動手：

- **規程錯**：通過線和產品的設計不符。修這份規程；產品不動。
- **工具做不到**：操作者的工具到不了驗證點。該格維持 `not-run`；在規程寫下已驗證可行的做法，或換工具。
- **產品缺陷**：從截圖和日誌確認後，先寫一個會失敗的測試，再修產品；需要的話，規程也補上對應的通過線。
