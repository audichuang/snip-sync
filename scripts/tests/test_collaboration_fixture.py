#!/usr/bin/env python3
"""Focus tests for the functional collaboration acceptance fixture."""

from __future__ import annotations

import base64
import copy
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPTS_DIR = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))
REPO_ROOT = os.path.abspath(os.path.join(SCRIPTS_DIR, ".."))
if SCRIPTS_DIR not in sys.path:
    sys.path.insert(0, SCRIPTS_DIR)

from collaboration_fixture import (  # noqa: E402
    EMPTY_TREE,
    CompareError,
    FixtureSafetyError,
    GitSession,
    VerificationError,
    canonical_dataset_hash,
    compare_step,
    generate,
    git_available,
    missing_git_disposition,
    preview_is_stale,
    snapshot,
    verify,
)

SCRIPT = os.path.join(SCRIPTS_DIR, "collaboration_fixture.py")
REQUIRE_ALL = os.environ.get("SNIP_REQUIRE_ALL_TESTS") is not None


def run_cli(args: list[str], *, env: dict[str, str] | None = None, timeout: int = 300) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, SCRIPT, *args],
        capture_output=True,
        text=True,
        env=env,
        timeout=timeout,
        check=False,
    )


def require_git(test: unittest.TestCase) -> None:
    if git_available():
        return
    if REQUIRE_ALL:
        test.fail("git is required because SNIP_REQUIRE_ALL_TESTS=1")
    test.skipTest("git is not installed; collaboration fixture tests need real git")


def repo_record(manifest: dict, repo_id: str) -> dict:
    return next(item for item in manifest["repos"] if item["repoId"] == repo_id)


def step_record(manifest: dict, step_id: str) -> dict:
    return next(item for item in manifest["steps"] if item["id"] == step_id)


def worktree(root: str, manifest: dict, repo_id: str) -> Path:
    return Path(root) / repo_record(manifest, repo_id)["relativePath"]


