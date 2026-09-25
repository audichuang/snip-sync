import assert from "node:assert/strict";
import { test } from "node:test";
import { graphEntries } from "./history-graph.ts";

test("forks use separate lanes and a merge preserves both parents", () => {
	const result = graphEntries({
		root: "/repo",
		head: "merge",
		hasMore: false,
		refs: [
			{ name: "refs/heads/main", sha: "merge" },
			{ name: "refs/heads/feature", sha: "side" },
		],
		commits: [
			["merge", ["main", "side"]],
			["side", ["base"]],
			["main", ["base"]],
			["base", []],
		].map(([sha, parents]) => ({
			sha: sha as string,
			parents: parents as string[],
			authorName: "A",
			authorEmail: "a@x",
			authorDate: "2026-01-01",
			subject: sha as string,
		})),
	});
	assert.deepEqual(result[0]?.parents, ["main", "side"]);
	assert.equal(result[0]?.branch, result[2]?.branch);
	assert.notEqual(result[0]?.branch, result[1]?.branch);
});
