// Real-app scenarios: every scenario builds fresh git repos, drives the app
// (Rust backend + WebView) through tauri-driver, and checks the outcome with
// git. Exits non-zero if any scenario fails.
//
//   bun run tauri build --debug --no-bundle
//   xvfb-run -a node e2e/scenarios.mjs            # Linux (WebKitWebDriver)
//   node e2e/scenarios.mjs                        # Windows (msedgedriver on PATH)
//
// SNIP_APP overrides the app binary, SNIP_E2E_OUT the screenshot directory,
// SNIP_E2E_ONLY=C3,F1 runs a subset (name prefixes).

import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import * as e from "./lib.mjs";

const { git, write, read, readLf, exists, commit, newRepo, newFolder, clone } =
	e;
const here = path.dirname(fileURLToPath(import.meta.url));
const exe = process.platform === "win32" ? "snip-sync.exe" : "snip-sync";
const APP =
	process.env.SNIP_APP ?? path.join(here, "../../../target/debug", exe);
const OUT =
	process.env.SNIP_E2E_OUT ??
	mkdtempSync(path.join(tmpdir(), "snip-e2e-shots-"));
const ONLY = (process.env.SNIP_E2E_ONLY ?? "").split(",").filter(Boolean);

const tree = (repo) => git(repo, "ls-tree", "-r", "HEAD");
const log = (repo, n) => git(repo, "log", `-${n}`, "--format=%s|%an <%ae>|%aI");
const count = (repo, range) => git(repo, "rev-list", "--count", range);

