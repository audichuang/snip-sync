/// <reference types="node" />
import assert from "node:assert/strict";
import { test } from "node:test";
import { showCopyNotification } from "./settings.ts";

test("showCopyNotification defaults to true and honours only booleans", () => {
	assert.equal(showCopyNotification(undefined), true);
	assert.equal(showCopyNotification({}), true);
	assert.equal(showCopyNotification({ showCopyNotification: "false" }), true);
	assert.equal(showCopyNotification({ showCopyNotification: false }), false);
	assert.equal(showCopyNotification({ showCopyNotification: true }), true);
});
