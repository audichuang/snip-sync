# Native Workbench Performance Harness (bench_native_memory.py)

**Status**: Verified Test Driver Specification  
**Date**: 2026-09-26 (explicit source-aware copy; standard-dataset discovery blocker recorded, no D4 claim)
**Protocol Reference**: [`docs/memory-measurement-protocol.md`](memory-measurement-protocol.md)  
**Product Delivery Spec**: [`docs/native-workbench-delivery-spec.md`](native-workbench-delivery-spec.md) (Read-only)

---

## 1. Overview & Scope

`scripts/bench_native_memory.py` is the independent Linux/X11 performance and memory measurement driver for the native GPUI workbench. It integrates:
1. `scripts/memory_harness.py` for exact process-tree memory sampling (RSS, PSS via `/proc/<pid>/smaps_rollup`, VmHWM).
2. Pre-exec launcher (`--sampler-ready-file`): inside `dbus-run-session` the launcher records `(pid, starttime)` and waits. The sampler publishes the gate only after handlers are installed and `/proc/<pid>/exe` is readable. The launcher then `os.execv`s the target. Launch samples exist only after that exe transition is observed. This is discrete ~50 ms sampling, not the first dynamic-linker instruction.
3. Strict process boundary: External wrappers (`Xvfb`, `dbus-run-session`, `dbus-daemon`) are excluded from application process-tree RAM accounting. Samples taken while the launcher image is still running are `launcher-setup` and are excluded from target peaks.
4. `scripts/bench_tauri_memory.py` for shared process identity, session isolation, reap/teardown mechanics, and metric summaries. That driver still attaches late and says so.
5. Private headless display management via Xvfb (`-displayfd`), Mesa lavapipe software Vulkan rasterization (`VK_DRIVER_FILES`), and isolated D-Bus sessions (`dbus-run-session`).
6. Real OS-level user input injection via XTEST (`xdotool key`, `xdotool mousemove`, `xdotool click`) without mock handlers.
7. Independent Git and filesystem oracles against standard benchmark workloads.
8. Build receipt, hash, and source note are recorded as provenance. `--compare-baseline` exits `UNSUPPORTED` and does not emit a comparison. The release D4 gate is not passed by this driver.

---

## 2. Measurement Boundaries & Known Constraints

### Process Tree Accounting vs. Unmeasured Surfaces
- **Measured**: User-space RAM occupied by the application process and all spawned descendants (Git workers, reader tasks).
- **Unmeasured / Excluded**: GPU VRAM, DMA-BUF, driver-internal allocations, font caches managed outside process memory, and X11 server pixmaps.
- **Discrete Sampled Peaks vs. Absolute Peaks**: Nominal ~50 ms sampling records observed process-tree memory at discrete intervals. It is a discrete sampled peak, not an absolute continuous hardware peak, and sub-interval transient allocations may not be reflected.
- **Software Rendering CPU Overhead**: Under headless Xvfb, rendering relies on Mesa lavapipe (`llvmpipe`). Vertex processing and fragment rasterization execute entirely on the host CPU. Steady-state CPU usage measurements reflect this software rendering workload; they must not be compared directly against hardware-accelerated GPU execution.

### Launch Sampling vs. Driver Late-Attach
- **Native workbench**: The launcher pauses on `sampler_ready.signal`. The harness publishes that file only after it can read the process identity. Samples before `/proc/<pid>/exe` becomes the expected binary are `launcher-setup` and are excluded from target peaks. Ticks whose exe changes or cannot be read across the sample are excluded. `launch` is claimed only when the gate was published on a different exe and a later tick saw the expected exe. A process that already is the expected binary at attach is `pre-ready`.
- **Tauri baseline**: WebDriver attaches after the owned app PID exists. The report says `late-attach` and does not claim a launch phase. `pre-ready` includes WebView load and the UI workload. `processAgeAtSamplerStartSec` is the gap before the first sample.