const scenarios = {
	// The monorepo sync from the original smoke test: add, rename, delete and
	// a binary that must be reported as not copied.
	async S00_monorepo_three_commits(check) {
		const a = newRepo("a");
		write(a, "README.md", "# monorepo\n");
		write(a, "packages/api/src/index.ts", "export const port = 3000;\n");
		write(
			a,
			"packages/web/src/app.tsx",
			"export const App = () => null;\n",
		);
		write(a, "scripts/legacy.sh", "echo legacy\n");
		const base = commit(a, "chore: initial monorepo");
		write(a, "packages/api/src/index.ts", "export const port = 8080;\n");
		write(
			a,
			"packages/api/src/health.ts",
			"export const health = () => 'ok';\n",
		);
		write(
			a,
			"packages/web/public/logo.png",
			Buffer.from([0x89, 0x50, 0x4e, 0x47, 0, 0xff, 0xfe]),
		);
		const c1 = commit(a, "feat(api): add health endpoint");
		git(a, "mv", "packages/web/src/app.tsx", "packages/web/src/main.tsx");
		write(
			a,
			"packages/web/src/main.tsx",
			"export const App = () => 'hi';\n",
		);
		commit(a, "refactor(web): rename app to main");
		git(a, "rm", "-q", "scripts/legacy.sh");
		const c3 = commit(a, "chore: remove legacy script");
		const b = clone(a, base);
		await e.copyCommits(a, c3, c1);
		check((await e.replayCommits(b)) === "ok", "replay reports success");
		check(
			log(a, 3) === log(b, 3),
			"same messages, authors and author dates",
		);
		const noLogo = (r) =>
			tree(r)
				.split("\n")
				.filter((l) => !l.endsWith("logo.png"))
				.join("\n");
		check(noLogo(a) === noLogo(b), "same tree apart from the binary");
		check(
			!git(b, "ls-tree", "-r", "--name-only", "HEAD").includes(
				"logo.png",
			),
			"binary not written",
		);
	},

	async C01_target_has_its_own_commit(check) {
		const a = newRepo("a");
		write(a, "app.ts", "v1\n");
		const base = commit(a, "base");
		write(a, "app.ts", "v2\n");
		const c1 = commit(a, "feat: v2");
		write(a, "lib.ts", "lib\n");
		const c2 = commit(a, "feat: lib");
		const b = clone(a, base);
		write(b, "local.md", "mine\n");
		commit(b, "local: b only");
		await e.copyCommits(a, c2, c1);
		check((await e.replayCommits(b)) === "ok", "replay reports success");
		check(
			git(b, "log", "-3", "--format=%s") ===
				"feat: lib\nfeat: v2\nlocal: b only",
			"stacked on B's own commit",
		);
		check(read(b, "local.md") === "mine\n", "B's own file kept");
		check(
			read(b, "app.ts") === "v2\n" && read(b, "lib.ts") === "lib\n",
			"A's content arrived",
		);
	},

	async C02_dirty_target_and_unrelated_staged_file(check) {
		const a = newRepo("a");
		write(a, "x.txt", "x0\n");
		const base = commit(a, "base");
		write(a, "x.txt", "x-from-a\n");
		const c1 = commit(a, "change x");
		const b = clone(a, base);
		write(b, "x.txt", "uncommitted in b\n");
		write(b, "y.txt", "staged only\n");
		git(b, "add", "y.txt");
		await e.copyCommits(a, c1, c1);
		check((await e.replayCommits(b)) === "ok", "replay reports success");
		check(
			read(b, "x.txt") === "x-from-a\n",
			"touched path overwritten (spec 4.3)",
		);
		check(
			!git(b, "show", "--name-only", "--format=", "HEAD").includes(
				"y.txt",
			),
			"staged y.txt not in the commit",
		);
		check(
			git(b, "diff", "--cached", "--name-only") === "y.txt",
			"y.txt still staged",
		);
	},

	async C03_unicode_multiline_crlf_exact_bytes(check) {
		const a = newRepo("a");
		write(a, "README.md", "r\n");
		const base = commit(a, "base");
		write(a, "文件/說明.md", "中文內容 🎉\n第二行");
		write(a, "win.txt", "line1\r\nline2\r\n");
		write(a, "tabs and spaces.txt", "\tindent\n  \n\n");
		const c1 = commit(
			a,
			"feat: 中文標題\n\n內文第一行\n- bullet 🎉\n\nSigned-off-by: Alice <alice@example.com>",
		);
		const b = clone(a, base);
		await e.copyCommits(a, c1, c1);
		check((await e.replayCommits(b)) === "ok", "replay reports success");
		check(
			git(a, "log", "-1", "--format=%B") ===
				git(b, "log", "-1", "--format=%B"),
			"full message identical",
		);
		for (const f of ["文件/說明.md", "win.txt", "tabs and spaces.txt"])
			check(
				git(a, "rev-parse", `HEAD:${f}`) ===
					git(b, "rev-parse", `HEAD:${f}`),
				`blob of ${f} identical`,
			);
		check(log(a, 1) === log(b, 1), "author and author date identical");
	},

	async C04_merge_commit_in_range(check) {
		const a = newRepo("a");
		write(a, "main.txt", "m1\n");
		const m1 = commit(a, "m1");
		write(a, "main.txt", "m2\n");
		const m2 = commit(a, "m2");
		git(a, "checkout", "-q", "-b", "feat", m1);
		write(a, "feat.txt", "f1\n");
		commit(a, "f1");
		write(a, "feat.txt", "f2\n");
		commit(a, "f2");
		git(a, "checkout", "-q", "main");
		e.merge(a, "feat", "Merge feat");
		write(a, "main.txt", "m3\n");
		const m3 = commit(a, "m3");
		const b = clone(a, m1);
		await e.copyCommits(a, m3, m2);
		check((await e.replayCommits(b)) === "ok", "replay reports success");
		check(
			git(b, "log", "-3", "--format=%s") === "m3\nMerge feat\nm2",
			"first-parent commits replayed",
		);
		check(count(b, `${m1}..HEAD`) === "3", "linear: the merge is squashed");
		check(tree(a) === tree(b), "final tree equals A");
	},

	async C05_non_contiguous_selection_refused(check) {
		const a = newRepo("a");
		write(a, "main.txt", "1\n");
		commit(a, "m1");
		git(a, "checkout", "-q", "-b", "side");
		write(a, "s.txt", "s\n");
		const s1 = commit(a, "side-1");
		git(a, "checkout", "-q", "main");
		write(a, "main.txt", "2\n");
		commit(a, "m2");
		const top = e.merge(a, "side", "Merge side");
		const toast = await e.copyCommits(a, top, s1);
		// The message names both ends by short hash, in either language.
		check(
			toast.includes(s1.slice(0, 8)) && toast.includes(top.slice(0, 8)),
			`refused with both ends named: ${toast}`,
		);
	},

	async C06_range_with_root_into_empty_repo(check) {
		const a = newRepo("a");
		write(a, "a.txt", "a\n");
		const root = commit(a, "root");
		write(a, "b.txt", "b\n");
		const c1 = commit(a, "second");
		const b = newRepo("empty");
		await e.copyCommits(a, c1, root);
		check((await e.replayCommits(b)) === "ok", "replay reports success");
		check(
			git(b, "log", "--format=%s") === "second\nroot",
			"both commits, root included",
		);
		check(tree(a) === tree(b), "tree equals A");
	},

	async C07_round_trip_back_to_the_source(check) {
		const a = newRepo("a");
		write(a, "f.txt", "0\n");
		const base = commit(a, "base");
		write(a, "f.txt", "from-a\n");
		const ca = commit(a, "a: edit");
		const b = clone(a, base);
		await e.copyCommits(a, ca, ca);
		check((await e.replayCommits(b)) === "ok", "A -> B replay");
		write(b, "f.txt", "from-b\n");
		write(b, "new-in-b.txt", "nb\n");
		const cb = commit(b, "b: edit back");
		await e.copyCommits(b, cb, cb);
		check((await e.replayCommits(a)) === "ok", "B -> A replay");
		check(
			git(a, "log", "-2", "--format=%s") === "b: edit back\na: edit",
			"A got B's commit on top",
		);
		check(
			read(a, "f.txt") === "from-b\n" &&
				read(a, "new-in-b.txt") === "nb\n",
			"A content",
		);
	},

	async C08_into_unborn_branch(check) {
		const a = newRepo("a");
		write(a, "a.txt", "a\n");
		commit(a, "root");
		write(a, "b.txt", "b\n");
		const c1 = commit(a, "add b");
		write(a, "a.txt", "a2\n");
		const c2 = commit(a, "edit a");
		const b = newRepo("unborn");
		await e.copyCommits(a, c2, c1);
		check((await e.replayCommits(b)) === "ok", "replay reports success");
		check(
			git(b, "log", "--format=%s") === "edit a\nadd b",
			"two commits on the unborn branch",
		);
		check(
			read(b, "a.txt") === "a2\n" && read(b, "b.txt") === "b\n",
			"files of the replayed commits",
		);
	},

	async C09_target_is_not_a_git_repo(check) {
		const a = newRepo("a");
		write(a, "a.txt", "a\n");
		commit(a, "root");
		write(a, "a.txt", "a2\n");
		const c1 = commit(a, "edit");
		const plain = newFolder("plain");
		await e.copyCommits(a, c1, c1);
		const kind = await e.previewPaste(plain);
		check(
			kind.startsWith("toast:") &&
				kind.includes("is not inside a git repository"),
			`clear error: ${kind}`,
		);
		check(!exists(plain, "a.txt"), "nothing written");
	},

	async C10_many_commits_many_files(check) {
		const a = newRepo("a");
		write(a, "seed.txt", "s\n");
		const base = commit(a, "base");
		let first;
		let last;
		for (let i = 1; i <= 25; i++) {
			for (let j = 0; j < 8; j++)
				write(
					a,
					`pkg${j}/src/file${i}.ts`,
					`export const v${i}_${j} = ${i * j};\n`.repeat(20),
				);
			last = commit(a, `feat: batch ${i}`);
			first ??= last;
		}
		const b = clone(a, base);
		await e.copyCommits(a, last, first);
		check((await e.replayCommits(b)) === "ok", "replay reports success");
		check(count(b, `${base}..HEAD`) === "25", "25 commits");
		check(tree(a) === tree(b), "tree equals A");
	},

	// ---- file mode, git sources (IDE-compatible format) ----

	async F01_working_tree_to_a_clone(check) {
		const a = newRepo("a");
		write(a, "keep.txt", "k\n");
		write(a, "mod.txt", "old\n");
		write(a, "gone.txt", "g\n");
		const base = commit(a, "base");
		write(a, "mod.txt", "new\n");
		write(a, "src/untracked.ts", "u\n");
		rmSync(path.join(a, "gone.txt"));
		const b = clone(a, base);
		await e.copyGitSource(a, "working");
		check((await e.restoreFiles(b)) === "ok", "restore reports success");
		// The IDE format drops the trailing newline (accepted limitation).
		check(read(b, "mod.txt") === "new", "modified file restored");
		check(read(b, "src/untracked.ts") === "u", "untracked file created");
		check(!exists(b, "gone.txt"), "deleted file removed");
		check(readLf(b, "keep.txt") === "k\n", "untouched file untouched");
	},

	async F02_staged_content_only(check) {
		const a = newRepo("a");
		write(a, "s.txt", "0\n");
		write(a, "w.txt", "0\n");
		const base = commit(a, "base");
		write(a, "s.txt", "staged\n");
		git(a, "add", "s.txt");
		write(a, "s.txt", "edited after staging\n");
		write(a, "w.txt", "worktree only\n");
		const b = clone(a, base);
		await e.copyGitSource(a, "staged");
		check((await e.restoreFiles(b)) === "ok", "restore reports success");
		check(
			read(b, "s.txt") === "staged",
			"index content, not the later edit",
		);
		check(readLf(b, "w.txt") === "0\n", "worktree-only change not copied");
	},

	async F03_single_commit_into_a_plain_folder(check) {
		const a = newRepo("a");
		write(a, "a.txt", "1\n");
		commit(a, "base");
		write(a, "a.txt", "2\n");
		write(a, "b.txt", "b\n");
		const c = commit(a, "c");
		const plain = newFolder("plain");
		await e.copyGitSource(a, "commit", { sha: c });
		check(
			(await e.restoreFiles(plain)) === "ok",
			"restore reports success",
		);
		check(
			read(plain, "a.txt") === "2" && read(plain, "b.txt") === "b",
			"commit's files written",
		);
	},

	async F04_non_utf8_target_is_explained_not_touched(check) {
		const a = newRepo("a");
		write(a, "data.txt", "x\n");
		const base = commit(a, "base");
		write(a, "data.txt", "new text\n");
		const b = clone(a, base);
		const binary = Buffer.from([0xff, 0xfe, 0x41, 0x00]);
		write(b, "data.txt", binary);
		await e.copyGitSource(a, "working");
		check(
			(await e.previewPaste(b)) === "files",
			"the skipped plan is still shown",
		);
		check(
			(await e.bodyText()).includes("UTF-8"),
			"the skip reason is visible",
		);
		check(
			(await e.attr("apply-restore", "disabled")) !== null ||
				(await e.attr("apply-restore", "data-disabled")) !== null,
			"nothing to apply",
		);
		check(
			readFileSync(path.join(b, "data.txt")).equals(binary),
			"non-UTF-8 target untouched",
		);
	},

	async F05_repo_subdirectory_as_workspace(check) {
		const a = newRepo("a");
		write(a, "packages/api/x.ts", "0\n");
		write(a, "other.txt", "o\n");
		const base = commit(a, "base");
		write(a, "packages/api/x.ts", "1\n");
		const b = clone(a, base);
		await e.copyGitSource(path.join(a, "packages", "api"), "working");
		check(
			(await e.restoreFiles(path.join(b, "packages", "api"))) === "ok",
			"restore reports success",
		);
		check(
			read(b, "packages/api/x.ts") === "1",
			"written next to the old file",
		);
		check(!exists(b, "packages/api/packages"), "no doubled path");
	},
};

