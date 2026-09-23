/// <reference types="node" />
import assert from "node:assert/strict";
import { test } from "node:test";
import type { CopyOutcome } from "../generated/CopyOutcome";
import { commitCopyNote, copyMessage, copyNote } from "./copy-message.ts";
import { translator } from "./test-i18n.ts";

const outcome = (over: Partial<CopyOutcome> = {}): CopyOutcome => ({
	copiedFileCount: 3,
	skippedFileSizeCount: 0,
	skippedUnreadableCount: 0,
	fileLimitReached: false,
	fileCountLimit: 500,
	chars: 1234,
	lines: 10,
	words: 20,
	tokens: 300,
	...over,
});

test("file copy message matches TS copySelectedFiles", async () => {
	const t = await translator();
	assert.equal(copyMessage(t, "files", outcome()), "3 file(s) copied.");
	assert.equal(
		copyMessage(
			t,
			"files",
			outcome({ skippedFileSizeCount: 2, fileLimitReached: true, skippedUnreadableCount: 1 }),
		),
		"3 file(s) copied (2 skipped: size exceeded). File limit 500 reached. 1 skipped: not UTF-8 text or unreadable.",
	);
});

test("git copy message matches TS copyGitChanges", async () => {
	const t = await translator();
	assert.equal(copyMessage(t, "git", outcome()), "3 Git file(s) copied.");
	assert.equal(
		copyMessage(t, "git", outcome({ skippedFileSizeCount: 2, skippedUnreadableCount: 1 })),
		"3 Git file(s) copied (2 skipped: size exceeded, 1 skipped: not UTF-8 text or unreadable).",
	);
	assert.equal(
		copyMessage(t, "git", outcome({ skippedUnreadableCount: 4, fileLimitReached: true })),
		"3 Git file(s) copied (4 skipped: not UTF-8 text or unreadable). File limit 500 reached.",
	);
});

test("stats line and token thresholds match TS notifyCopied", async () => {
	const t = await translator();
	assert.deepEqual(copyNote(t, "files", outcome()), {
		severity: "success",
		text: "3 file(s) copied. 1,234 chars · 10 lines · 20 words · ~300 tokens.",
	});
	const warn = copyNote(t, "files", outcome({ tokens: 1_000_000 }));
	assert.equal(warn.severity, "warning");
	assert.match(warn.text, /~1,000,000 tokens\. Over 1,000,000 tokens\.$/);
	const danger = copyNote(t, "files", outcome({ tokens: 2_500_000 }));
	assert.equal(danger.severity, "danger");
	assert.match(danger.text, / Over 2,000,000 tokens\.$/);
});

test("commit copy note reports not-copied files", async () => {
	const t = await translator();
	assert.deepEqual(
		commitCopyNote(t, { commitCount: 3, fileCount: 7, chars: 5000, notCopiedCount: 0 }),
		{ severity: "success", text: "3 commit(s) copied: 7 file(s), 5,000 chars." },
	);
	assert.deepEqual(
		commitCopyNote(t, { commitCount: 3, fileCount: 7, chars: 5000, notCopiedCount: 1 }),
		{
			severity: "warning",
			text: "3 commit(s) copied: 7 file(s), 5,000 chars. 1 file(s) not copied.",
		},
	);
});

test("Traditional Chinese renders the same numbers", async () => {
	const t = await translator("zh-Hant");
	assert.equal(
		copyNote(t, "files", outcome()).text,
		"已複製 3 個檔案。1,234 字元 · 10 行 · 20 字 · ~300 tokens。",
	);
});