### Cache Provenance
- **Process-cold**: every run is a fresh process tree with private XDG directories and a private D-Bus session.
- **Filesystem cache**: uncontrolled. This driver does not drop caches and does not claim filesystem-cold. Filesystem-cold is `UNSUPPORTED`.

### Latency Reporting
- Latencies are measured using monotonic clocks from real input event emission (`alt+2`, mouse clicks) to literal app log confirmation (`[APP:REPO_LOADED]`, `[APP:GRAPH_LOADED]`).
- For profiles without user input transitions (e.g., `idle`), latency is reported as `–` (not applicable), never fabricated.

---

## 3. Benchmark Profile Semantics

| Profile | Workspace Setup | App Mode | Semantic Scope & Retained State |
| :--- | :--- | :--- | :--- |
| **`idle`** | Empty workspace dir | `--mode idle` | Measures clean startup baseline with 0 repositories. Verifies empty title bar, zero git workers, and window mapping. Latency: `–`. |
| **`1repo`** | Single Git repository | `--mode normal` | Single repository with working changes and one history page. The basket starts empty. The driver clicks one source-aware row and its checkbox, then `btn-copy`. Verifies that entry's bytes (index for staged, worktree for unstaged/untracked). |
| **`15overview`** | Standard 15-repo workspace | `--mode overview` | **Important Prototype Finding**: Upon launch with 15 repositories, the current native workbench prototype automatically selects repository 0, loads its history graph, and displays its working tree preview. Therefore, `15overview` measures the combined footprint of 15 discovered repositories plus 1 active loaded repository. **It does not certify a summary-only overview memory** until a future revision implements deferred graph/preview retention. |
| **`15active`** | Standard 15-repo workspace | `--mode normal` (`SNIP_NATIVE_E2E=1`) | Launches into the 15-repo workspace (repository 0 loads), then switches to repository 1 by clicking `repo-row:<name>`. This checkpoint's keymap has no `Alt+2` binding. Measures `clickToRepoLoadedMs` and `clickToGraphLoadedMs`. |
| **`soak`** | Standard 15-repo workspace | `--mode normal` (`SNIP_NATIVE_E2E=1`) | Executes 100 sequential repository switches by clicking `repo-row` controls, scrolling inside `left-list`. Each switch must reach `REPO_LOADED` and `GRAPH_LOADED`, and must not add a basket entry. The reported switch count is the number of completed clicks. The final copy is one explicit source row, after `rail-changes`. |

---

## 4. Verification & Strict Oracles

### Literal Log Readiness Matching
Readiness is never determined by generic startup logs. The driver requires literal token matches for:
- Repository count: `[APP:READY_REPOS: <N>]`
- Loaded repository state: `[APP:REPO_LOADED: <name> files=<N>]`
- Graph rendering: `[APP:GRAPH_LOADED: commits=<N>]`
- Preview rendering: `[APP:PREVIEW_LOADED: <path>]`

### Cropped Root Screenshots
Because Xvfb lacks a composite window manager, `xwd -id` against unmanaged GPUI windows captures black frames (<1 KB). The driver captures the full X11 root window (`xwd -root`) and crops strictly to the translated client geometry determined by `xwininfo -id`. OCR checks (`tesseract`) verify that repository titles and commit SHAs are visually rendered.

The header leaf (`hdr-workspace`) is clipped to 140px and ends in an ellipsis. On this binary's 1080×720 window the painted prefix is 21 ASCII characters (`snip-driver-small-fix.` for the small driver fixture). The screenshot must-token is that prefix when the leaf is longer, and the whole leaf when it fits (`repo-01-core`, `empty-workspace`). Repository name, selected path, preview lines, and history lines stay required in full.

### Source rows and an empty basket
`[APP:REPO_LOADED] files=` is the number of staged, unstaged, untracked, and conflicted rows from `git status --porcelain=v2 -z --untracked-files=normal --no-renames`. A path that is both staged and unstaged counts twice. Comparing that number to distinct paths fails the run.

Loading a repository and switching repositories must not check a row. Any `[APP:BASKET] n=` other than 0, or `[APP:FILE_TOGGLED] selected=true`, before the driver's own checkbox click fails the run. The driver does not clear a non-empty basket to make the copy succeed.