const results = [];
const stop = await e.start(APP, OUT);
try {
	for (const [name, run] of Object.entries(scenarios)) {
		if (ONLY.length && !ONLY.some((p) => name.startsWith(p))) continue;
		const failures = [];
		const check = (ok, what) => {
			if (!ok) failures.push(what);
		};
		const t0 = Date.now();
		try {
			await run(check);
		} catch (error) {
			failures.push(`error: ${error.message}`);
		}
		const ms = Date.now() - t0;
		if (failures.length) {
			await e.shot(`${name}-failure`).catch(() => undefined);
			await e.saveSource(`${name}-failure`).catch(() => undefined);
		} else {
			await e.shot(`${name}`).catch(() => undefined);
		}
		results.push({ name, ok: failures.length === 0, ms, failures });
		console.log(`${failures.length ? "FAIL" : "PASS"} ${name} (${ms} ms)`);
		for (const f of failures) console.log(`     - ${f}`);
	}
} finally {
	await stop();
	writeFileSync(
		path.join(OUT, "results.json"),
		JSON.stringify(results, null, 2),
	);
}

const failed = results.filter((r) => !r.ok);
console.log(
	`\n${results.length - failed.length}/${results.length} scenarios passed; screenshots in ${OUT}`,
);
if (failed.length || results.length === 0) process.exit(1);
