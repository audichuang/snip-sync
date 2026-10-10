# Native Workbench Performance Harness (bench_native_memory.py)

> Note: the Tauri app (`crates/desktop`) and its drivers (`bench_tauri_memory.py`, `measure_tauri.sh`, the Tauri E2E) have been removed from the repo. Passages below that build, test or measure them are historical; use git history to roll back.

**Status**: Verified Test Driver Specification  
**Date**: 2026-09-27 (optional matched diff profile passed native/Tauri functional pilots; no D4 claim)
**Protocol Reference**: [`docs/memory-measurement-protocol.md`](memory-measurement-protocol.md)  
**Product Delivery Spec**: [`docs/native-workbench-delivery-spec.md`](native-workbench-delivery-spec.md) (Read-only)

---

## 1. Overview & Scope

`scripts/bench_native_memory.py` is the independent Linux/X11 performance and memory measurement driver for the native GPUI workbench. It integrates:
1. `scripts/memory_harness.py` for exact process-tree memory sampling (RSS, PSS via `/proc/<pid>/smaps_rollup`, VmHWM).
2. Pre-exec launcher (`--sampler-ready-file`): inside `dbus-run-session` the launcher records `(pid, starttime)` and waits. The sampler publishes the gate only after handlers are installed and `/proc/<pid>/exe` is readable. The launcher then `os.execv`s the target. Launch samples exist only after that exe transition is observed. This is discrete ~50 ms sampling, not the first dynamic-linker instruction.
3. Strict process boundary: External wrappers (`Xvfb`, `dbus-run-session`, `dbus-daemon`) are excluded from application process-tree RAM accounting. Samples taken while the launcher image is still running are `launcher-setup` and are excluded from target peaks.
4. The shared-helpers section of `scripts/bench_native_memory.py` for process identity, session isolation, reap/teardown mechanics, and metric summaries (moved there from the removed `bench_tauri_memory.py`).
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
| **`1repo`** | Single Git repository | `--mode normal` | Single repository with working changes and one history page. Nothing is copied before the driver's own copy. The driver clicks one source-aware row, right-clicks it and picks `menu-item:copy-files` (Copy). Verifies that row's bytes (index for staged, worktree for unstaged/untracked). |
| **`1repo-diff`** | One standard repository | `--mode normal` (`SNIP_NATIVE_E2E=1`) | Opt-in: selected two-commit feature branch on a standard repository. Both native and Tauri select `refs/heads/feat/divergent`, its current tip, and the same Git-oracle text file through real UI controls. Final client size is 1080×720, graph/refs remain active, diff is displayed, and no Copy action occurs. |
| **`15overview`** | Standard 15-repo workspace | `--mode overview` | **Important Prototype Finding**: Upon launch with 15 repositories, the current native workbench prototype automatically selects repository 0, loads its history graph, and displays its working tree preview. Therefore, `15overview` measures the combined footprint of 15 discovered repositories plus 1 active loaded repository. **It does not certify a summary-only overview memory** until a future revision implements deferred graph/preview retention. |
| **`15active`** | Standard 15-repo workspace | `--mode normal` (`SNIP_NATIVE_E2E=1`) | Launches into the 15-repo workspace (repository 0 loads), then switches to repository 1 by clicking `repo-row:<name>`. This checkpoint's keymap has no `Alt+2` binding. Measures `clickToRepoLoadedMs` and `clickToGraphLoadedMs`. |
| **`soak`** | Standard 15-repo workspace | `--mode normal` (`SNIP_NATIVE_E2E=1`) | Executes 100 sequential repository switches by clicking `repo-row` controls, scrolling inside `left-list`. Each switch must reach `REPO_LOADED` and `GRAPH_LOADED`, and must not run a copy. The reported switch count is the number of completed clicks. The final copy is one explicit source row, after `rail-changes`. |
| **`3tabs`** | One repository, plus two more repositories of the dataset | `--mode normal` (`SNIP_NATIVE_E2E=1`) | Opt-in: three workspace tabs in one window, one repository each. The launch repository loads in the first tab; the driver opens the next two dataset repositories by clicking `ws-tab-new` and typing each path, waits for each to load, then clicks `ws-tab:0` and makes the same copy `1repo` makes. Measured only: no budget is set from it yet. |

---

## 4. Verification & Strict Oracles

### Literal Log Readiness Matching
Readiness is never determined by generic startup logs. The driver requires literal token matches for:
- Repository count: `[APP:READY_REPOS: <N>]`
- Loaded repository state: `[APP:REPO_LOADED: <name> files=<N>]`
- Graph rendering: `[APP:GRAPH_LOADED: commits=<N>]`
- Preview rendering: `[APP:PREVIEW_LOADED: <path>]`

### Cropped Root Screenshots
Because Xvfb lacks a composite window manager, `xwd -id` against unmanaged GPUI windows captures black frames (<1 KB). The driver captures the full X11 root window (`xwd -root`) and crops strictly to the translated client geometry determined by `xwininfo -id`.  The screenshot only has to be non-blank; what the app shows is verified through its own state lines against the git oracle, not by reading pixels.

