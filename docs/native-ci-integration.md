# 原生 CI／harness 整合收據

## Current Linux acceptance entrypoints (2026-09-27)

`just preflight` now also requires `just native-acceptance`. The required Linux
CI job `Native Acceptance (Linux)` runs the same Python entrypoint; the existing
Rust/frontend/Tauri, native smoke/lifecycle, audit/DTO and four-target candidate
jobs and package names remain in place. A wiring change is not a recorded CI or
product pass: current run evidence must come from the generated reports.

| Entry | Checks |
| --- | --- |
| `just native-acceptance` | Build one release binary, then all three gates below |
| `just native-ime` | Deterministic startup ordering plus all nine IME phases |
| `just native-collaboration` | Canonical fixture and all 18 real-app cases; no step filter |
| `just native-resources-short` | Medium 15-repo functional subgate; existing 20 warmup/100 measured switches and thresholds |
| `just native-resources-long` | Standard workload and unchanged long resource gate; currently fails for missing real hide/tray coverage |
| `just native-acceptance-build` | Build/freeze only; no GUI acceptance claim |

All entries accept `--output FRESH_DIRECTORY_OUTSIDE_CHECKOUT` and `--build-receipt RECEIPT`.
Without a receipt, the runner invokes a locked release Cargo build and freezes
the emitted executable. With a receipt, it verifies that build and uses its
frozen executable without rebuilding. The combined entry builds only once.

One executable interpreter is selected by `SNIP_NATIVE_PYTHON` (default
`python3`), including every driver subprocess. It is an executable name/path,
not a shell command or a list of arguments. For example, when the system Python
has Pillow but another Python on PATH does not:

```bash
SNIP_NATIVE_PYTHON=/usr/bin/python3 just native-acceptance
```

Do not globally install packages to repair a different interpreter. The Linux
CI job installs `python3-pil`, `fcitx5`, `fcitx5-pinyin`, X11/Xvfb/input tools,
D-Bus, fonts, Mesa and native build headers with apt, then explicitly invokes
`/usr/bin/python3`. A local isolated Python environment with Pillow also works.
Missing tools, Pillow, unsupported platforms, failed drivers and mismatched
inputs fail closed; every driver gets `SNIP_REQUIRE_ALL_TESTS=1`. Local native
linker configuration, when needed, remains caller-provided; the IME helper no
longer inserts a particular developer's library directory.

Evidence defaults to a unique `/tmp/snip-native-acceptance-*` directory.
An explicit output must not exist and must be outside the checkout, including
outside ignored `target/`: the fixture generator rejects worktree paths.
This is checked before any build. Fixtures and all evidence stay together in
that directory; no historical run is deleted or reused.
The runner clears inherited display/bus and Git-routing variables; existing
drivers create their own private Xvfb, HOME and D-Bus. These entries do not wrap
the drivers in the less-isolated smoke-test display helper.

The build receipt binds actual Cargo exit status/command, build environment,
HEAD and tree, dirty/staged/untracked state, source file hashes and the frozen
binary SHA-256. Source snapshots are recorded before/after building and checked
before/after each gate, including failed gates. A receipt from an old or changed
checkout is refused even when its HEAD alone looks correct. This entrypoint
issues its own receipts; an arbitrary historical binary or hand-written source
claim is not an alternative. The resource driver's existing
`sourceBuildAuthorized=false` receipt classification stays unchanged; the
orchestrator's separately observed build provides the source/build evidence.

For a single frozen source snapshot, preserve the receipt path printed by a
build-only run, then run gates into new directories:

```bash
SNIP_NATIVE_PYTHON=/usr/bin/python3 just native-acceptance-build
# Use the build-receipt.json from that fresh output, without editing the checkout:
SNIP_NATIVE_PYTHON=/usr/bin/python3 just native-ime --build-receipt /absolute/run/build-receipt.json
SNIP_NATIVE_PYTHON=/usr/bin/python3 just native-collaboration --build-receipt /absolute/run/build-receipt.json
SNIP_NATIVE_PYTHON=/usr/bin/python3 just native-resources-short --build-receipt /absolute/run/build-receipt.json
```

