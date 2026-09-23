/// <reference types="node" />
import assert from "node:assert/strict";
import { test } from "node:test";
import type { CommitSummary } from "../generated/CommitSummary";
import {
	chainBetween,
	clickCommit,
	toCommitSelection,
} from "./commit-range.ts";

const c = (sha: string, ...parents: string[]): CommitSummary => ({
	sha,
	parents,
	authorName: "A",
	authorEmail: "a@x",
	authorDate: "2026-01-01T00:00:00Z",
	subject: sha,
});

// Newest first, as `git log` lists them: e merges side branch s into d.
const log = [
	c("e", "d", "s"),
	c("s", "b"),
	c("d", "c"),
	c("c", "b"),
	c("b", "a"),
	c("a"),
];

test("click sets the anchor, shift-click extends from it", () => {
	const first = clickCommit(null, "d", false);
	assert.deepEqual(first, { anchor: "d", end: "d" });
	assert.deepEqual(clickCommit(first, "b", true), { anchor: "d", end: "b" });
	assert.deepEqual(clickCommit(first, "b", false), { anchor: "b", end: "b" });
	assert.deepEqual(clickCommit(null, "b", true), { anchor: "b", end: "b" });
});

test("chain follows first parents, in either click order", () => {
	assert.deepEqual(chainBetween(log, { anchor: "b", end: "e" }), [
		"e",
		"d",
		"c",
		"b",
	]);
	assert.deepEqual(chainBetween(log, { anchor: "e", end: "b" }), [
		"e",
		"d",
		"c",
		"b",
	]);
	// s is not on e's first-parent chain: not contiguous.
	assert.deepEqual(chainBetween(log, { anchor: "e", end: "s" }), []);
});

test("range selection uses the older end's first parent as base", () => {
	assert.deepEqual(toCommitSelection(log, { anchor: "c", end: "e" }), {
		ok: true,
		selection: { kind: "range", base: "b", tip: "e" },
	});
	// s is off e's first-parent chain; b..e would silently copy e, d, c.
	assert.deepEqual(toCommitSelection(log, { anchor: "e", end: "s" }), {
		ok: false,
		reason: "discontinuous",
		tip: "e",
		oldest: "s",
	});
});

test("a range down to the root commit is 'last n' only from HEAD", () => {
	assert.deepEqual(toCommitSelection(log, { anchor: "a", end: "e" }), {
		ok: true,
		selection: { kind: "last", n: 5 },
	});
	assert.deepEqual(toCommitSelection(log, { anchor: "a", end: "c" }), {
		ok: false,
		reason: "rootRangeUnsupported",
	});
});
