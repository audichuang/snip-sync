// End-to-end: drive the REAL app (Rust backend + WebView frontend) through
// tauri-driver and sync three commits of a small monorepo from clone A to
// clone B via the system clipboard.
//
//   bun run tauri build --debug --no-bundle     # app with the frontend embedded
//   xvfb-run -a node e2e/commit-sync.mjs        # Linux; needs tauri-driver + WebKitWebDriver
//
// SNIP_APP overrides the app binary, SNIP_E2E_OUT the screenshot directory.
// Plain WebDriver over fetch: no test framework, no npm dependencies.

import { execFileSync, spawn } from "node:child_process";
import { mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const workspace = path.resolve(here, "../../..");
const exe = process.platform === "win32" ? "snip-sync.exe" : "snip-sync";
const APP = process.env.SNIP_APP ?? path.join(workspace, "target/debug", exe);
const OUT =
	process.env.SNIP_E2E_OUT ??
	mkdtempSync(path.join(tmpdir(), "snip-e2e-shots-"));
const PORT = 4444;
const DRIVER = `http://127.0.0.1:${PORT}`;

// ---- fixture: a monorepo with a base commit and three commits to sync ----

function git(cwd, ...args) {
	return execFileSync("git", args, {
		cwd,
		encoding: "utf8",
		env: {
			...process.env,
			GIT_CONFIG_GLOBAL: "/dev/null",
			GIT_CONFIG_NOSYSTEM: "1",
		},
	}).trim();
}

function write(root, file, content) {
	mkdirSync(path.dirname(path.join(root, file)), { recursive: true });
	writeFileSync(path.join(root, file), content);
}

function commit(root, message, date) {
	git(root, "add", "-A");
	git(
		root,
		"-c",
		"user.name=Committer",
		"-c",
		"user.email=c@example.com",
		"commit",
		"-q",
		"--author=Alice <alice@example.com>",
		`--date=${date}`,
		"-m",
		message,
	);
}

function makeRepos() {
	const base = mkdtempSync(path.join(tmpdir(), "snip-e2e-"));
	const a = path.join(base, "a");
	mkdirSync(a);
	git(a, "init", "-q", "-b", "main");
	write(a, "README.md", "# monorepo\n");
	write(a, "packages/api/src/index.ts", "export const port = 3000;\n");
	write(a, "packages/web/src/app.tsx", "export const App = () => null;\n");
	write(a, "scripts/legacy.sh", "echo legacy\n");
	commit(a, "chore: initial monorepo", "2026-01-01T09:00:00+08:00");
	const baseSha = git(a, "rev-parse", "HEAD");

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
	commit(a, "feat(api): add health endpoint", "2026-01-02T10:00:00+08:00");
	git(a, "mv", "packages/web/src/app.tsx", "packages/web/src/main.tsx");
	write(a, "packages/web/src/main.tsx", "export const App = () => 'hi';\n");
	commit(a, "refactor(web): rename app to main", "2026-01-03T11:00:00+08:00");
	git(a, "rm", "-q", "scripts/legacy.sh");
	commit(a, "chore: remove legacy script", "2026-01-04T12:00:00+08:00");

	const b = path.join(base, "b");
	git(base, "clone", "-q", a, b);
	git(b, "reset", "-q", "--hard", baseSha);
	return { a, b, baseSha };
}

// ---- minimal WebDriver client ----

async function wd(method, url, body) {
	const init = { method, headers: { "content-type": "application/json" } };
	if (body !== undefined) init.body = JSON.stringify(body);
	const res = await fetch(DRIVER + url, init);
	const json = await res.json();
	if (!res.ok)
		throw new Error(`${method} ${url}: ${JSON.stringify(json.value)}`);
	return json.value;
}

const ELEMENT = "element-6066-11e4-a52e-4f735466cecf"; // W3C element key
let sid;
const s = (p) => `/session/${sid}${p}`;

async function until(what, fn, timeoutMs = 20000) {
	const end = Date.now() + timeoutMs;
	let last;
	while (Date.now() < end) {
		try {
			const v = await fn();
			if (v) return v;
		} catch (error) {
			last = error;
		}
		await new Promise((r) => setTimeout(r, 250));
	}
	throw new Error(
		`timed out waiting for ${what}${last ? `: ${last.message}` : ""}`,
	);
}

const js = (script, ...args) =>
	wd("POST", s("/execute/sync"), { script, args });
const find = (css) =>
	until(
		css,
		async () =>
			(
				await wd("POST", s("/element"), {
					using: "css selector",
					value: css,
				})
			)[ELEMENT],
	);
const click = async (css) =>
	wd("POST", s(`/element/${await find(css)}/click`), {});

async function shot(name) {
	const png = await wd("GET", s("/screenshot"));
	writeFileSync(path.join(OUT, `${name}.png`), Buffer.from(png, "base64"));
}

async function setRepo(dir) {
	const el = await find('[data-testid="repo-path"]');
	// WebKitWebDriver under Xvfb types upper-case letters as lower-case, so
	// set the value the way React sees typing, then press a real Enter.
	await js(
		`const [input, value] = arguments;
		Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value").set.call(input, value);
		input.dispatchEvent(new Event("input", { bubbles: true }));`,
		{ [ELEMENT]: el },
		dir,
	);
	await wd("POST", s(`/element/${el}/value`), { text: "\uE007" });
	let seen;
	await until(`repo field to read ${dir}`, async () => {
		seen = await js(
			"return document.querySelector('[data-testid=\"repo-path\"]').value",
		);
		return seen === dir;
	}).catch((error) => {
		throw new Error(
			`${error.message} (field holds ${JSON.stringify(seen)})`,
		);
	});
}

async function openTab(key) {
	await click(`[role="tab"][data-key="${key}"]`);
	await until(`tab ${key}`, () =>
		js(
			`return document.querySelector('[role="tab"][data-key="${key}"]').getAttribute('aria-selected') === 'true'`,
		),
	);
}

// ---- the scenario ----

async function main() {
	const { a, b, baseSha } = makeRepos();
	const [c3, , c1] = git(a, "rev-list", "-3", "HEAD").split("\n");
	const driver = spawn("tauri-driver", ["--port", String(PORT)], {
		stdio: "inherit",
	});
	try {
		await until(
			"tauri-driver",
			() => fetch(`${DRIVER}/status`).then((r) => r.ok),
			10000,
		);
		sid = (
			await wd("POST", "/session", {
				capabilities: {
					alwaysMatch: { "tauri:options": { application: APP } },
				},
			})
		).sessionId;

		// The first commands can land before the WebView has loaded the app;
		// wait until React has rendered the header.
		await until(
			"app to render",
			async () => {
				const url = await wd("GET", s("/url"));
				return (
					url.startsWith("tauri://") &&
					(await js(
						"return !!document.querySelector('[data-testid=\"repo-path\"]')",
					))
				);
			},
			30000,
		);

		// A: pick the three commits on the timeline and copy them.
		await setRepo(a);
		await openTab("commits");
		// Re-press until the rows show: a press that lands while the panel is
		// still mounting is dropped.
		await until("timeline rows", async () => {
			if (
				await js(
					"return document.querySelectorAll('[data-commit]').length > 0",
				)
			)
				return true;
			await click('[data-testid="load-history"]');
			return false;
		});
		await click(`[data-commit="${c3}"]`);
		// Shift+click via a real bubbling event; the timeline reads e.shiftKey.
		await js(
			"arguments[0].dispatchEvent(new MouseEvent('click', { bubbles: true, shiftKey: true }))",
			{ [ELEMENT]: await find(`[data-commit="${c1}"]`) },
		);
		await until(
			"3 commits selected",
			async () =>
				(await js(
					"return document.querySelectorAll('[data-commit][data-selected]').length",
				)) === 3,
		);
		await shot("1-a-selected");
		await click('[data-testid="copy-commits"]');
		await until(
			"copy toast",
			() =>
				js(
					'return !!document.querySelector(\'[role="alert"], [data-slot="toast"], .toast\')',
				),
			10000,
		).catch(() => undefined); // the toast is informative only; the paste below is the real check
		await shot("2-a-copied");

		// B: preview the clipboard and replay the commits.
		await setRepo(b);
		await openTab("paste");
		await click('[data-testid="preview-clipboard"]');
		await find('[data-testid="replay-commits"]');
		await shot("3-b-preview");
		await click('[data-testid="replay-commits"]');
		await until(
			"B to gain 3 commits",
			() => git(b, "rev-list", "--count", `${baseSha}..HEAD`) === "3",
		);
		await shot("4-b-replayed");
	} catch (error) {
		if (sid) await shot("failure").catch(() => undefined);
		if (sid)
			await wd("GET", s("/source"))
				.then((html) =>
					writeFileSync(path.join(OUT, "failure.html"), html),
				)
				.catch(() => undefined);
		throw error;
	} finally {
		if (sid) await wd("DELETE", `/session/${sid}`).catch(() => undefined);
		driver.kill();
	}

	// Same messages, authors and author dates; hashes are allowed to differ.
	const log = (repo) => git(repo, "log", "-3", "--format=%s|%an <%ae>|%aI");
	if (log(a) !== log(b))
		throw new Error(`log differs:\nA:\n${log(a)}\nB:\n${log(b)}`);
	// Same tree except the binary, which commit mode reports as not copied.
	const tree = (repo) =>
		git(repo, "ls-tree", "-r", "HEAD")
			.split("\n")
			.filter((l) => !l.endsWith("logo.png"))
			.join("\n");
	if (tree(a) !== tree(b))
		throw new Error(`tree differs:\nA:\n${tree(a)}\nB:\n${tree(b)}`);
	if (git(b, "ls-tree", "-r", "--name-only", "HEAD").includes("logo.png"))
		throw new Error(
			"binary logo.png should have been reported as not copied, not written",
		);
	console.log(
		`e2e ok: ${c1.slice(0, 8)}..${c3.slice(0, 8)} replayed into B; screenshots in ${OUT}`,
	);
}

main().catch((error) => {
	console.error(error);
	process.exit(1);
});