Each run generates its own fixtures using the existing generators. Collaboration
pins the canonical dataset hash and helper hashes; resources pin the workload
manifest hash. Driver outputs and raw logs remain alongside `acceptance.json`,
the build receipt and build-input snapshots. The CI job uploads these artifacts
on success or failure and has a 120-minute budget for compilation plus all
18 cases and 100 measured switches; actual timings still require a live run.

The short gate is **functional-short**, not standard-release, full D4, an
absolute-memory acceptance result or cross-platform UI verification. The long
entry generates the real standard workload and propagates the existing
`missing-coverage` failure until hide/tray are supported and observed. It must
pass separately for full native resource/release acceptance. This wiring does
not change release publishing or candidate-artifact promotion policy.

The remainder of this document is the historical 2026-09-26 integration receipt.

> **後續整合（2026-09-26）**：Codex 已將通過獨立審查的六個 packaging 檔由 `915ae79` 併入；下方 worker 交付時的「尚未進樹」是歷史狀態。完整 Python suite 已獨立通過 138 tests（12.620s），日誌 `/tmp/snip-integrated-harness-package-20260926.log`。建置文件的 headless 指令與舊測試數也已校正。完整 preflight、四目標 CI 與原生產品驗收仍未完成。

日期：2026-09-26。工作樹 `/home/audichuang/research/snip-sync`，分支 `feature/lightweight-git-workbench-plan`，HEAD `7ed02790b63cb8e051bf726828daa1fbfcd765ce`。base `9be8684f0b7bd556f59159c6887fd5af339de148` 仍是 HEAD 的祖先。這次沒有 commit、push 或 release。

這是基礎建設併入。來源上的 pilot 量測與監督者先前的 127 個 Python 測試、Tauri idle／1repo driver 檢查，留在來源 commit，不在這份收據裡改記成發布驗收或 D4 通過。

## 來源

不可變來源：`a4b201f1843080ca3e9be9231960d5093e83dfec`（`fix(perf): verify target executable before claiming launch metrics`）。

已包含在該 commit、本檢出不另抄的祖先：

- `ea7ab85`：workload generator 與 memory harness
- `d42c739`：native input smoke 的 CI
- `e592999`：memory pilot 與 candidate package 檢查

`docs/evidence/` 沒有複製。歷史仍在該 commit 的 `docs/evidence/`。

## 納入

腳本與測試的 blob 與 git mode（`100644` → `644`，`100755` → `755`）與來源一致。本機 umask `0002` 曾把它們寫成 `664`／`775`，已改回來源 mode。

| 路徑 | mode |
| --- | --- |
| `scripts/tests/__init__.py` | 644 |
| `scripts/tests/test_workload_generator.py` | 644 |
| `scripts/tests/test_harness_contracts.py` | 644 |
| `scripts/tests/test_bench_tauri.py` | 644 |
| `scripts/tests/test_bench_native.py` | 644 |
| `scripts/workload_generator.py` | 755 |
| `scripts/memory_harness.py` | 755 |
| `scripts/bench_native_memory.py` | 644 |
| `scripts/bench_tauri_memory.py` | 755 |
| `scripts/headless-x11.sh` | 755 |
| `scripts/smoke_native.sh` | 755 |
| `scripts/measure_tauri.sh` | 755 |
| `scripts/generate_15_repos.sh` | 755 |
| `scripts/measure_memory.sh` | 755 |
| `docs/memory-measurement-protocol.md` | 644 |
| `docs/native-build-prerequisites.md` | 644 |

`scripts/generate_15_repos.sh` 與 `scripts/measure_memory.sh` 換成來源版本，轉呼叫 `workload_generator.py` 與 `memory_harness.py`。

有連結修補、blob 因此不同的文件：

- `docs/native-perf-harness.md`：delivery spec 連結改為同目錄的 `native-workbench-delivery-spec.md`。被拒的 pilot 報告改註明仍在來源 commit，本檢出不複製 `docs/evidence/`。
- `docs/tauri-baseline-measurement.md`：兩處 evidence 連結改為來源 commit 內的 `docs/evidence/tauri-baseline-20260925-r2/` 與 `docs/evidence/tauri-cleanup-check-20260925/`。表內數字、sha256、指令與 exit 註記維持原文。

`.gitignore` 加入 `__pycache__/` 與 `*.pyc`。

