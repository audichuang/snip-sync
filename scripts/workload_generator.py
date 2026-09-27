#!/usr/bin/env python3
"""
workload_generator.py - Deterministic Git Workload Generator for snip-sync.

Generates reproducible multi-repository Git workloads with exact counts,
divergent branches, merge commits, and rich working-tree/index states:
- Staged changes
- Unstaged changes
- Untracked files
- samepath staged A working B
- Staged renames
- Staged and unstaged deletions

Implements safety guards:
- Rejects non-empty target directories before touching anything.
- Rejects symlink targets and unexpected symlink parents.
- Cleans up only own temporary scratch directories.
"""

from __future__ import annotations

import argparse
import datetime
import json
import os
import random
import shutil
import subprocess
import sys
import tempfile
from typing import Any

WORKLOAD_REVISION = "2026-09-25.1"

# Presets defined per docs/native-git-workbench-plan.md sections 9-10
PRESETS: dict[str, dict[str, int]] = {
    "smoke": {
        "repos": 2,
        "files": 10,
        "commits": 10,
        "refs": 3,
    },
    "medium": {
        "repos": 15,
        "files": 100,
        "commits": 60,
        "refs": 10,
    },
    "standard": {
        # Standard plan per Section 10: 15 repos, each >=10,000 paths, 20,000 commits, 100 refs
        "repos": 15,
        "files": 10000,
        "commits": 20000,
        "refs": 100,
    },
}

REPO_NAMES = [
    "repo-01-core",
    "repo-02-api",
    "repo-03-web",
    "repo-04-auth",
    "repo-05-billing",
    "repo-06-infra",
    "repo-07-docs",
    "repo-08-mobile",
    "repo-09-analytics",
    "repo-10-gateway",
    "repo-11-shared-ui",
    "repo-12-cli",
    "repo-13-notifications",
    "repo-14-search",
    "repo-15-admin",
]


def validate_and_prepare_target_dir(target_dir: str) -> tuple[str, bool]:
    """
    Validate target_dir according to safety rules:
    - Reject if target_dir is a symlink.
    - Reject if target_dir exists and is non-empty.
    - Create directory if it does not exist.
    Returns (abs_path, was_created).
    """
    if not target_dir or not target_dir.strip():
        raise ValueError("Target directory path cannot be empty.")

    abs_dir = os.path.abspath(target_dir)

    # Reject if target path itself is a symlink
    if os.path.islink(abs_dir) or os.path.islink(target_dir):
        raise ValueError(f"Target directory '{target_dir}' is a symlink, which is rejected for safety.")

    if os.path.exists(abs_dir):
        if not os.path.isdir(abs_dir):
            raise ValueError(f"Target path '{target_dir}' exists and is not a directory.")
        entries = os.listdir(abs_dir)
        if len(entries) > 0:
            raise ValueError(
                f"Target directory '{target_dir}' is not empty (contains {len(entries)} items). "
                "Refusing to clobber existing directory."
            )
        return abs_dir, False
    else:
        # Verify parent directory is not a symlink to an unexpected place
        parent = os.path.dirname(abs_dir)
        if os.path.islink(parent):
            raise ValueError(f"Parent directory of '{target_dir}' is a symlink, rejected for safety.")
        os.makedirs(abs_dir, exist_ok=False)
        return abs_dir, True