### Source rows and no early copy
`[APP:REPO_LOADED] files=` is the number of staged, unstaged, untracked, and conflicted rows from `git status --porcelain=v2 -z --untracked-files=normal --renames`. A path that is both staged and unstaged counts twice. Comparing that number to distinct paths fails the run.

Loading a repository and switching repositories must not copy anything. Any `[APP:COPY_PREP:` or `[APP:COPY_DONE:` before the driver's own copy fails the run. (The app has no selection basket: a Copy reads only the node it was opened on, so there is no earlier selection to leak into the payload.)

The copy target is one visible control. Prefer a path whose index bytes differ from the worktree, and copy the staged row. Otherwise copy the first non-deleted UTF-8 staged, unstaged, or untracked row. Required ids are `change-row:<source>:<path>` and, once its context menu is open (`[APP:MENU_OPEN: Left items=copy-files,…]`), `menu-item:copy-files`. A path-only id such as `change-row:<path>` is not accepted. `left-list` is the scroll viewport; `left-scroll` is not accepted. Missing bounds fail the run.

`SNIP_NATIVE_E2E=1` is set for every profile that clicks a control, because that is when the app emits `[APP:CTRL_BOUNDS]`. Idle does not. The flag is recorded on the run. It is not a release setting.

### True Byte-Exact Clipboard Oracle
The copy is a right-click on the row, then a click on `menu-item:copy-files`. `Ctrl+C` is not used here: with the reader focused and text selected it copies preview text.
1. **Sentinel**: before the click the driver writes a unique sentinel and reads it back. After `COPY_DONE` the clipboard must no longer contain it.
2. **Raw Byte Reading**: the driver reads the X11 clipboard via `xclip -selection clipboard -o` using raw subprocess byte pipes, with no universal-newline translation. The bytes are also saved as `clipboard.bin`. Writes use `xclip -selection clipboard -i -quiet` so the Popen pid stays the foreground owner (`-silent` forks and the parent exits). `stop` waits for that pid before it tears down Xvfb. It does not signal any other xclip.
3. **Strict UTF-8 Decode**: non-UTF-8 payloads fail immediately.
4. **Root Header Anchoring**: the first line must match `// clipcode-root: <repo_name>` exactly.
5. **Only the copied row**: the `// file:` paths in the payload must be exactly that one path, and `COPY_DONE copied=` must be 1.
6. **Anchored File Framing**: `^// file: (?:\[[A-Z]+\] )?<path>$`, then Scheme A unescape (`//clipcode-esc: `).
7. **Source bytes**: staged content is `git show :<path>` (the index). Unstaged and untracked content is the worktree file. A deletion is the core deleted-file marker. CRLF, a trailing blank line, a UTF-8 BOM, and non-ASCII bytes are kept. A mismatch fails the run through `finalize_run`. Staged and deleted copies omit empty text wrappers (one delimiter newline after the file). Unstaged and untracked copies include them: a blank line after the root header, and one extra newline after the delimiter. The extractor removes that framing. It does not drop the file's own trailing newline.

### History page
The workbench loads 50 commits per page. That length is the application's built-in value, checked against `[APP:GRAPH_LOADED]` and `[APP:E2E_LOG]` (`mode=graph`, `first=` equals the first commit of `git log --topo-order -n 51 --branches --remotes --tags HEAD`). There is no `--history-page-size` flag and no screen flag. Observed window geometry stays on each run. A different row count fails the run.

### Optional matched `1repo-diff`

This profile is **selected two-commit feature branch on standard repository**, not a comparison of the default native 50-row and Tauri 300-row pages, and not a 15-repository comparison. Existing defaults and old `1repo` Copy/content workflows are preserved; request `--profile 1repo-diff` explicitly in either driver.

The oracle reads `git log --topo-order --format=%H refs/heads/feat/divergent --` and refuses a missing ref or any count other than two. It chooses the tip's first small added/modified text file using the existing commit oracle. OIDs and paths are computed for each run, not hardcoded to the current standard fixture. The standard manifest must name the participating repository.

Native first uses its real locale toggle to switch the frozen app's initial Traditional Chinese UI to English, matching Tauri. It requires the observed `LOCALE: En` event and the English UI in the screenshot. It clicks stable current ref bounds (or searches the existing ref picker by its short branch label), commit row and commit file. Fresh `REF_FILTER`, `GRAPH_LOADED` and `E2E_LOG` establish ref, count, graph mode and first row. The two live commit-row bounds must contain the unique expected short OIDs in vertical order. The full preview OID/path/source and retained patch line count/FNV must match the independent Git patch. This hash proves the retained patch, not that every patch byte was painted. A nonblank root-cropped screenshot additionally checks the path, history and changed text. Tauri records the language toggle's rendered label and rejects a non-English UI.

Tauri uses WebDriver ref/commit/file clicks. Its selected ref, full displayed OID sequence, commit-details OID, preview path and diff mode must match. For this small diff, all changed rows must fit inside the visible preview: shadow-DOM `data-line` rows supply their addition/deletion kind, text, sequence and count. One renderer terminal LF/CRLF is removed per row; other whitespace and blank changed lines are preserved. Missing, extra, reordered, clipped or different rows fail. This is normalized rendered-row equality, not byte-exact patch rendering. Neither driver visits a full-file content view in this profile or claims to have displayed the complete source file.

