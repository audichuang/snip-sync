import * as e from "./lib.mjs";

const entry = (path) => `[data-testid="explorer-entry-${path}"]`;
const toggle = async (path, part) => {
	const selector = await e.js(
		`const node = document.querySelector(arguments[0]).closest('.rc-tree-treenode'); node.setAttribute('data-e2e-node', 'target'); return '[data-e2e-node="target"] .rc-tree-' + arguments[1];`,
		entry(path),
		part,
	);
	await e.click(selector);
	await e.js(
		"document.querySelector('[data-e2e-node=target]').removeAttribute('data-e2e-node')",
	);
};

export const ideScenarios = {
	async I01_project_tree_preview_and_folder_copy(check) {
		const a = e.newRepo("workspace 多語");
		e.write(a, "packages/api/a.ts", "api content\n");
		e.write(a, "packages/api/nested/b.ts", "nested content\n");
		e.write(a, "packages/web/c.ts", "web untouched\n");
		const base = e.commit(a, "workspace");
		const b = e.clone(a, base);
		e.write(a, "packages/api/a.ts", "selected api\n");
		await e.setRepo(a);
		await e.openTab("files");
		await e.clickId("source-files");
		await e.find('[data-testid="explorer-entry-packages"]');
		await toggle("packages", "switcher");
		await e.find('[data-testid="explorer-entry-packages/api"]');
		await toggle("packages/api", "switcher");
		await e.clickId("explorer-entry-packages/api/a.ts");
		await e.until(
			"source preview",
			async () =>
				(await e.js(
					'return document.querySelector("[data-testid=source-content]")?.textContent',
				)) === "selected api\n",
		);
		check(
			(await e.attr("source-preview", "data-path")) ===
				"packages/api/a.ts",
			"preview is the selected file",
		);
		await toggle("packages/api", "checkbox");
		await e.shot("I01-project-workspace");
		await e.withToast(() => e.clickId("copy-files"));
		check(
			(await e.restoreFiles(b)) === "ok",
			"checked directory copies recursively, including unopened nested folders",
		);
		check(
			e.read(b, "packages/api/a.ts") === "selected api",
			"chosen file content copied",
		);
		check(
			e.read(b, "packages/api/nested/b.ts") === "nested content",
			"unopened descendant copied",
		);
		check(
			e.readLf(b, "packages/web/c.ts") === "web untouched\n",
			"sibling package untouched",
		);
	},
	async I02_all_refs_and_non_head_root_selection(check) {
		const a = e.newRepo("branches");
		e.write(a, "base.txt", "base\n");
		const root = e.commit(a, "root");
		e.git(a, "checkout", "-qb", "feature/ui");
		e.write(a, "packages/ui/view.ts", "feature snapshot\n");
		const side = e.commit(a, "unmerged UI work");
		e.git(a, "update-ref", "refs/remotes/origin/feature/ui", side);
		e.git(a, "tag", "v-preview", side);
		e.git(a, "checkout", "-q", "main");
		e.write(a, "main-only.txt", "main only\n");
		const main = e.commit(a, "main work");
		await e.setRepo(a);
		await e.openTab("commits");
		await e.find(`[data-commit="${side}"]`);
		check(
			await e.present("ref-refs/heads/feature/ui"),
			"unmerged local branch visible",
		);
		check(
			await e.present("ref-refs/remotes/origin/feature/ui"),
			"remote-tracking branch visible",
		);
		check(await e.present("ref-refs/tags/v-preview"), "tag visible");
		await e.click(`[data-commit="${side}"] span`);
		await e.find('[data-testid="preview-file-packages/ui/view.ts"]');
		await e.clickId("preview-content");
		await e.until(
			"commit source content",
			async () =>
				(await e.js(
					'return document.querySelector("[data-testid=source-content]")?.textContent',
				)) === "feature snapshot\n",
		);
		await e.shot("I02-branch-graph-preview");
		await e.clickId("ref-refs/heads/feature/ui");
		await e.until(
			"branch filter",
			async () =>
				(
					await e.js(
						"return [...document.querySelectorAll('[data-commit]')].map(e => e.dataset.commit)",
					)
				).length === 2,
		);
		check(
			!(await e.js(
				"return !!document.querySelector(arguments[0])",
				`[data-commit="${main}"]`,
			)),
			"unrelated main commit absent from branch filter",
		);
		await e.copyCommits(a, side, root);
		const b = e.newRepo("empty-target");
		check(
			(await e.replayCommits(b)) === "ok",
			"root-inclusive range off HEAD replays",
		);
		check(
			e.git(b, "log", "--format=%s") === "unmerged UI work\nroot",
			"exact selected branch history copied",
		);
		check(
			!e.exists(b, "main-only.txt") &&
				e.read(b, "packages/ui/view.ts") === "feature snapshot\n",
			"no files leaked from HEAD",
		);
		check(
			e.git(a, "rev-parse", "HEAD") === main,
			"browsing does not change checkout",
		);
	},
	async I03_history_paging_and_search(check) {
		const a = e.newRepo("long-history");
		const tree = e.git(a, "mktree");
		let tip;
		let first;
		for (let i = 0; i < 305; i++) {
			tip = e.git(
				a,
				"-c",
				"user.name=A",
				"-c",
				"user.email=a@x",
				"commit-tree",
				tree,
				...(tip ? ["-p", tip] : []),
				"-m",
				i === 0 ? "needle oldest commit" : `history ${i}`,
			);
			first ??= tip;
		}
		e.git(a, "update-ref", "refs/heads/main", tip);
		await e.setRepo(a);
		await e.openTab("commits");
		await e.find('[data-testid="load-more-history"]');
		check(
			(await e.js(
				"return document.querySelectorAll('[data-commit]').length",
			)) === 300,
			"first history page is bounded and visibly extendable",
		);
		await e.clickId("load-more-history");
		await e.find(`[data-commit="${first}"]`);
		check(
			(await e.js(
				"return document.querySelectorAll('[data-commit]').length",
			)) === 305,
			"older history loads without omissions",
		);
		await e.setInput('[data-testid="history-search"]', "needle oldest");
		await e.until(
			"search full history",
			async () =>
				(await e.js(
					"return [...document.querySelectorAll('[data-commit]')].map(e => e.dataset.commit).join()",
				)) === first,
		);
		check(
			!(await e.present("load-more-history")),
			"search finds old commit and reflects exhaustion",
		);
	},
};
