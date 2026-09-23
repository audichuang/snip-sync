/// <reference types="node" />
import assert from "node:assert/strict";
import { test } from "node:test";
import type { RestorePlan } from "../generated/RestorePlan";
import {
	applyRestoreBase,
	buildPlanTree,
	confirmationSummary,
	hasActionable,
	initialCheckedKeys,
	resultSummary,
	suggestionText,
	toSelection,
} from "./restore-plan.ts";
import { translator } from "./test-i18n.ts";

const create = (relativePath: string, existed: boolean) => ({
	relativePath,
	absolutePath: `/w/${relativePath}`,
	content: "x",
	existed,
	rootPath: "/w",
});

const plan: RestorePlan = {
	roots: ["/w"],
	createOperations: [create("src/a.ts", false), create("src/b.ts", true), create("README.md", true)],
	deleteOperations: [{ relativePath: "src/old.ts", absolutePath: "/w/src/old.ts" }],
	skippedOperations: [
		{ rawPath: "../evil", relativePath: null, reason: "UNRESOLVED_PATH" },
		{ rawPath: "bin/x", relativePath: "bin/x", reason: "NON_UTF8_TARGET" },
	],
};

test("tree groups by folder and disables skipped rows", () => {
	const tree = buildPlanTree(plan);
	const src = tree.find((n) => n.name === "src")!;
	assert.deepEqual(
		src.children!.map((n) => [n.key, n.leaf?.kind]),
		[
			["c:0", "new"],
			["c:1", "overwrite"],
			["d:0", "delete"],
		],
	);
	const evil = tree.find((n) => n.key === "s:0")!;
	assert.equal(evil.name, "../evil");
	assert.equal(evil.disableCheckbox, true);
	assert.equal(evil.leaf?.reason, "UNRESOLVED_PATH");
	// A folder with only skipped rows cannot be checked either.
	assert.equal(tree.find((n) => n.name === "bin")!.disableCheckbox, true);
	assert.equal(src.disableCheckbox, undefined);
});

test("unchecked rows become unchecked indices; overwrite by default", () => {
	assert.deepEqual(initialCheckedKeys(plan), ["c:0", "c:1", "c:2", "d:0"]);
	const checked = new Set(["c:0", "c:2", "dir:/src"]);
	assert.deepEqual(toSelection(plan, checked, false), {
		overwriteExisting: true,
		skipExisting: false,
		uncheckedCreates: [1],
		uncheckedDeletes: [0],
	});
	assert.equal(toSelection(plan, checked, true).skipExisting, true);
	assert.equal(toSelection(plan, checked, true).overwriteExisting, false);
});

test("empty plan has nothing actionable (TS 'No actionable files found')", async () => {
	const t = await translator();
	const empty: RestorePlan = { ...plan, createOperations: [], deleteOperations: [] };
	assert.equal(hasActionable(empty), false);
	assert.equal(hasActionable(plan), true);
	assert.equal(
		t("noActionable", { count: empty.skippedOperations.length }),
		"No actionable files found. Skipped 2.",
	);
});

test("confirmation and result texts match TS", async () => {
	const t = await translator();
	assert.equal(
		confirmationSummary(t, plan),
		"Snipcode will create 1, overwrite 2, delete 1, and skip 2 operation(s).",
	);
	assert.deepEqual(
		resultSummary(t, {
			createdCount: 1,
			overwrittenCount: 2,
			skippedExistingCount: 0,
			deletedCount: 1,
			errors: [],
		}),
		{ text: "Created 1, Overwritten 2, Deleted 1", errors: null },
	);
	assert.deepEqual(
		resultSummary(t, {
			createdCount: 0,
			overwrittenCount: 0,
			skippedExistingCount: 0,
			deletedCount: 0,
			errors: ["a: x", "b: y", "c: z", "d: w"],
		}),
		{ text: "No files changed.", errors: "Snipcode failed 4 operation(s): a: x; b: y; c: z" },
	);
});

test("applyRestoreBase adds and strips a leading segment (TS restoreBase.test)", () => {
	assert.equal(applyRestoreBase({ kind: "add", prefix: "repo" }, "src/a.ts"), "repo/src/a.ts");
	assert.equal(applyRestoreBase({ kind: "strip", segment: "repo" }, "repo/src/a.ts"), "src/a.ts");
	assert.equal(applyRestoreBase({ kind: "strip", segment: "repo" }, "src/a.ts"), "src/a.ts");
});

test("suggestion prompt carries the TS example", async () => {
	const t = await translator();
	assert.equal(
		suggestionText(t, plan, { base: { kind: "strip", segment: "src" }, total: 3 }),
		'These paths look like they belong elsewhere in this workspace. I can remove the leading "src/" for all 3 file(s).\n\nExample: src/a.ts → a.ts',
	);
});