The copy target is one visible control. Prefer a path whose index bytes differ from the worktree, and copy the staged row. Otherwise copy the first non-deleted UTF-8 staged, unstaged, or untracked row. Required ids are `change-row:<source>:<path>`, `change-chk:<source>:<path>`, and `btn-copy`. A path-only id such as `change-row:<path>` is not accepted. `left-list` is the scroll viewport; `left-scroll` is not accepted. Missing bounds fail the run.

`SNIP_NATIVE_E2E=1` is set for every profile that clicks a control, because that is when the app emits `[APP:CTRL_BOUNDS]`. Idle does not. The flag is recorded on the run. It is not a release setting.

### True Byte-Exact Clipboard Oracle
The copy is a click on `btn-copy` after the checkbox is on. `Ctrl+C` is not the basket gesture here: with the reader focused it copies preview text.
1. **Sentinel**: before the click the driver writes a unique sentinel and reads it back. After `COPY_DONE` the clipboard must no longer contain it.
2. **Raw Byte Reading**: the driver reads the X11 clipboard via `xclip -selection clipboard -o` using raw subprocess byte pipes, with no universal-newline translation. The bytes are also saved as `clipboard.bin`. Writes use `xclip -selection clipboard -i -quiet` so the Popen pid stays the foreground owner (`-silent` forks and the parent exits). `stop` waits for that pid before it tears down Xvfb. It does not signal any other xclip.
3. **Strict UTF-8 Decode**: non-UTF-8 payloads fail immediately.
4. **Root Header Anchoring**: the first line must match `// clipcode-root: <repo_name>` exactly.
5. **Only the selected entry**: the `// file:` paths in the payload must be exactly that one path, and `COPY_DONE copied=` must be 1.
6. **Anchored File Framing**: `^// file: (?:\[[A-Z]+\] )?<path>$`, then Scheme A unescape (`//clipcode-esc: `).
7. **Source bytes**: staged content is `git show :<path>` (the index). Unstaged and untracked content is the worktree file. A deletion is the core deleted-file marker. CRLF, a trailing blank line, a UTF-8 BOM, and non-ASCII bytes are kept. A mismatch fails the run through `finalize_run`. Staged and deleted copies omit empty text wrappers (one delimiter newline after the file). Unstaged and untracked copies include them: a blank line after the root header, and one extra newline after the delimiter. The extractor removes that framing. It does not drop the file's own trailing newline.

### History page
The workbench loads 50 commits per page. That length is the application's built-in value, checked against `[APP:GRAPH_LOADED]` and `[APP:E2E_LOG]` (`mode=graph`, `first=` equals the first commit of `git log --topo-order -n 51 --all HEAD`). There is no `--history-page-size` flag and no screen flag. Observed window geometry stays on each run. A different row count fails the run.

### Standard-dataset discovery
On the first D3 checkpoint, startup calls discovery for one page of 10,000 directory visits and depth 8, and that page walks into the working tree before it records `.git`. Against `/tmp/snip-workload-standard-20260925` and against `repo-01-core` alone, the only readiness line is `[APP:READY_REPOS: 0]`; it does not update if the process is left running. The driver fails that run. It does not point the app at a smaller tree, and it does not treat 0 as the 15-repo workload. Repo switches, the explicit copy, and the 100-switch soak therefore do not start.

A separate smoke-preset tree (15 repos, 10 files and 10 commits each) can be used only to exercise this driver. Its manifest preset stays `smoke`. It is not the standard workload and its memory numbers are not a D4 or savings result.

Pointer motion is `xdotool mousemove` without `--sync`. `--sync` waits for a motion event, and that wait never finishes when the pointer is already on the target pixel. `windowfocus --sync` is unchanged. The click or wheel that follows is in the same `xdotool` invocation, and the driver still waits for the app's bounds or `REPO_SELECTING` line.