## 延後的六個 packaging 檔

下列路徑沒有建立或修改，留給 packaging worker，再由 Codex 併入：

- `scripts/package_native.sh`
- `scripts/verify_artifacts.py`
- `scripts/smoke_native.py`
- `scripts/tests/test_verify_artifacts.py`
- `scripts/tests/test_smoke_native.py`
- `docs/native-cross-platform-ci-and-packaging.md`

CI 與 justfile 仍呼叫前三支腳本。沒有 placeholder，也沒有把缺少檔案改成略過。

## 相對來源的必要修改

CI 以來源 workflow 為底，併入主線工作樹已有的 macOS fast gate。來源沒有這個 step。

保留的主線門檻：Rust fmt、clippy、rustdoc、`cargo audit`、DTO drift、clean checkout、frontend、三平台 test，以及 desktop E2E（Linux 與 `windows-2022`，腳本仍是 `crates/desktop/e2e/scenarios.mjs`）。`CI gate` 的 `needs` 是 `actionlint`、`format`、`lint-rust`、`lint-frontend`、`test`、`desktop-e2e`、`harness`、`native-smoke`、`native-candidate-artifacts`。

Linux job 沿用來源的 `ubuntu-24.04`。2026-09-26 的 GitHub 文件 [Choosing the runner for a job](https://docs.github.com/en/actions/using-jobs/choosing-the-runner-for-a-job) 把 `ubuntu-latest` 與 `ubuntu-24.04` 都連到同一份 Ubuntu 24.04 image readme。釘選對應 `docs/native-build-prerequisites.md` 的 noble 套件名（`libegl1`、`libegl-mesa0`）。

`test` job 依來源把 `snip-desktop-native` 從 workspace test 拆出。其餘 crate 仍是 `cargo test --workspace --locked`。原生 crate 在三平台只跑 `--bin snip-desktop-native`。Linux integration test 只在 `native-smoke`：`SNIP_REQUIRE_ALL_TESTS=1`，經 `scripts/headless-x11.sh` 提供 X11 `DISPLAY` 與 lavapipe，再跑 `cargo test -p snip-desktop-native --test smoke`。`crates/desktop-native/tests/smoke.rs` 在沒有 `DISPLAY` 或 `xdotool` 時，若 `SNIP_REQUIRE_ALL_TESTS` 有設會 assert。這次沒有改產品或 native 測試碼，也沒有在這台機器執行 `native-smoke`。

Intel candidate：來源把 `x86_64-apple-darwin` 放在 `macos-latest`（arm64）上交叉編譯，host 不是 `x86_64` 時該 step `exit 0`，binary 沒有執行。同一份 GitHub 文件的標準 runner 表（public 與 private）列出 Intel label `macos-15-intel` 與 `macos-26-intel`。`macos-latest` 連到 macOS 26 arm64 readme。`macos-13` 已於 2025-12-04 退役。此 leg 改為 `macos-26-intel`（與 `macos-latest` 同代的標準 Intel runner，不是 larger runner 的 `macos-26-large`）。`uname -m` 不是 `x86_64` 時 step `exit 1`。Mach-O header 不算執行通過。這個 job 還沒有在 GitHub 上跑過。

四個 candidate target 都還在：

| target | runs-on |
| --- | --- |
| `aarch64-apple-darwin` | `macos-latest` |
| `x86_64-apple-darwin` | `macos-26-intel` |
| `x86_64-unknown-linux-gnu` | `ubuntu-24.04` |
| `x86_64-pc-windows-msvc` | `windows-latest` |

`native-smoke` 只跑 Linux X11。candidate job 的 CLI smoke 是 `smoke_native.py` 對該 target binary 的執行，而且該腳本尚未進樹。macOS 與 Windows 的 GPUI 輸入沒有被這些 job 驅動，仍待實機驗證。

justfile 採用來源配方，並保留主線 `just native`（`cargo run -p snip-desktop-native`）。工作樹裡只跑 `cargo test --test smoke` 的 `native-smoke` 已換成來源的 headless 配方。沒有參數的 `bench-memory` 已移除；量測入口是 `scripts/measure_memory.sh` 與 `scripts/bench_native_memory.py`，來源 justfile 也沒有 `bench-memory`。`just preflight` 現在包含 `preflight-harness` 與 `native-smoke`。這次沒有執行 `just preflight`。

## 來源文件裡仍在的落差

`docs/native-build-prerequisites.md` §2.B 仍寫 `xvfb-run -a cargo test ...`。同一來源 commit 的 workflow 與 justfile 實際是 `scripts/headless-x11.sh`，並檢查 `smoke.log` 與三張 PNG。權威是 `.github/workflows/ci.yml` 與 `justfile`。該文件 §5 寫 harness「18 tests」；這次四個模組是 100 tests。`docs/tauri-baseline-measurement.md` 的「44 tests OK」是當時連 packaging 測試的歷史紀錄。這些句子沒有改。

## 這次實際跑過的檢查

`shellcheck` 不在 PATH，actionlint 沒有再把 workflow 的 `run:` 區塊送進 shellcheck。`bash -n` 覆蓋了變更過的 shell 腳本。

| 檢查 | exit | 紀錄 |
| --- | --- | --- |
| `actionlint` 1.7.12（與 CI 的 `ACTIONLINT_VERSION` 相同），repo 根目錄 | 0，無輸出 | `/tmp/snip-native-ci-integration/actionlint-all.txt`、摘要在 `static-checks.txt` |
| `bash -n`：`generate_15_repos.sh`、`measure_memory.sh`、`headless-x11.sh`、`smoke_native.sh`、`measure_tauri.sh` | 0 | 同上 `static-checks.txt` |
| `just` 1.51.0 `--list` | 0 | `/tmp/snip-native-ci-integration/just-list.txt` |
| `just --dry-run`：`preflight`、`preflight-rust`、`preflight-harness`、`native`、`native-smoke`、`package-native`、`verify-artifacts`、`native-cli-smoke` | 0 | `/tmp/snip-native-ci-integration/just-dry-*.txt` |
| Python 3.12.3，明確模組，100 tests，11.059s，`OK` | 0 | `/tmp/snip-native-ci-integration/unittest-3.12.txt` |
| Python 3.14.7，同一組模組，100 tests，11.178s，`OK` | 0 | `/tmp/snip-native-ci-integration/unittest-3.14.txt` |

兩份 unittest log 都有一行 `FAILED: cleanup: driver process group: still alive`。那是 `tests/test_bench_tauri.py` 設進 `FakeDriver.stop_problems` 的預期診斷，suite 結果是 `OK`。

命令是 `SNIP_REQUIRE_ALL_TESTS=1 PYTHONDONTWRITEBYTECODE=1 python3.12 -B -m unittest`（3.14 用 `python3`），模組為 `tests.test_workload_generator`、`tests.test_harness_contracts`、`tests.test_bench_tauri`、`tests.test_bench_native`，工作目錄 `scripts/`。

沒有跑 `unittest discover`。來源的 `test_smoke_native.py`（10 個 test）會 `from scripts.smoke_native import ...`，`test_verify_artifacts.py`（17 個 test）會 `from scripts.verify_artifacts import ...`。這 27 個 test 加上這次的 100 個，就是來源 suite 的 127。兩個模組都不在樹上。CI 的 harness job 與 `just preflight-harness` 仍是來源的 `python3 -B -m unittest discover -s scripts/tests`；packaging 檔進樹之後，discover 會一併收集那 27 個。

沒有跑 `cargo fmt`、clippy、`just preflight`、`native-smoke` 或 desktop E2E。`crates/core` 與 `crates/desktop-native` 仍由另一個 worker 修改。

## 仍待完成的 gate

- packaging 六個檔進樹之前，`test` job 的 CLI smoke，以及 `native-candidate-artifacts` 的 package／verify，會因為腳本不存在而失敗。
- `native-smoke` 與 22 個 Tauri real-app E2E 這次沒有執行。
- `macos-26-intel` 上的 `x86_64-apple-darwin` 執行這次沒有在 runner 上發生。
- macOS 與 Windows 的 GPUI GUI 輸入仍待實機驗證。candidate CLI smoke 通過也不代表 GUI 通過。
- Codex 負責併入 packaging 與 native 修復、跑完整 preflight、review，再 push。這份收據不代表實作完成或可以 release。