The final view matches, but startup traces differ: native initially loads its 50-row history and working preview; Tauri initially loads the commits tab's 300-row page. These prior actions are recorded. Native retains a bounded patch reader; Tauri's preview component also holds its backend content result. This profile does not establish identical internal retention, identical startup allocation peaks, or a release acceptance gate. Compare settled process-tree measurements under this narrow scenario, with these differences stated.

Native verifies actual X11 client geometry. Tauri adjusts the WebDriver window rectangle until the observed inner size is 1080×720 and verifies a device-pixel ratio of 1. Both validate geometry, selection and clipboard again after steady sampling. The same fixed clipboard sentinel is owned by a separate foreground `xclip` process on the owned private X11 display, outside application RAM accounting; its exact bytes must remain unchanged. Native rejects any copy event (`COPY_PREP`, `COPY_DONE`). Tauri checks that no file-selection checkbox is checked; it has no native-style basket, and the previewed commit remains selected.

Each run hashes the binary, standard manifest, participating repo refs/index and tracked/nonignored untracked worktree bytes before and after. This fingerprint does not certify every other repository or ignored file in the 15-repository dataset. Changes fail the run. At least 30 steady seconds and valid RSS/PSS median, p95 and maximum are required; missing PSS is a failure for this profile. Raw sampler output, process identity, private XDG/D-Bus, process-cold/uncontrolled filesystem-cache semantics and teardown remain the existing harness behavior. The native launch peak and Tauri late-attach pre-ready peak remain different measurement boundaries.

After the coordinator approves a GUI slot, run a single functional pilot with the frozen release binary and matching receipt (substitute concrete paths). No build is performed:

```bash
rtk proxy env -u GIT_DIR -u GIT_WORK_TREE -u GIT_COMMON_DIR \
  -u DISPLAY -u WAYLAND_DISPLAY -u DBUS_SESSION_BUS_ADDRESS \
  python3 scripts/bench_native_memory.py \
  --bin /path/to/frozen/snip-desktop-native --build-profile release \
  --label "Matched functional pilot" --source-note "frozen binary; see receipt" \
  --workspace /tmp/snip-workload-standard-20260925 \
  --profile 1repo-diff --runs 1 --steady-seconds 30 --sample-interval 0.05 \
  --out-dir /tmp/native-matched-pilot-NEW
```

The adjacent `.receipt.json` is discovered automatically; pass `--build-receipt` if it is elsewhere. The 2026-09-27 functional pair (before the Tauri app was removed) passed with native binary `b11e6724` and Tauri `a4664824`, English UIs, 600 steady samples each and clean teardown. Evidence and source snapshots are listed in `/tmp/snip-session4-matched-performance-report.md`. Earlier native picker/OCR failures and a Chinese-locale functional run remain separate artifacts. Formal ten-run measurements are pending stable final binaries and coordinator approval; a functional pilot does not pass D4. `--compare-baseline` remains unsupported; the coordinator compares concrete matched artifacts.

### Standard-dataset discovery
On the first D3 checkpoint, startup calls discovery for one page of 10,000 directory visits and depth 8, and that page walks into the working tree before it records `.git`. Against `/tmp/snip-workload-standard-20260925` and against `repo-01-core` alone, the only readiness line is `[APP:READY_REPOS: 0]`; it does not update if the process is left running. The driver fails that run. It does not point the app at a smaller tree, and it does not treat 0 as the 15-repo workload. Repo switches, the explicit copy, and the 100-switch soak therefore do not start.

A separate smoke-preset tree (15 repos, 10 files and 10 commits each) can be used only to exercise this driver. Its manifest preset stays `smoke`. It is not the standard workload and its memory numbers are not a D4 or savings result.

Pointer motion is `xdotool mousemove` without `--sync`. `--sync` waits for a motion event, and that wait never finishes when the pointer is already on the target pixel. `windowfocus --sync` is unchanged. The click or wheel that follows is in the same `xdotool` invocation, and the driver still waits for the app's bounds or `REPO_SELECTING` line.

If session startup fails after Xvfb is running (no display number, or the launcher `Popen` raises), the constructor reaps that Xvfb, its log, and the private directory before the exception leaves `NativeSession`. `stop` is safe to call again. The app-log reader is joined before its stdout and `app.log` are closed. A reader that does not finish is reported; cleanup does not claim success with an empty problem list.

### Historical UI probes
The Python unit suite does not launch a historic debug binary and does not duplicate the product UI case. Same-path index `INDEX_A` versus worktree `WORK_B` through the native controls lives in `crates/native-e2e/tests/smoke.rs` (the block that writes `both.txt`, right-clicks `change-row:staged:both.txt` / `change-row:unstaged:both.txt`, picks `menu-item:copy-files`, and checks the clipboard). Driver unit tests keep the git byte oracle and the ClipCode extractor.

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