def generate_fast_import_stream(
    repo_name: str,
    num_commits: int,
    num_files: int,
    num_refs: int,
    seed: int,
) -> tuple[bytes, dict[str, Any]]:
    """
    Constructs a deterministic git fast-import stream.
    Produces:
    - Exactly num_commits commits reachable from HEAD
    - Base commit on refs/heads/main
    - Divergent branch (refs/heads/feat/divergent)
    - Merge commit into main with 2 parent commits
    - Additional secondary branches and tags to reach exactly num_refs
    - Exactly num_files tracked paths in HEAD
    """
    rng = random.Random(seed)
    lines: list[str] = []

    author_name = "Benchmarker"
    author_email = "benchmark@example.com"
    base_time = 1700000000

    actual_commits = max(4, num_commits)
    actual_files = max(8, num_files)
    actual_refs = max(2, num_refs)

    # Plan file list to achieve EXACTLY actual_files in HEAD
    fixed_in_c1 = [
        "README.md",
        "package.json",
        "src/staged_and_working.ts",
        "src/rename_src.ts",
        "src/delete_staged.ts",
        "src/delete_unstaged.ts",
    ]
    c2_file = "src/feature_divergent.ts"
    c3_file = "src/mainline_update.ts"
    all_fixed = fixed_in_c1 + [c2_file, c3_file]  # 8 files

    num_numbered = max(0, actual_files - len(all_fixed))
    numbered_paths = [f"src/file_{i:05d}.ts" for i in range(1, num_numbered + 1)]

    # Split numbered paths across C1 and subsequent mainline commits
    c1_numbered_count = len(numbered_paths) // 2
    c1_numbered = numbered_paths[:c1_numbered_count]
    rem_numbered = numbered_paths[c1_numbered_count:]

    # Commit 1: Base commit on refs/heads/main
    mark_counter = 1
    lines.append("commit refs/heads/main")
    lines.append(f"mark :{mark_counter}")
    lines.append(f"author {author_name} <{author_email}> {base_time} +0000")
    lines.append(f"committer {author_name} <{author_email}> {base_time} +0000")
    c1_msg = f"feat({repo_name}): initial base commit"
    lines.append(f"data {len(c1_msg.encode('utf-8'))}")
    lines.append(c1_msg)

    # Base contents
    c1_contents: dict[str, str] = {
        "README.md": f"# {repo_name}\nDeterministic benchmark repository.\n",
        "package.json": json.dumps({"name": repo_name, "version": "1.0.0"}, indent=2) + "\n",
        "src/staged_and_working.ts": (
            "// Base committed version (Version 0)\n"
            f"export const REPO_NAME = '{repo_name}';\n"
            "export const STATE_VERSION = 0;\n"
        ),
        "src/rename_src.ts": (
            "// File destined for staged rename\n"
            "export function originalHelper() { return 'rename_me'; }\n"
        ),
        "src/delete_staged.ts": (
            "// File destined for staged delete\n"
            "export const DELETE_STAGED = true;\n"
        ),
        "src/delete_unstaged.ts": (
            "// File destined for unstaged delete\n"
            "export const DELETE_UNSTAGED = true;\n"
        ),
    }
    for p in c1_numbered:
        c1_contents[p] = f"// File {p} in {repo_name}\nexport const id = '{p}';\n"

    for p, content in sorted(c1_contents.items()):
        b = content.encode("utf-8")
        lines.append(f"M 644 inline {p}")
        lines.append(f"data {len(b)}")
        lines.append(content.rstrip("\n"))
    lines.append("")

    base_mark = mark_counter

    # Commit 2: Divergent branch commit
    mark_counter = 2
    branch_name = "feat/divergent"
    c2_time = base_time + mark_counter * 60
    lines.append(f"commit refs/heads/{branch_name}")
    lines.append(f"mark :{mark_counter}")
    lines.append(f"author {author_name} <{author_email}> {c2_time} +0000")
    lines.append(f"committer {author_name} <{author_email}> {c2_time} +0000")
    c2_msg = f"feat({branch_name}): divergent branch implementation"
    lines.append(f"data {len(c2_msg.encode('utf-8'))}")
    lines.append(c2_msg)
    lines.append(f"from :{base_mark}")
    c2_content = f"// Divergent feature file\nexport const FEATURE_BRANCH = '{branch_name}';\n"
    lines.append(f"M 644 inline {c2_file}")
    lines.append(f"data {len(c2_content.encode('utf-8'))}")
    lines.append(c2_content.rstrip("\n"))
    lines.append("")

    divergent_mark = mark_counter

    # Commit 3: Mainline commit while divergent branch was active
    mark_counter = 3
    c3_time = base_time + mark_counter * 60
    lines.append("commit refs/heads/main")
    lines.append(f"mark :{mark_counter}")
    lines.append(f"author {author_name} <{author_email}> {c3_time} +0000")
    lines.append(f"committer {author_name} <{author_email}> {c3_time} +0000")
    c3_msg = f"chore({repo_name}): mainline maintenance step"
    lines.append(f"data {len(c3_msg.encode('utf-8'))}")
    lines.append(c3_msg)
    lines.append(f"from :{base_mark}")
    c3_content = f"// Mainline update\nexport const MAINLINE_STEP = {mark_counter};\n"
    lines.append(f"M 644 inline {c3_file}")
    lines.append(f"data {len(c3_content.encode('utf-8'))}")
    lines.append(c3_content.rstrip("\n"))
    lines.append("")

    main_before_merge = mark_counter

    # Commit 4: True merge commit of divergent branch into main (2 parents)
    mark_counter = 4
    c4_time = base_time + mark_counter * 60
    lines.append("commit refs/heads/main")
    lines.append(f"mark :{mark_counter}")
    lines.append(f"author {author_name} <{author_email}> {c4_time} +0000")
    lines.append(f"committer {author_name} <{author_email}> {c4_time} +0000")
    c4_msg = f"Merge branch '{branch_name}' into main"
    lines.append(f"data {len(c4_msg.encode('utf-8'))}")
    lines.append(c4_msg)
    lines.append(f"from :{main_before_merge}")
    lines.append(f"merge :{divergent_mark}")
    # Explicitly include the divergent file into the merged tree
    lines.append(f"M 644 inline {c2_file}")
    lines.append(f"data {len(c2_content.encode('utf-8'))}")
    lines.append(c2_content.rstrip("\n"))
    lines.append("")

    merge_mark = mark_counter
    current_head_mark = merge_mark

    # Commits 5 .. actual_commits are added to refs/heads/main
    # Distribute the remaining paths to reach exactly actual_files
    rem_idx = 0
    commits_remaining = max(1, actual_commits - 4)

    for step in range(1, commits_remaining + 1):
        mark_counter = 4 + step
        c_time = base_time + mark_counter * 60
        lines.append("commit refs/heads/main")
        lines.append(f"mark :{mark_counter}")
        lines.append(f"author {author_name} <{author_email}> {c_time} +0000")
        lines.append(f"committer {author_name} <{author_email}> {c_time} +0000")
        msg = f"chore({repo_name}): progressive enhancement {mark_counter}"
        lines.append(f"data {len(msg.encode('utf-8'))}")
        lines.append(msg)
        lines.append(f"from :{current_head_mark}")

        steps_left = commits_remaining - step + 1
        n_to_add = (len(rem_numbered) - rem_idx + steps_left - 1) // steps_left
        if n_to_add > 0:
            for p in rem_numbered[rem_idx : rem_idx + n_to_add]:
                content = f"// File {p} added in commit {mark_counter}\nexport const id = '{p}';\n"
                lines.append(f"M 644 inline {p}")
                b_cnt = content.encode("utf-8")
                lines.append(f"data {len(b_cnt)}")
                lines.append(content.rstrip("\n"))
            rem_idx += n_to_add
        else:
            # Modify existing README
            mod_content = f"# {repo_name}\nDeterministic benchmark repository.\nUpdated at commit {mark_counter}.\n"
            lines.append("M 644 inline README.md")
            b_cnt = mod_content.encode("utf-8")
            lines.append(f"data {len(b_cnt)}")
            lines.append(mod_content.rstrip("\n"))

        lines.append("")
        current_head_mark = mark_counter

    # We now have exactly actual_commits commits in refs/heads/main.
    # Now generate refs to reach exactly actual_refs.
    # Currently existing refs: refs/heads/main (1) and refs/heads/feat/divergent (2).
    extra_refs_needed = max(0, actual_refs - 2)
    branches_to_create = extra_refs_needed // 2
    tags_to_create = extra_refs_needed - branches_to_create

    for b_idx in range(1, branches_to_create + 1):
        target_mark = max(1, min(current_head_mark, 1 + (b_idx * (current_head_mark // (branches_to_create + 1)))))
        lines.append(f"reset refs/heads/topic-{b_idx:02d}")
        lines.append(f"from :{target_mark}")
        lines.append("")

    for t_idx in range(1, tags_to_create + 1):
        target_mark = max(1, min(current_head_mark, 1 + (t_idx * (current_head_mark // (tags_to_create + 1)))))
        lines.append(f"reset refs/tags/v0.{t_idx}.0")
        lines.append(f"from :{target_mark}")
        lines.append("")

    stream_bytes = "\n".join(lines).encode("utf-8")
    metadata = {
        "merge_mark": merge_mark,
        "divergent_mark": divergent_mark,
        "main_before_merge": main_before_merge,
        "total_commits": mark_counter,
    }
    return stream_bytes, metadata


def apply_working_tree_states(repo_dir: str, repo_index: int) -> dict[str, list[str]]:
    """
    Applies deterministic working-tree and index modifications.
    Ensures:
    - repo-01 (and specific others) have samepath stagedAworkingB
    - staged renames, staged deletions, unstaged deletions
    - staged new files, unstaged modifications, untracked files
    """
    states: dict[str, list[str]] = {
        "staged": [],
        "unstaged": [],
        "untracked": [],
        "stagedAWorkingB": [],
        "renamed": [],
        "deleted": [],
    }

    # Helper runners
    def git_run(*args: str) -> str:
        res = subprocess.run(
            ["git", *args],
            cwd=repo_dir,
            capture_output=True,
            text=True,
            check=True,
        )
        return res.stdout.strip()

    # Pattern assignment based on repo_index:
    # repo 0 has ALL rich states including stagedAworkingB
    has_staged_working_b = repo_index in (0, 5, 8, 13)
    has_staged_changes = repo_index in (0, 1, 7, 11)
    has_unstaged_changes = repo_index in (0, 1, 3, 5, 8, 10, 13)
    has_untracked = repo_index in (0, 1, 3, 5, 8, 11)
    has_renames = repo_index in (0, 1, 10)
    has_deletes = repo_index in (0, 2, 10, 13)

    # 1. samepath stagedAworkingB
    # Index holds Version A; working tree holds Version B != Version A != Version 0
    if has_staged_working_b:
        target_file = os.path.join(repo_dir, "src/staged_and_working.ts")
        if os.path.exists(target_file):
            # Write Version A into file and stage it
            with open(target_file, "w", encoding="utf-8") as f:
                f.write(
                    "// Version A (staged in index)\n"
                    f"export const STATE_VERSION = 'A';\n"
                    f"export const STAGED_IN_INDEX = true;\n"
                )
            git_run("add", "src/staged_and_working.ts")

            # Overwrite working tree with Version B (do NOT git add)
            with open(target_file, "w", encoding="utf-8") as f:
                f.write(
                    "// Version B (working tree modification)\n"
                    f"export const STATE_VERSION = 'B';\n"
                    f"export const WORKING_TREE_DIRTY = true;\n"
                )
            states["stagedAWorkingB"].append("src/staged_and_working.ts")

    # 2. Staged rename
    if has_renames:
        src_file = os.path.join(repo_dir, "src/rename_src.ts")
        if os.path.exists(src_file):
            git_run("mv", "src/rename_src.ts", "src/rename_dest.ts")
            states["renamed"].append("src/rename_src.ts -> src/rename_dest.ts")

    # 3. Staged deletion
    if has_deletes:
        del_staged_file = os.path.join(repo_dir, "src/delete_staged.ts")
        if os.path.exists(del_staged_file):
            git_run("rm", "src/delete_staged.ts")
            states["deleted"].append("src/delete_staged.ts")

    # 4. Unstaged deletion
    if has_deletes:
        del_unstaged_file = os.path.join(repo_dir, "src/delete_unstaged.ts")
        if os.path.exists(del_unstaged_file):
            os.remove(del_unstaged_file)
            states["deleted"].append("src/delete_unstaged.ts")

    # 5. Staged new file
    if has_staged_changes:
        staged_new = os.path.join(repo_dir, "src/new_staged_feature.ts")
        with open(staged_new, "w", encoding="utf-8") as f:
            f.write("// Staged new feature\nexport const STAGED_FEATURE = true;\n")
        git_run("add", "src/new_staged_feature.ts")
        states["staged"].append("src/new_staged_feature.ts")

    # 6. Unstaged modification on tracked file
    if has_unstaged_changes:
        readme = os.path.join(repo_dir, "README.md")
        if os.path.exists(readme):
            with open(readme, "a", encoding="utf-8") as f:
                f.write("\n<!-- Unstaged working tree note -->\n")
            states["unstaged"].append("README.md")

    # 7. Untracked files
    if has_untracked:
        env_file = os.path.join(repo_dir, ".env.local")
        with open(env_file, "w", encoding="utf-8") as f:
            f.write("SECRET_KEY=bench_12345\n")
        states["untracked"].append(".env.local")

        scratch_file = os.path.join(repo_dir, "scratch.log")
        with open(scratch_file, "w", encoding="utf-8") as f:
            f.write("temporary build scratch log\n")
        states["untracked"].append("scratch.log")

    return states


def inspect_repository(repo_dir: str) -> dict[str, Any]:
    """Inspects a generated git repository using git plumbing oracle commands."""

    def git_run(*args: str) -> str:
        res = subprocess.run(
            ["git", *args],
            cwd=repo_dir,
            capture_output=True,
            text=True,
            check=True,
        )
        return res.stdout.strip()

    head_oid = git_run("rev-parse", "HEAD")
    head_branch = git_run("rev-parse", "--abbrev-ref", "HEAD")
    commit_count = int(git_run("rev-list", "--count", "HEAD"))
    porcelain_status = git_run("status", "--porcelain")
    tracked_paths_count = len(git_run("ls-tree", "-r", "--name-only", "HEAD").splitlines())

    # Find merge commit in history
    merge_line = git_run("rev-list", "--parents", "--merges", "-n", "1", "HEAD")
    merge_commit_oid = None
    merge_parents: list[str] = []
    if merge_line:
        parts = merge_line.split()
        if len(parts) >= 3:
            merge_commit_oid = parts[0]
            merge_parents = parts[1:]

    # Refs listing
    refs_output = git_run("for-each-ref", "--format=%(refname)")
    all_refs = refs_output.splitlines() if refs_output else []

    return {
        "headOid": head_oid,
        "headBranch": head_branch,
        "commitCount": commit_count,
        "refCount": len(all_refs),
        "trackedPathsCount": tracked_paths_count,
        "hasMergeCommit": merge_commit_oid is not None,
        "mergeCommitOid": merge_commit_oid,
        "mergeParents": merge_parents,
        "porcelainStatus": porcelain_status,
        "allRefs": all_refs,
    }


def generate_workload(
    target_dir: str,
    preset: str = "medium",
    num_repos: int | None = None,
    num_files: int | None = None,
    num_commits: int | None = None,
    num_refs: int | None = None,
    seed: int = 42,
    workload_revision: str = WORKLOAD_REVISION,
    manifest_path: str | None = None,
    quiet: bool = False,
) -> dict[str, Any]:
    """
    Main generator routine.
    Validates target directory, generates repositories via fast-import,
    applies working-tree states, verifies with oracle checks, and writes manifest.
    """
    preset_cfg = PRESETS.get(preset, PRESETS["medium"])
    actual_repos = num_repos if num_repos is not None else preset_cfg["repos"]
    actual_files = num_files if num_files is not None else preset_cfg["files"]
    actual_commits = num_commits if num_commits is not None else preset_cfg["commits"]
    actual_refs = num_refs if num_refs is not None else preset_cfg["refs"]

    abs_target_dir, was_created = validate_and_prepare_target_dir(target_dir)

    if not quiet:
        print(f"Generating workload preset '{preset}' in: {abs_target_dir}")
        print(f"  repos={actual_repos}, files={actual_files}, commits={actual_commits}, refs={actual_refs}, seed={seed}")

    repo_manifests: list[dict[str, Any]] = []

    try:
        for idx in range(actual_repos):
            repo_name = REPO_NAMES[idx] if idx < len(REPO_NAMES) else f"repo-{idx+1:02d}-extra"
            r_dir = os.path.join(abs_target_dir, repo_name)
            os.makedirs(r_dir, exist_ok=False)

            # Initialize repo
            subprocess.run(
                ["git", "init", "-q", "--initial-branch=main"],
                cwd=r_dir,
                check=True,
            )
            subprocess.run(["git", "config", "user.name", "Benchmarker"], cwd=r_dir, check=True)
            subprocess.run(["git", "config", "user.email", "benchmark@example.com"], cwd=r_dir, check=True)
            subprocess.run(["git", "config", "commit.gpgSign", "false"], cwd=r_dir, check=True)

            # Fast-import stream
            repo_seed = seed + idx * 1000
            stream_bytes, meta = generate_fast_import_stream(
                repo_name=repo_name,
                num_commits=actual_commits,
                num_files=actual_files,
                num_refs=actual_refs,
                seed=repo_seed,
            )

            p = subprocess.Popen(
                ["git", "fast-import", "--quiet"],
                cwd=r_dir,
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
            stdout_data, stderr_data = p.communicate(input=stream_bytes)
            if p.returncode != 0:
                raise RuntimeError(
                    f"git fast-import failed in {r_dir} with code {p.returncode}: {stderr_data.decode()}"
                )

            # Checkout working tree to populate files
            subprocess.run(["git", "reset", "--hard", "HEAD"], cwd=r_dir, check=True, capture_output=True)

            # Apply state modifications (staged, unstaged, samepath stagedAworkingB, etc.)
            state_details = apply_working_tree_states(r_dir, idx)

            # Oracle inspection
            oracle = inspect_repository(r_dir)

            repo_record = {
                "name": repo_name,
                "path": r_dir,
                "headOid": oracle["headOid"],
                "headBranch": oracle["headBranch"],
                "commitCount": oracle["commitCount"],
                "refCount": oracle["refCount"],
                "trackedPathsCount": oracle["trackedPathsCount"],
                "hasMergeCommit": oracle["hasMergeCommit"],
                "mergeCommitOid": oracle["mergeCommitOid"],
                "mergeParents": oracle["mergeParents"],
                "porcelainStatus": oracle["porcelainStatus"],
                "states": state_details,
            }
            repo_manifests.append(repo_record)

            if not quiet and (idx + 1) % 5 == 0:
                print(f"  generated {idx + 1}/{actual_repos} repos...")

        # Aggregate summary
        total_staged = sum(len(r["states"]["staged"]) for r in repo_manifests)
        total_unstaged = sum(len(r["states"]["unstaged"]) for r in repo_manifests)
        total_untracked = sum(len(r["states"]["untracked"]) for r in repo_manifests)
        total_staged_working_b = sum(len(r["states"]["stagedAWorkingB"]) for r in repo_manifests)
        total_renamed = sum(len(r["states"]["renamed"]) for r in repo_manifests)
        total_deleted = sum(len(r["states"]["deleted"]) for r in repo_manifests)

        manifest_data = {
            "workloadRevision": workload_revision,
            "seed": seed,
            "preset": preset,
            "timestampUtc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
            "targetDir": abs_target_dir,
            "parameters": {
                "repos": actual_repos,
                "filesPerRepo": actual_files,
                "commitsPerRepo": actual_commits,
                "refsPerRepo": actual_refs,
            },
            "summary": {
                "totalRepos": len(repo_manifests),
                "totalCommits": sum(r["commitCount"] for r in repo_manifests),
                "totalRefs": sum(r["refCount"] for r in repo_manifests),
                "totalTrackedPaths": sum(r["trackedPathsCount"] for r in repo_manifests),
                "stagedCount": total_staged,
                "unstagedCount": total_unstaged,
                "untrackedCount": total_untracked,
                "stagedAWorkingBCount": total_staged_working_b,
                "renameCount": total_renamed,
                "deleteCount": total_deleted,
            },
            "repos": repo_manifests,
        }

        # Write manifest
        out_manifest = manifest_path or os.path.join(abs_target_dir, "workload_manifest.json")
        with open(out_manifest, "w", encoding="utf-8") as f:
            json.dump(manifest_data, f, indent=2)

        if not quiet:
            print(f"Workload successfully generated in: {abs_target_dir}")
            print(f"Manifest written to: {out_manifest}")

        return manifest_data

    except Exception:
        # If generator created the target_dir itself in this run, clean it up on failure
        if was_created and os.path.exists(abs_target_dir):
            shutil.rmtree(abs_target_dir, ignore_errors=True)
        raise


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Deterministic Git Workload Generator for snip-sync benchmark harness."
    )
    parser.add_argument(
        "target",
        nargs="?",
        default=None,
        help="Target directory for generating repositories (must be non-existent or empty).",
    )
    parser.add_argument(
        "--target-dir",
        dest="target_dir_flag",
        default=None,
        help="Target directory (alternative flag form).",
    )
    parser.add_argument(
        "--preset",
        choices=["smoke", "medium", "standard"],
        default="medium",
        help="Workload preset (default: medium).",
    )
    parser.add_argument(
        "--repos",
        type=int,
        default=None,
        help="Number of repositories (overrides preset).",
    )
    parser.add_argument(
        "--files",
        type=int,
        default=None,
        help="Tracked paths per repository (overrides preset).",
    )
    parser.add_argument(
        "--commits",
        type=int,
        default=None,
        help="Commits per repository (overrides preset).",
    )
    parser.add_argument(
        "--refs",
        type=int,
        default=None,
        help="Refs per repository (overrides preset).",
    )
    parser.add_argument(
        "--seed",
        type=int,
        default=42,
        help="Deterministic pseudorandom seed (default: 42).",
    )
    parser.add_argument(
        "--workload-revision",
        default=WORKLOAD_REVISION,
        help=f"Workload revision label (default: {WORKLOAD_REVISION}).",
    )
    parser.add_argument(
        "--manifest",
        default=None,
        help="Path to output manifest JSON (default: <target-dir>/workload_manifest.json).",
    )
    parser.add_argument(
        "-q",
        "--quiet",
        action="store_true",
        help="Suppress console progress output.",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv or sys.argv[1:])
    target_path = args.target_dir_flag or args.target
    if not target_path:
        target_path = tempfile.mkdtemp(prefix="snip-workload-")

    try:
        generate_workload(
            target_dir=target_path,
            preset=args.preset,
            num_repos=args.repos,
            num_files=args.files,
            num_commits=args.commits,
            num_refs=args.refs,
            seed=args.seed,
            workload_revision=args.workload_revision,
            manifest_path=args.manifest,
            quiet=args.quiet,
        )
        # Print path to stdout for shell scripts
        print(os.path.abspath(target_path))
        return 0
    except Exception as e:
        print(f"Error: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
