// WebDriver helpers for the real-app scenarios (plain fetch, no npm deps).
// One app session drives every scenario; element lookups go through
// data-testid so the UI language does not matter.

import { execFileSync, spawn } from "node:child_process";
import {
	existsSync,
	mkdirSync,
	mkdtempSync,
	readFileSync,
	writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";

const PORT = 4444;
const DRIVER = `http://127.0.0.1:${PORT}`;
// W3C element key
const ELEMENT = "element-6066-11e4-a52e-4f735466cecf";
let sid;
let out;

// ---- git fixtures ----

export function git(cwd, ...args) {
	return execFileSync("git", args, { cwd, encoding: "utf8" }).replace(
		/\n$/u,
		"",
	);
}

export function write(root, file, content) {
	mkdirSync(path.dirname(path.join(root, file)), { recursive: true });
	writeFileSync(path.join(root, file), content);
}

export const read = (root, file) => readFileSync(path.join(root, file), "utf8");
/** Text with CRLF folded to LF: a Windows checkout (core.autocrlf) has CRLF. */
export const readLf = (root, file) => read(root, file).replaceAll("\r\n", "\n");
export const exists = (root, file) => existsSync(path.join(root, file));

let commits = 0;
/** Commits everything as Alice with a distinct, fixed author date. */
export function commit(root, message) {
	commits++;
	git(root, "add", "-A");
	const day = String((commits % 28) + 1).padStart(2, "0");
	const month = String((Math.floor(commits / 28) % 12) + 1).padStart(2, "0");
	git(
		root,
		"-c",
		"user.name=Alice",
		"-c",
		"user.email=alice@example.com",
		"commit",
		"-q",
		`--date=2026-${month}-${day}T10:00:00+08:00`,
		"-m",
		message,
	);
	return git(root, "rev-parse", "HEAD");
}

export function merge(root, branch, message) {
	git(
		root,
		"-c",
		"user.name=Alice",
		"-c",
		"user.email=alice@example.com",
		"merge",
		"-q",
		"--no-ff",
		"-m",
		message,
		branch,
	);
	return git(root, "rev-parse", "HEAD");
}

const scratch = () => mkdtempSync(path.join(tmpdir(), "snip-e2e-"));

export function newRepo(name = "repo") {
	const dir = path.join(scratch(), name);
	mkdirSync(dir, { recursive: true });
	git(dir, "init", "-q", "-b", "main");
	return dir;
}

export function newFolder(name = "folder") {
	const dir = path.join(scratch(), name);
	mkdirSync(dir, { recursive: true });
	return dir;
}

/** Clone of `src`, optionally reset to `at`. */
export function clone(src, at, name = "clone") {
	const dir = path.join(scratch(), name);
	execFileSync("git", ["clone", "-q", src, dir]);
	if (at) git(dir, "reset", "-q", "--hard", at);
	return dir;
}

// ---- WebDriver ----

async function wd(method, url, body) {
	const init = { method, headers: { "content-type": "application/json" } };
	if (body !== undefined) init.body = JSON.stringify(body);
	const res = await fetch(DRIVER + url, init);
	const json = await res.json();
	if (!res.ok)
		throw new Error(`${method} ${url}: ${JSON.stringify(json.value)}`);
	return json.value;
}

const s = (p) => `/session/${sid}${p}`;
export const js = (script, ...args) =>
	wd("POST", s("/execute/sync"), { script, args });
export const sleep = (ms) =>
	new Promise((r) => {
		setTimeout(r, ms);
	});

export async function until(what, fn, timeoutMs = 20000) {
	const end = Date.now() + timeoutMs;
	let last;
	while (Date.now() < end) {
		try {
			const v = await fn();
			if (v) return v;
		} catch (error) {
			last = error;
		}
		await sleep(200);
	}
	throw new Error(
		`timed out waiting for ${what}${last ? `: ${last.message}` : ""}`,
	);
}

const q = (id) => `[data-testid="${id}"]`;
export const present = (id) =>
	js(`return !!document.querySelector('${q(id)}')`);
export const find = (css) =>
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
export const click = async (css) =>
	wd("POST", s(`/element/${await find(css)}/click`), {});
export const clickId = (id) => click(q(id));
export const bodyText = () => js("return document.body.innerText");
export const attr = (id, name) =>
	js(
		`const e = document.querySelector('${q(id)}'); return e && e.getAttribute('${name}')`,
	);

export async function shot(name) {
	const png = await wd("GET", s("/screenshot"));
	writeFileSync(path.join(out, `${name}.png`), Buffer.from(png, "base64"));
}

export async function saveSource(name) {
	writeFileSync(
		path.join(out, `${name}.html`),
		await wd("GET", s("/source")),
	);
}

// WebKitWebDriver under Xvfb types upper-case letters as lower-case, so set
// values the way React sees typing; Enter is still a real key press.
export async function setInput(css, value, { enter = false } = {}) {
	const el = await find(css);
	await js(
		`const [input, value] = arguments;
		Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value").set.call(input, value);
		input.dispatchEvent(new Event("input", { bubbles: true }));`,
		{ [ELEMENT]: el },
		value,
	);
	if (enter) await wd("POST", s(`/element/${el}/value`), { text: "" });
	await until(
		`${css} to read ${value}`,
		async () =>
			(await js(
				"return document.querySelector(arguments[0]).value",
				css,
			)) === value,
	);
}

export async function setRepo(dir) {
	await setInput(q("repo-path"), dir, { enter: true });
}

export async function openTab(key) {
	await click(`[role="tab"][data-key="${key}"]`);
	await until(`tab ${key}`, () =>
		js(
			`return document.querySelector('[role="tab"][data-key="${key}"]').getAttribute('aria-selected') === 'true'`,
		),
	);
}

// Toasts: HeroUI renders them as role=alert / status regions.
const toastList = () =>
	js(
		'return [...document.querySelectorAll(\'[data-slot="toast"], [role="alert"], [role="status"]\')].map(e => e.innerText.trim()).filter(Boolean)',
	);
/** Toasts present now that were not in `before` (older ones may be leaving). */
async function newToasts(before) {
	const seen = new Set(before);
	return (await toastList()).filter((t) => !seen.has(t));
}
export const toastText = () =>
	js(
		'return [...document.querySelectorAll(\'[data-slot="toast"], [role="alert"], [role="status"]\')].map(e => e.innerText).join(\' | \')',
	);

/** Runs `action`, then waits for a toast that was not there before. */
export async function withToast(action) {
	const before = await toastList();
	await action();
	return until(
		"a new toast",
		async () => (await newToasts(before)).join(" | ") || false,
		10000,
	);
}

// ---- flows ----

/** Commit mode, copy side: select newest..oldest on the timeline and copy. */
export async function copyCommits(repo, newest, oldest) {
	await setRepo(repo);
	await openTab("commits");
	// A press that lands while the panel is still mounting is dropped.
	await until("timeline rows", async () => {
		if (
			await js(
				"return document.querySelectorAll('[data-commit]').length > 0",
			)
		)
			return true;
		await clickId("load-history");
		return false;
	});
	await click(`[data-commit="${newest}"]`);
	// Shift+click as a real bubbling event; the timeline reads e.shiftKey.
	await js(
		"arguments[0].dispatchEvent(new MouseEvent('click', { bubbles: true, shiftKey: true }))",
		{ [ELEMENT]: await find(`[data-commit="${oldest}"]`) },
	);
	return withToast(() => clickId("copy-commits"));
}

/** Paste side: preview the clipboard in `repo`; resolves to what showed up. */
export async function previewPaste(repo) {
	await setRepo(repo);
	await openTab("paste");
	const before = await toastList();
	await clickId("preview-clipboard");
	return until("a preview", async () => {
		if (await present("replay-commits")) return "commits";
		if (await present("apply-restore")) return "files";
		const fresh = await newToasts(before);
		return fresh.length > 0 ? `toast:${fresh.join(" | ")}` : false;
	});
}

/** Commit mode, paste side: preview, replay, wait for the result. */
export async function replayCommits(repo) {
	const kind = await previewPaste(repo);
	if (kind !== "commits")
		throw new Error(`expected a commit preview, got ${kind}`);
	await clickId("replay-commits");
	await find('[data-testid="replay-result"]');
	return attr("replay-result", "data-status");
}

/** File mode, paste side: preview, accept as-is, overwrite, apply. */
export async function restoreFiles(repo) {
	const kind = await previewPaste(repo);
	if (kind !== "files")
		throw new Error(`expected a file preview, got ${kind}`);
	if (await present("use-as-is")) await clickId("use-as-is");
	if (await present("overwrite-all")) await clickId("overwrite-all");
	await clickId("apply-restore");
	await find('[data-testid="restore-result"]');
	return attr("restore-result", "data-status");
}

/** File mode, copy side, from a git source: working | staged | commit | range. */
export async function copyGitSource(repo, source, { sha, base, tip } = {}) {
	await setRepo(repo);
	await openTab("files");
	await clickId(`source-${source}`);
	if (sha) await setInput(q("commit-sha"), sha);
	if (base) await setInput(q("range-base"), base);
	if (tip) await setInput(q("range-tip"), tip);
	return withToast(() => clickId("copy-files"));
}

// ---- session ----

export async function start(app, outDir) {
	out = outDir;
	const driver = spawn("tauri-driver", ["--port", String(PORT)], {
		stdio: "inherit",
	});
	await until(
		"tauri-driver",
		() => fetch(`${DRIVER}/status`).then((r) => r.ok),
		10000,
	);
	sid = (
		await wd("POST", "/session", {
			capabilities: {
				alwaysMatch: { "tauri:options": { application: app } },
			},
		})
	).sessionId;
	// tauri://localhost on Linux/macOS, http://tauri.localhost on Windows.
	await until(
		"app to render",
		async () =>
			/^(tauri:\/\/|https?:\/\/tauri\.localhost)/u.test(
				await wd("GET", s("/url")),
			) && (await present("repo-path")),
		30000,
	);
	return async () => {
		await wd("DELETE", `/session/${sid}`).catch(() => {});
		driver.kill();
	};
}
