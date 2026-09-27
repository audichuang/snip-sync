# Native Git Workbench: Required Core APIs (snip-core needs)

This document records the core APIs required by the Native Git Workbench (`crates/desktop-native`).
All required bounded runner and reader APIs have been delivered in `snip-core` (PR #19 merged to `develop` as `c05658e`).
`crates/desktop-native/src/core_shim.rs` has been completely eliminated; all features consume `snip-core` directly.

---

## 1. Commit Directory Listing (`commit_directory` - Delivered)

### Status: Delivered in `snip_core::browser::commit_directory`
- Uses bounded streaming (`RunOptions`), returning `Vec<TreeEntry>` with `TreeKind::Blob`, `TreeKind::Tree`, `TreeKind::Submodule`.
- Supports `CancelToken` for immediate query cancellation.
- Enforces strict UTF-8 validation without lossy substitution.
- Tested and verified in `crates/core/tests/git_runner.rs` and native integration.

---

## 2. Commit Blob Reading (`commit_blob` - Delivered)

### Status: Delivered in `snip_core::browser::commit_blob` / `commit_blob_with`
- Uses bounded streaming (`RunOptions`), returning `BlobText` with `BlobText::Text`, `BlobText::Binary`, `BlobText::TooLarge`, `BlobText::NotUtf8`.
- Size-first admission verifies blob size before reading content.
- Binary detection checks for NUL bytes within admission header.
- Strict UTF-8 verification differentiates binary vs non-UTF-8 vs valid UTF-8.
- Tested and verified in `crates/core/tests/git_runner.rs` and native integration.

---

## 3. History Filtered by Author / Scope (`history_by_author` - Delivered)

### Status: Delivered in `snip_core::browser::history_by_author` / `history_by_author_with`
- Uses bounded streaming (`RunOptions`), returning `(Vec<CommitSummary>, bool)` indicating whether further pages exist.
- Consistent NUL-delimited log parsing (`%H%x00%P%x00%an%x00%ae%x00%aI%x00%s%x1e`).
- Maintains `--topo-order` for deterministic DAG edges.
- Tested and verified in `crates/core/tests/git_runner.rs` and native integration.

---

## 4. Bounded Runner & Process Lifecycle Integration (Delivered in snip-core)

Delivered and integrated:
1. **Global Concurrency & Queue**: Maximum 2 concurrent Git processes across the application, with a 64-waiter queue limit (`snip_core::gitrun`).
2. **Cancellation & Cleanup**: `CancelToken` terminates running Git child process trees cleanly via process group (Unix) or Job Object (Windows) without zombie leaks or pipe deadlocks.
3. **Bounded Memory Buffers**: `RunOptions::interactive` and `RunOptions::preview` enforce strict byte caps (e.g. 1MiB preview, 8MiB history) and prevent unbounded memory consumption.
4. **Per-Worktree Locking**: `lock_heavy` ensures at most one index- or ref-changing Git operation per worktree.

---

## 5. Multi-Repo Staged vs Working Content & Basket Transfer Engine (D3, not accepted)

Source-safe basket, explicit root mapping, and transfer replay are part of D3 and are only partially wired. The native UI now calls the shared APIs below; this is not a D3 acceptance record.

Added on top of the accepted transfer engine:

- `plan_commit_export_exact(git, tip, selected)` checks that `selected` is exactly the contiguous first-parent chain ending at `tip`, including a root on a branch that is not HEAD.
- `CommitReplayPreview::capture` plans, records symlink-safe destination freshness for every named replay path (including a non-UTF-8 skip), plans again, and refuses if those two plans differ. `revalidate` checks that freshness and a fresh `plan_commit_replay`. A skipped path that becomes writable is `StaleDestination`. Symlink targets are not opened. `NotCopied` stays out of the file snapshot. `commits::replay` remains the only writer.
- `DestinationFreshnessSnapshot::capture_paths` is the file-import snapshot. It still follows symlinks. Commit replay does not use that follower.

Still unresolved for a later core change: `plan_export` does not take a `CancelToken` (its third argument is the payload byte cap). Commit copy still uses `Git::run` default options. Cancellation has to be added on the shared API, not by overloading that byte cap.

## 5b. Previously described transfer surface

Source-safe basket, explicit root mapping, and transfer replay use these accepted shared `snip-core` APIs:

1. **Accepted Core Transfer APIs (`snip_core::transfer`)**:
   - `CanonicalRootId`: Verified, canonicalized workspace/repo root identity.
   - `SourceKind`: `File`, `Working`, `Unstaged`, `Staged`, `Commit { rev }`.
   - `ExportItem`: `(root, relative_path, source, change_type)`.
   - `ExportSelection`: Multi-root export container validated against root escaping, path tampering, staged vs working conflicts, duplicate basenames, and wire header collisions.
   - `plan_export`: Bounded export serialization with wire overhead admission and source freshness revalidation (`plan.revalidate()?`).
   - `ImportMapping`: Explicit destination mapping (`map_prefix`, `map_entry`, `block_prefix`, `primary_destination`).
   - `plan_import`: Import planning returning `TransferImportPlan` with destination freshness token.
   - `TransferImportPlan::apply`: Revalidates destination freshness (HEAD ref, index hash, file mtime/hash) immediately prior to applying changes. Returns `TransferError::StaleDestination` without modifying files if external edits occurred.

2. **No Filesystem Atomic Rollback Promise**:
   - As documented in porting notes and delivery spec, filesystem writes are not atomic across arbitrary processes without kernel-level transaction support.
   - Safety is guaranteed by pre-condition dry runs, destination freshness token verification (`destination_freshness.revalidate()?`), and strict cancellation rather than promising unachievable multi-file filesystem rollback.