If session startup fails after Xvfb is running (no display number, or the launcher `Popen` raises), the constructor reaps that Xvfb, its log, and the private directory before the exception leaves `NativeSession`. `stop` is safe to call again. The app-log reader is joined before its stdout and `app.log` are closed. A reader that does not finish is reported; cleanup does not claim success with an empty problem list.

### Historical UI probes
The Python unit suite does not launch a historic debug binary and does not duplicate the product UI case. Same-path index `INDEX_A` versus worktree `WORK_B` through the native controls lives in `crates/desktop-native/tests/smoke.rs` (the block that writes `both.txt`, clicks `change-chk:staged:both.txt` / `change-chk:unstaged:both.txt`, and checks the clipboard). Driver unit tests keep the git byte oracle and the ClipCode extractor.

A one-off probe against the immutable first-D3 binary is evidence under `/tmp`, not a CI test. Binary SHA-256 `a923e8332ef81a895ce3175462cc34a96595993d6ca47251500d9b97772ef2fb` at `/tmp/snip-d3-immutable-a923e833-20260926/snip-desktop-native-0.1.4/bin/snip-desktop-native` (the 2026-09-26 package member; the shared unpack path was later overwritten). Command:

```bash
python3 /tmp/snip-driver-review-followup-20260926/probe.py
```

That script writes `/tmp/snip-driver-review-followup-20260926/dual-source/` (one staged copy and one unstaged copy, fresh sessions, private repo) and `/tmp/snip-driver-review-followup-20260926/short-switch/` (one `repo-row` click, no `--sync`). It checks the hash before either run. It is not a five-profile measurement and it is not a D4 result.

The 2026-09-26 run exited 0 against that hash. Staged copy was index `INDEX_A\n` (8 bytes, `clipboard.bin` 66 bytes). Unstaged copy was worktree `WORK_B\n` (7 bytes, `clipboard.bin` 67 bytes). Each was `copied=1`, path `both.txt` only, source rows 2, distinct paths 1, cleanup `[]`. The switch started on `repo-a` and the click selected `repo-b` at index 1 with an empty basket. Record: `/tmp/snip-driver-review-followup-20260926/result.json`.

---

## 5. Artifact & Evidence Policy

- Binary `target/native-pilot/snip-desktop-native-debug-pilot` (SHA-256 `1ab0d03e523441439a1f4d27cee96f730a653a1d343b2d52280e37b26f68cb28`) is labeled **DEBUG PILOT ONLY**.
- Pilot runs serve to stabilize and verify the harness test infrastructure. They do not constitute official release evidence or acceptance claims against Tauri baselines.
- Prior rejected reports (such as `docs/evidence/native-pilot-checkpoint-20260925/`) are retained for historical audit trails with explicit `[REJECTED / SUPERSEDED]` notices.
- **Profile labels** are `native <buildProfile> <profile>`. The build profile and any receipt are provenance. They do not pass the release D4 gate.
- **`--compare-baseline`** exits 2 and prints `UNSUPPORTED`. It does not write a comparison. The supervisor compares matched run artifacts.
- **Viewport**: each run records the window from `xwininfo` and the cropped screenshot. There is no `--screen` flag. `applicationHistoryPageLength` is the workbench built-in checked above. Observed history row counts are on `ui.state.historyRows`. It is not a comparison acceptance.
- **Debug pilots**: a debug binary, including the first D3 checkpoint, does not pass the release D4 gate and is not a Tauri baseline. Tauri still has no 15-repo measurement; this driver does not estimate one. `--compare-baseline` stays `UNSUPPORTED`.

### Repeated measurement

Process-cold, filesystem cache uncontrolled, no cache purge. This command measures. It does not compare and it does not pass D4:

```bash
python3 scripts/bench_native_memory.py \
  --bin target/release/snip-desktop-native \
  --build-profile release \
  --build-receipt target/release/snip-desktop-native.receipt.json \
  --label "Release Candidate" \
  --workspace /tmp/snip-workload-standard \
  --out-dir target/benchmark-native-10repeat \
  --runs 10 \
  --steady-seconds 30.0 \
  --sample-interval 0.05
```
