/// <reference types="node" />
import assert from "node:assert/strict";
import { test } from "node:test";
import { withGitHeader } from "./patch.ts";

test("adds a diff --git line to a plain unified diff", () => {
	const patch = "--- a/src/old.ts\n+++ b/src/new.ts\n@@ -1 +1 @@\n-a\n+b\n";
	assert.equal(
		withGitHeader(patch),
		`diff --git a/src/old.ts b/src/new.ts\n${patch}`,
	);
	assert.equal(withGitHeader(withGitHeader(patch)), withGitHeader(patch));
	assert.equal(withGitHeader(""), "");
});
