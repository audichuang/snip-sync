/// <reference types="node" />
import assert from "node:assert/strict";
import { test } from "node:test";
import { errorText, pickLanguage } from "./i18n.ts";
import en from "./locales/en.ts";
import zhHant from "./locales/zh-Hant.ts";
import { translator } from "./test-i18n.ts";

test("both languages carry the same keys and placeholders", () => {
	assert.deepEqual(Object.keys(zhHant).sort(), Object.keys(en).sort());
	const holes = (s: string) => (s.match(/{{\w+}}/g) ?? []).sort();
	for (const key of Object.keys(en) as (keyof typeof en)[]) {
		assert.deepEqual(holes(zhHant[key]), holes(en[key]), key);
	}
});

test("any Chinese locale picks Traditional Chinese unless one was stored", () => {
	assert.equal(pickLanguage(null, "zh-CN"), "zh-Hant");
	assert.equal(pickLanguage(null, "zh-TW"), "zh-Hant");
	assert.equal(pickLanguage(null, "en-US"), "en");
	assert.equal(pickLanguage("en", "zh-TW"), "en");
	assert.equal(pickLanguage("fr", "de"), "en");
});

test("known backend errors are translated, others shown verbatim", async () => {
	const t = await translator("zh-Hant");
	assert.equal(errorText(t, "No files selected."), "沒有選取任何檔案。");
	assert.equal(errorText(t, "git exploded"), "git exploded");
});