class SafetyTests(unittest.TestCase):
    def test_reject_nonempty_preserves_existing_file(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            target = Path(tmp) / "occupied"
            target.mkdir()
            precious = target / "keep.txt"
            precious.write_text("do not delete\n", encoding="utf-8")
            with self.assertRaises(FixtureSafetyError) as caught:
                generate(str(target))
            self.assertIn("not empty", str(caught.exception))
            self.assertEqual(precious.read_text(encoding="utf-8"), "do not delete\n")

    def test_reject_symlink_target_and_symlink_ancestor(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            real = root / "real"
            real.mkdir()
            link = root / "link"
            link.symlink_to(real, target_is_directory=True)
            with self.assertRaises(FixtureSafetyError) as caught:
                generate(str(link))
            self.assertIn("symlink", str(caught.exception))
            self.assertEqual(list(real.iterdir()), [])

            parent_real = root / "parent-real"
            parent_real.mkdir()
            parent_link = root / "parent-link"
            parent_link.symlink_to(parent_real, target_is_directory=True)
            nested = parent_link / "fixture"
            with self.assertRaises(FixtureSafetyError) as caught:
                generate(str(nested))
            self.assertIn("symlink", str(caught.exception))
            self.assertEqual(list(parent_real.iterdir()), [])


class PolicyTests(unittest.TestCase):
    def test_missing_git_disposition_and_cli_is_nonzero(self) -> None:
        self.assertEqual(missing_git_disposition(True), "fail")
        self.assertEqual(missing_git_disposition(False), "skip")
        with tempfile.TemporaryDirectory(prefix="snip-collab-nopath-") as empty_dir:
            empty = Path(empty_dir)
            env = os.environ.copy()
            for key in ("GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR"):
                env.pop(key, None)
            env["PATH"] = str(empty)
            env["SNIP_REQUIRE_ALL_TESTS"] = "1"
            result = run_cli(["verify", "--fixture", str(empty / "missing")], env=env, timeout=30)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("git-missing", result.stderr)
            env.pop("SNIP_REQUIRE_ALL_TESTS")
            result = run_cli(["generate", "--output", str(empty / "out")], env=env, timeout=30)
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse((empty / "out").exists())


class FixtureTests(unittest.TestCase):
    tmp: tempfile.TemporaryDirectory[str]
    dir1: str
    dir2: str
    manifest: dict
    other: dict

    @classmethod
    def setUpClass(cls) -> None:
        if not git_available():
            if REQUIRE_ALL:
                raise AssertionError("git is required because SNIP_REQUIRE_ALL_TESTS=1")
            raise unittest.SkipTest("git is not installed; collaboration fixture tests need real git")
        cls.tmp = tempfile.TemporaryDirectory(prefix="snip-collab-test-")
        try:
            cls.dir1 = os.path.join(cls.tmp.name, "one")
            cls.dir2 = os.path.join(cls.tmp.name, "two")
            cls.manifest = generate(cls.dir1, log_path=os.path.join(cls.tmp.name, "one.log"))
            cls.other = generate(cls.dir2, log_path=os.path.join(cls.tmp.name, "two.log"))
        except Exception:
            cls.tmp.cleanup()
            raise

    @classmethod
    def tearDownClass(cls) -> None:
        if hasattr(cls, "tmp"):
            cls.tmp.cleanup()

    def test_two_outputs_share_hash_and_are_not_a_benchmark(self) -> None:
        self.assertEqual(self.manifest["datasetHash"], self.other["datasetHash"])
        self.assertEqual(self.manifest, self.other)
        self.assertFalse(self.manifest["standardBenchmark"])
        self.assertEqual(self.manifest["kind"], "functional-collaboration-acceptance")
        self.assertEqual(self.manifest["summary"]["repoCount"], 30)
        self.assertEqual(self.manifest["summary"]["worktreeCount"], 30)
        self.assertEqual(self.manifest["summary"]["originCount"], 15)
        self.assertEqual(self.manifest["summary"]["positiveStepCount"], 9)
        self.assertEqual(self.manifest["summary"]["negativeStepCount"], 9)
        for repo in self.manifest["repos"]:
            self.assertTrue(any(item["path"] == "scratch/ignored.txt" for item in repo["files"]))
        self.assertGreaterEqual(self.manifest["summary"]["minCommitsPerRepo"], 10)
        self.assertLessEqual(self.manifest["summary"]["maxCommitsPerRepo"], 30)
        log = Path(self.tmp.name, "one.log").read_text(encoding="utf-8")
        self.assertIn("fast-import", log)
        self.assertIn(self.manifest["datasetHash"], log)
        self.assertIn("SUMMARY", log)
        self.assertNotIn(self.dir1, json.dumps(self.manifest))

    def test_layout_mappings_and_basename_collisions(self) -> None:
        repos = self.manifest["repos"]
        self.assertEqual(sum(1 for repo in repos if repo["machine"] == "A"), 15)
        self.assertEqual(sum(1 for repo in repos if repo["machine"] == "B"), 15)
        worktrees = list(Path(self.dir1).glob("machine-*/*/*/.git"))
        self.assertEqual(len(worktrees), 30)
        self.assertTrue(all(path.is_dir() for path in worktrees))
        origins = list(Path(self.dir1, "origins").glob("*.git"))
        self.assertEqual(len(origins), 15)
        self.assertTrue(all(not str(path).startswith(str(Path(self.dir1) / "machine-")) for path in origins))
        for pair in self.manifest["pairs"]:
            self.assertNotEqual(pair["aBasename"], pair["bBasename"])
            self.assertTrue(pair["destinationBasenameDiffers"])
        billing = [repo for repo in repos if repo["basename"] == "billing"]
        self.assertEqual({repo["parentDir"] for repo in billing}, {"west", "east"})
        edges = [repo for repo in repos if repo["machine"] == "B" and repo["basename"] == "edge"]
        self.assertEqual({repo["parentDir"] for repo in edges}, {"north", "south"})
        self.assertEqual(repo_record(self.manifest, "a-west-billing")["counterpartRepoId"], "b-north-ledger")
        explicit = step_record(self.manifest, "file-explicit-valid-root-pair01-to-pair15b")
        self.assertTrue(explicit["writes"])
        self.assertNotEqual(explicit["destRepoId"], "b-north-ledger")
        self.assertEqual(explicit["destRepoId"], "b-south-edge")

    def test_real_git_graph_refs_and_staged_versus_working(self) -> None:
        require_git(self)
        root = Path(self.dir1)
        billing = worktree(self.dir1, self.manifest, "a-west-billing")
        record = repo_record(self.manifest, "a-west-billing")
        self.assertTrue(record["graph"]["mergeCommitOids"])
        self.assertTrue(record["graph"]["octopusCommitOids"])
        octopus = next(item for item in record["commits"] if item["oid"] == record["graph"]["octopusCommitOids"][0])
        self.assertGreaterEqual(len(octopus["parents"]), 3)
        self.assertIn("refs/heads/topic", record["graph"]["unmergedTips"])
        self.assertTrue(any(ref["name"] == "refs/remotes/origin/main" for ref in record["refs"]))
        annotated = next(ref for ref in record["refs"] if ref["name"] == "refs/tags/v-annotated")
        self.assertEqual(annotated["type"], "tag")
        self.assertTrue(annotated["peeled"])
        self.assertNotEqual(annotated["peeled"], annotated["oid"])
        with GitSession(None) as git:
            ancestor = git.run(billing, ["merge-base", "--is-ancestor", "refs/heads/topic", "HEAD"], check=False)
            self.assertNotEqual(ancestor.code, 0)
            rename = git.stdout(
                billing,
                ["diff-tree", "-r", "-M", "--raw", "--no-abbrev", "--no-commit-id", "refs/tags/post-merge", "refs/tags/rename-point"],
            )
            self.assertIn("notes/guide.txt", rename)
            self.assertIn("src/drop_me.txt", rename)
            origin = root / "origins/pair-01.git"
            self.assertEqual(git.stdout(origin, ["rev-parse", "--is-bare-repository"]).strip(), "true")
            main_a = git.rev_parse(billing, "refs/heads/main")
            main_b = git.rev_parse(worktree(self.dir1, self.manifest, "b-north-ledger"), "refs/heads/main")
            main_o = git.rev_parse(origin, "refs/heads/main")
            self.assertEqual(main_a, main_b)
            self.assertEqual(main_a, main_o)
            index_oid = git.rev_parse(billing, ":src/samepath.txt")
            head_oid = git.rev_parse(billing, "HEAD:src/samepath.txt")
            index_bytes = git.blob(billing, ["cat-file", "blob", index_oid])
            head_bytes = git.blob(billing, ["cat-file", "blob", head_oid])
            work_bytes = (billing / "src/samepath.txt").read_bytes()
            self.assertEqual(len({index_bytes, head_bytes, work_bytes}), 3)
            staged_oid = git.rev_parse(billing, ":transfer/staged.txt")
            staged_bytes = git.blob(billing, ["cat-file", "blob", staged_oid])
            self.assertNotEqual(staged_bytes, (billing / "transfer/staged.txt").read_bytes())
            mobile = worktree(self.dir1, self.manifest, "a-west-mobile")
            names = git.stdout(mobile, ["ls-files", "--stage", "--", "src/rename_src.txt", "src/rename_dst.txt"])
            self.assertIn("src/rename_dst.txt", names)
            self.assertNotIn("src/rename_src.txt", names)
            self.assertEqual(git.rev_parse(mobile, "HEAD:src/rename_src.txt"), git.rev_parse(mobile, ":src/rename_dst.txt"))
            conflict = worktree(self.dir1, self.manifest, "a-east-infra")
            stages = {
                int(line.split()[2])
                for line in git.stdout(conflict, ["ls-files", "-u", "--", "src/conflict.txt"]).splitlines()
            }
            self.assertEqual(stages, {1, 2, 3})
            text = (conflict / "src/conflict.txt").read_bytes()
            self.assertIn(b"OURS", text)
            self.assertIn(b"THEIRS", text)
            self.assertTrue(repo_record(self.manifest, "a-east-infra")["indexUnmerged"])
            self.assertTrue(repo_record(self.manifest, "a-east-infra")["mergeHead"])
        positive_dests = {step["destRepoId"] for step in self.manifest["steps"] if step["writes"]}
        self.assertNotIn("a-east-infra", positive_dests)
        author = record["commits"][0]
        self.assertNotEqual(author["authorTime"], author["committerTime"])

    def test_verify_rejects_file_head_index_and_manifest(self) -> None:
        require_git(self)
        billing = worktree(self.dir1, self.manifest, "a-west-billing")
        readme = billing / "README.txt"
        original = readme.read_bytes()
        readme.write_bytes(original + b"X")
        try:
            with self.assertRaises(VerificationError):
                verify(self.dir1)
        finally:
            readme.write_bytes(original)
        with GitSession(None) as git:
            old = git.rev_parse(billing, "HEAD")
            parent = git.rev_parse(billing, "HEAD^")
            git.run(billing, ["update-ref", "HEAD", parent])
            try:
                with self.assertRaises(VerificationError):
                    verify(self.dir1)
            finally:
                git.run(billing, ["update-ref", "HEAD", old])
            index = billing / ".git" / "index"
            backup = index.read_bytes()
            blob = b"index-tamper"
            oid = git.stdout(billing, ["hash-object", "-w", "--stdin"], input_bytes=blob).strip()
            git.run(billing, ["update-index", "--cacheinfo", f"100644,{oid},src/samepath.txt"])
            try:
                with self.assertRaises(VerificationError):
                    verify(self.dir1)
            finally:
                index.write_bytes(backup)
        manifest_path = Path(self.dir1) / "manifest.json"
        saved = manifest_path.read_text(encoding="utf-8")
        tampered = json.loads(saved)
        readme_row = next(item for item in tampered["repos"] if item["repoId"] == "a-west-billing")
        row = next(item for item in readme_row["files"] if item["path"] == "README.txt")
        row["base64"] = base64.b64encode(b"tampered").decode("ascii")
        row["sha256"] = hashlib.sha256(b"tampered").hexdigest()
        row["size"] = len(b"tampered")
        tampered["datasetHash"] = canonical_dataset_hash(tampered)
        manifest_path.write_text(json.dumps(tampered), encoding="utf-8")
        try:
            with self.assertRaises(VerificationError):
                verify(self.dir1)
        finally:
            manifest_path.write_text(saved, encoding="utf-8")
        extra = worktree(self.dir1, self.manifest, "a-west-billing") / "src/extra.txt"
        self.assertFalse(extra.exists())
        extra.write_bytes(b"restored\n")
        try:
            with self.assertRaises(VerificationError):
                verify(self.dir1)
        finally:
            extra.unlink()
        tampered = json.loads(saved)
        delete_op = next(
            op
            for op in next(item for item in tampered["steps"] if item["id"] == "file-a-to-b-pair01")["operations"]
            if op["op"] == "delete"
        )
        delete_op["source"]["oid"] = "0" * 40
        tampered["datasetHash"] = canonical_dataset_hash(tampered)
        manifest_path.write_text(json.dumps(tampered), encoding="utf-8")
        try:
            with self.assertRaises(VerificationError):
                verify(self.dir1)
        finally:
            manifest_path.write_text(saved, encoding="utf-8")
        commit_id = "commit-a-to-b-pair02"
        replacements = {
            "authorName": "Tampered Name",
            "authorEmail": "tampered@example.com",
            "authorTime": "1999-01-01T00:00:00+00:00",
            "message": "tampered message\n",
        }
        for field, value in replacements.items():
            tampered = json.loads(saved)
            commit = next(item for item in tampered["steps"] if item["id"] == commit_id)["expectedCommits"][0]
            commit[field] = value
            tampered["datasetHash"] = canonical_dataset_hash(tampered)
            manifest_path.write_text(json.dumps(tampered), encoding="utf-8")
            try:
                with self.assertRaises(VerificationError):
                    verify(self.dir1)
            finally:
                manifest_path.write_text(saved, encoding="utf-8")
        tampered = json.loads(saved)
        expected_file = next(item for item in tampered["steps"] if item["id"] == commit_id)["expectedFiles"][0]
        expected_file["path"] = expected_file["path"] + ".tampered"
        expected_file["size"] = int(expected_file["size"]) + 1
        tampered["datasetHash"] = canonical_dataset_hash(tampered)
        manifest_path.write_text(json.dumps(tampered), encoding="utf-8")
        try:
            with self.assertRaises(VerificationError):
                verify(self.dir1)
        finally:
            manifest_path.write_text(saved, encoding="utf-8")
        checked = verify(self.dir1)
        self.assertEqual(checked["datasetHash"], self.manifest["datasetHash"])
        self.assertEqual(checked["repos"], 30)

    def test_selection_metadata_and_bidirectional_expected_trees(self) -> None:
        require_git(self)
        file_ab = step_record(self.manifest, "file-a-to-b-pair01")
        file_ba = step_record(self.manifest, "file-b-to-a-pair09")
        commit_ab = step_record(self.manifest, "commit-a-to-b-pair02")
        commit_ba = step_record(self.manifest, "commit-b-to-a-unmerged-pair03")
        self.assertTrue(file_ab["writes"] and file_ba["writes"])
        self.assertTrue(commit_ab["writes"] and commit_ba["writes"])
        self.assertIn("working", {op["source"]["kind"] for op in file_ab["operations"]})
        self.assertIn("index", {op["source"]["kind"] for op in file_ab["operations"]})
        self.assertIn("fixed-oid", {op["source"]["kind"] for op in file_ab["operations"]})
        self.assertTrue(any(op["op"] == "delete" for op in file_ab["operations"]))
        for step in (file_ab, file_ba, step_record(self.manifest, "file-explicit-valid-root-pair01-to-pair15b")):
            for op in step["operations"]:
                self.assertEqual(op["source"]["path"], op["dest"]["path"])
                if op["op"] == "write":
                    self.assertTrue(op["overwrite"])
                    self.assertNotEqual(op["destBaselineSha256"], op["expectedSha256"])
        work_delete = next(op for op in file_ab["operations"] if op["op"] == "delete")
        index_delete = next(op for op in file_ba["operations"] if op["op"] == "delete")
        self.assertEqual(work_delete["source"]["kind"], "working")
        self.assertEqual(work_delete["source"]["status"], "D")
        self.assertEqual(index_delete["source"]["kind"], "index")
        self.assertEqual(index_delete["source"]["status"], "D")
        self.assertTrue(work_delete["source"]["inIndex"])
        self.assertFalse(index_delete["source"]["inIndex"])
        self.assertTrue(commit_ab["selection"]["legal"])
        self.assertTrue(commit_ba["selection"]["legal"])
        self.assertNotEqual(commit_ba["selection"]["tipRev"], "HEAD")
        binary = [item for commit in commit_ab["expectedCommits"] for item in commit["notCopied"]]
        self.assertTrue(any(item["reason"] == "binary" for item in binary))
        self.assertTrue(all("assets/tiny.bin" not in commit["treePaths"] for commit in commit_ab["expectedCommits"]))
        self.assertTrue(commit_ab["stagedPreservation"]["remainsStaged"])
        self.assertNotEqual(commit_ab["expectedCommits"][0]["authorTime"], "")
        root_step = step_record(self.manifest, "commit-root-pair05")
        self.assertTrue(root_step["selection"]["includesRoot"])
        self.assertIn("README.txt", {item["path"] for item in root_step["expectedFinalTreeEntries"]})
        merge_step = step_record(self.manifest, "commit-merge-first-parent-pair04")
        self.assertTrue(merge_step["selection"]["includesMerge"])
        self.assertEqual(len(merge_step["expectedCommits"]), 1)
        octopus = step_record(self.manifest, "commit-octopus-first-parent-pair01")
        self.assertEqual(len(octopus["expectedCommits"]), 1)
        self.assertGreaterEqual(octopus["expectedCommits"][0]["sourceParentCount"], 3)
        self.assertTrue(octopus["expectedCommits"][0]["singleParentOnDestination"])
        rename = step_record(self.manifest, "commit-rename-delete-pair08")
        final_paths = {item["path"] for item in rename["expectedFinalTreeEntries"]}
        self.assertIn("notes/guide.txt", final_paths)
        self.assertNotIn("notes/base.txt", final_paths)
        self.assertNotIn("src/feature.txt", final_paths)
        self.assertIn("relay/unmerged-b.txt", final_paths)
        with GitSession(None) as git:
            work_src = worktree(self.dir1, self.manifest, "a-west-billing")
            index_src = worktree(self.dir1, self.manifest, "b-south-accounts")
            self.assertEqual(
                git.blob(work_src, ["diff", "--name-status", "-z", "--", "src/extra.txt"]),
                b"D\0src/extra.txt\0",
            )
            self.assertEqual(
                git.blob(index_src, ["diff", "--cached", "--name-status", "-z", "--", "notes/guide.txt"]),
                b"D\0notes/guide.txt\0",
            )
            source = worktree(self.dir1, self.manifest, "a-west-docs")
            self.assertIn("assets/tiny.bin", git.stdout(source, ["ls-tree", "-r", "--name-only", "refs/heads/local-a"]))
            for step in (octopus, rename):
                trees = direct_replay(
                    git,
                    worktree(self.dir1, self.manifest, step["sourceRepoId"]),
                    worktree(self.dir1, self.manifest, step["destRepoId"]),
                    step["selection"]["oids"],
                )
                self.assertEqual(trees, [item["tree"] for item in step["expectedCommits"]])
        negative = [step for step in self.manifest["steps"] if not step["writes"]]
        reasons = {step["reason"] for step in negative}
        self.assertTrue(
            {
                "cross-repository",
                "discontinuous",
                "target-collision",
                "ambiguous-basename",
                "missing-destination",
                "stale-source",
                "stale-target",
                "overwrite-unauthorized",
                "cancel",
            }
            <= reasons
        )
        for step in negative:
            if step["id"] in {"neg-stale-source", "neg-stale-target"}:
                self.assertEqual(step["expected"], "pre-action-snapshot-after-setup-diff")
                self.assertEqual(step["noWriteSnapshot"], "immediately-before-action-after-setup-diff")
            else:
                self.assertEqual(step["expected"], "full-baseline-snapshot")
        self.assertEqual(step_record(self.manifest, "neg-stale-source")["phase"], "source-export")
        self.assertEqual(step_record(self.manifest, "neg-stale-target")["phase"], "destination-apply")
        ambiguous = step_record(self.manifest, "neg-mapping-ambiguous-basename")
        self.assertEqual(
            sorted(ambiguous["candidateRepoIds"]),
            ["a-east-billing", "a-west-billing"],
        )
        self.assertFalse(step_record(self.manifest, "neg-noncontiguous-tips")["selection"]["legal"])
        self.assertFalse(step_record(self.manifest, "neg-overwrite-unauthorized")["operation"]["authorized"])
        preview = step_record(self.manifest, "neg-stale-source")["preview"]
        live = repo_record(self.manifest, preview["repoId"])
        self.assertFalse(preview_is_stale(preview, live))
        changed = copy.deepcopy(live)
        changed["head"]["oid"] = "0" * 40
        self.assertTrue(preview_is_stale(preview, changed))

    def test_cli_snapshot_compare_and_relocation(self) -> None:
        require_git(self)
        snap_path = Path(self.tmp.name) / "snap.json"
        result = run_cli(["snapshot", "--fixture", self.dir1, "--output", str(snap_path)], timeout=180)
        self.assertEqual(result.returncode, 0, result.stderr)
        snap = json.loads(snap_path.read_text(encoding="utf-8"))
        compare_step(self.dir1, "neg-cancel", snap, "initial")
        compare_step(self.dir1, "file-a-to-b-pair01", snap, "initial")
        with self.assertRaises(CompareError):
            compare_step(self.dir1, "file-a-to-b-pair01", snap, "applied")
        applied = copy.deepcopy(snap)
        dest_id = "b-north-ledger"
        dest = next(item for item in applied["repos"] if item["repoId"] == dest_id)
        step = step_record(self.manifest, "file-a-to-b-pair01")
        files = {item["path"]: item for item in dest["files"]}
        absent = set(dest["absentWorktreePaths"])
        for op in step["operations"]:
            path = op["dest"]["path"]
            if op["op"] == "delete":
                files.pop(path, None)
                absent.add(path)
            else:
                raw = base64.b64decode(op["expectedBase64"])
                files[path] = {
                    "path": path,
                    "sha256": hashlib.sha256(raw).hexdigest(),
                    "size": len(raw),
                    "base64": op["expectedBase64"],
                }
                absent.discard(path)
        dest["files"] = [files[key] for key in sorted(files)]
        dest["absentWorktreePaths"] = sorted(absent)
        compare_step(self.dir1, "file-a-to-b-pair01", applied, "applied")
        commit = step_record(self.manifest, "commit-rename-delete-pair08")
        commit_snap = copy.deepcopy(snap)
        commit_dest = next(item for item in commit_snap["repos"] if item["repoId"] == commit["destRepoId"])
        parent = commit["preserved"]["baselineHeadOid"]
        for index, expected in enumerate(commit["expectedCommits"]):
            oid = f"{index + 1:040x}"
            commit_dest["commits"].append(
                {
                    "oid": oid,
                    "tree": expected["tree"],
                    "parents": [parent],
                    "authorName": expected["authorName"],
                    "authorEmail": expected["authorEmail"],
                    "authorTime": expected["authorTime"],
                    "authorUnix": 0,
                    "committerName": "Local User",
                    "committerEmail": "local@example.com",
                    "committerTime": "2026-01-01T00:00:00+00:00",
                    "committerUnix": 1,
                    "message": expected["message"],
                }
            )
            parent = oid
        commit_dest["head"]["oid"] = parent
        for ref in commit_dest["refs"]:
            if ref["name"] == commit_dest["head"]["branch"]:
                ref["oid"] = parent
        commit_dest["indexEntries"] = commit["expectedIndexEntries"]
        commit_dest["files"] = commit["expectedFiles"]
        commit_dest["absentWorktreePaths"] = commit["expectedAbsentWorktreePaths"]
        compare_step(self.dir1, "commit-rename-delete-pair08", commit_snap, "applied")
        with self.assertRaises(CompareError):
            compare_step(self.dir1, "commit-rename-delete-pair08", snap, "applied")
        dest_repo = worktree(self.dir1, self.manifest, commit["destRepoId"])
        with GitSession(None) as git:
            git.run(dest_repo, ["update-ref", "refs/heads/unexpected-ui-side-effect", "HEAD"])
            try:
                listed = git.stdout(
                    dest_repo,
                    [
                        "for-each-ref",
                        "--format=%(refname)%09%(objectname)%09%(objecttype)%09%(*objectname)",
                        "refs/heads/unexpected-ui-side-effect",
                    ],
                ).strip()
                parts = listed.split("\t")
                name, oid, kind = parts[:3]
                peeled = parts[3] if len(parts) > 3 else ""
                mutated = copy.deepcopy(commit_snap)
                mutated_dest = next(item for item in mutated["repos"] if item["repoId"] == commit["destRepoId"])
                mutated_dest["refs"].append(
                    {"name": name, "oid": oid, "type": kind, "peeled": peeled}
                )
                with self.assertRaises(CompareError) as caught:
                    compare_step(self.dir1, "commit-rename-delete-pair08", mutated, "applied")
                self.assertIn("ref namespace", str(caught.exception))
            finally:
                git.run(dest_repo, ["update-ref", "-d", "refs/heads/unexpected-ui-side-effect"])
        ignored_repo = worktree(self.dir1, self.manifest, "a-west-billing")
        ignored = ignored_repo / "unexpected-ignored.txt"
        exclude = ignored_repo / ".git" / "info" / "exclude"
        exclude_text = exclude.read_text(encoding="utf-8")
        try:
            exclude.write_text(exclude_text + "unexpected-ignored.txt\n", encoding="utf-8")
            ignored.write_bytes(b"ignored-extra")
            with GitSession(None) as git:
                hidden = git.blob(ignored_repo, ["ls-files", "-o", "-z", "--exclude-standard"])
                self.assertNotIn(b"unexpected-ignored.txt", hidden)
                check = git.run(ignored_repo, ["check-ignore", "-q", "unexpected-ignored.txt"], check=False)
                self.assertEqual(check.code, 0)
            dirty = snapshot(self.dir1)
            with self.assertRaises(CompareError):
                compare_step(self.dir1, "neg-cancel", dirty, "initial")
            with self.assertRaises(CompareError):
                compare_step(self.dir1, "commit-rename-delete-pair08", dirty, "applied")
        finally:
            if ignored.exists():
                ignored.unlink()
            exclude.write_text(exclude_text, encoding="utf-8")
        cli = run_cli(
            ["compare-step", "--fixture", self.dir1, "--step", "neg-cancel", "--snapshot", str(snap_path), "--phase", "initial"],
            timeout=60,
        )
        self.assertEqual(cli.returncode, 0, cli.stderr)
        failed = run_cli(
            [
                "compare-step",
                "--fixture",
                self.dir1,
                "--step",
                "commit-a-to-b-pair02",
                "--snapshot",
                str(snap_path),
                "--phase",
                "applied",
            ],
            timeout=60,
        )
        self.assertEqual(failed.returncode, 1)
        moved = Path(self.tmp.name) / "moved"
        shutil.copytree(self.dir1, moved, symlinks=True)
        self.assertNotIn(str(moved), (moved / "manifest.json").read_text(encoding="utf-8"))
        with GitSession(None) as git:
            url = git.stdout(moved / "machine-a/west/billing", ["remote", "get-url", "origin"]).strip()
            self.assertEqual(url, "../../../origins/pair-01.git")
            self.assertNotIn(str(moved), url)
        relocated = verify(str(moved))
        self.assertEqual(relocated["datasetHash"], self.manifest["datasetHash"])


def direct_replay(git: GitSession, src: Path, dst: Path, oids: list[str]) -> list[str]:
    """Independent git replay used only by tests. Does not call the fixture oracle."""
    scratch_root = Path(tempfile.mkdtemp(prefix="snip-collab-direct-"))
    scratch = scratch_root / "repo"
    shutil.copytree(dst, scratch, symlinks=True)
    trees: list[str] = []
    try:
        for oid in oids:
            source = parse_commit(git.blob(src, ["cat-file", "commit", oid]))
            parent = source["parents"][0] if source["parents"] else EMPTY_TREE
            diff = git.stdout(
                src,
                ["diff-tree", "-r", "-M", "--raw", "--no-abbrev", "--no-commit-id", parent, oid],
            )
            paths: list[str] = []
            for record in parse_text_diff(diff):
                if record["status"][:1] == "D":
                    blob = git.blob(src, ["cat-file", "blob", record["oldSha"]])
                    if b"\0" in blob:
                        continue
                    target = scratch / record["path"]
                    if target.exists() or target.is_symlink():
                        target.unlink()
                    paths.append(record["path"])
                    continue
                blob = git.blob(src, ["cat-file", "blob", record["newSha"]])
                if b"\0" in blob:
                    continue
                if record["status"][:1] in {"R", "C"}:
                    old = scratch / record["oldPath"]
                    if old.exists() or old.is_symlink():
                        old.unlink()
                    paths.append(record["oldPath"])
                destination = scratch / record["path"]
                destination.parent.mkdir(parents=True, exist_ok=True)
                destination.write_bytes(blob)
                paths.append(record["path"])
            deduped: list[str] = []
            for path in paths:
                if path not in deduped:
                    deduped.append(path)
            if deduped:
                git.run(scratch, ["--literal-pathspecs", "add", "-A", "-f", "--", *deduped])
            git.run(
                scratch,
                [
                    "--literal-pathspecs",
                    "commit",
                    "--quiet",
                    "--only",
                    "--no-verify",
                    "--allow-empty",
                    "--cleanup=verbatim",
                    "-F",
                    "-",
                    "--",
                    *deduped,
                ],
                input_bytes=source["message"],
                extra_env={
                    "GIT_AUTHOR_NAME": source["authorName"],
                    "GIT_AUTHOR_EMAIL": source["authorEmail"],
                    "GIT_AUTHOR_DATE": source["authorDate"],
                    "GIT_COMMITTER_NAME": "Fixture Committer",
                    "GIT_COMMITTER_EMAIL": "committer@collab.example",
                    "GIT_COMMITTER_DATE": source["committerDate"],
                },
            )
            trees.append(git.rev_parse(scratch, "HEAD^{tree}"))
        return trees
    finally:
        shutil.rmtree(scratch_root)


def parse_commit(body: bytes) -> dict[str, object]:
    header, message = body.split(b"\n\n", 1)
    parents: list[str] = []
    author = ""
    committer = ""
    for line in header.split(b"\n"):
        if line.startswith(b"parent "):
            parents.append(line[7:].decode("ascii"))
        elif line.startswith(b"author "):
            author = line[7:].decode("utf-8")
        elif line.startswith(b"committer "):
            committer = line[10:].decode("utf-8")
    author_name, author_email, author_date = split_ident(author)
    _name, _email, committer_date = split_ident(committer)
    return {
        "parents": parents,
        "message": message,
        "authorName": author_name,
        "authorEmail": author_email,
        "authorDate": author_date,
        "committerDate": committer_date,
    }


def split_ident(value: str) -> tuple[str, str, str]:
    name, rest = value.split(" <", 1)
    email, when = rest.split("> ", 1)
    return name, email, when


def parse_text_diff(text: str) -> list[dict[str, str]]:
    records = []
    for line in text.splitlines():
        if not line:
            continue
        meta, remainder = line.split("\t", 1)
        parts = meta.split(" ")
        status = parts[4]
        if status[:1] in {"R", "C"}:
            old_path, path = remainder.split("\t")
        else:
            old_path, path = "", remainder
        records.append(
            {
                "status": status,
                "oldSha": parts[2],
                "newSha": parts[3],
                "oldPath": old_path,
                "path": path,
            }
        )
    return records


if __name__ == "__main__":
    unittest.main()
