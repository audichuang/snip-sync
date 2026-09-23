// File-mode paste preview: plan -> checkable tree, checked keys -> selection,
// and the TS extension.ts summary / result texts.
import type { RestoreBase } from "../generated/RestoreBase";
import type { RestoreExecutionResult } from "../generated/RestoreExecutionResult";
import type { RestorePlan } from "../generated/RestorePlan";
import type { RestoreSelection } from "../generated/RestoreSelection";
import type { SkipReason } from "../generated/SkipReason";
import type { Translate } from "./i18n.ts";

export type OpKind = "new" | "overwrite" | "delete" | "skip";

/** One plan row. `key` is `c:<i>` / `d:<i>` / `s:<i>` into the plan arrays. */
export interface PlanLeaf {
	key: string;
	path: string;
	kind: OpKind;
	/** Index into create/delete/skipped operations. */
	index: number;
	reason?: SkipReason;
	/** Unresolved raw clipboard path: shown as-is, not split into folders. */
	raw?: boolean;
}

export interface PlanNode {
	key: string;
	/** Last path segment (folder or file name). */
	name: string;
	leaf?: PlanLeaf;
	children?: PlanNode[];
	/** Skipped rows cannot be checked. */
	disableCheckbox?: boolean;
	isLeaf?: boolean;
}

export function planLeaves(plan: RestorePlan): PlanLeaf[] {
	return [
		...plan.createOperations.map((op, index) => ({
			key: `c:${index}`,
			path: op.relativePath,
			kind: (op.existed ? "overwrite" : "new") as OpKind,
			index,
		})),
		...plan.deleteOperations.map((op, index) => ({
			key: `d:${index}`,
			path: op.relativePath,
			kind: "delete" as OpKind,
			index,
		})),
		...plan.skippedOperations.map((op, index) => ({
			key: `s:${index}`,
			path: op.relativePath ?? op.rawPath,
			kind: "skip" as OpKind,
			index,
			reason: op.reason,
			raw: op.relativePath === null,
		})),
	];
}

/** Groups leaves into folders by `/`-separated path. */
export function buildPlanTree(plan: RestorePlan): PlanNode[] {
	const root: PlanNode = { key: "", name: "", children: [] };
	for (const leaf of planLeaves(plan)) {
		const parts = leaf.raw
			? [leaf.path]
			: leaf.path.replaceAll("\\", "/").split("/").filter(Boolean);
		let node = root;
		for (const part of parts.slice(0, -1)) {
			const key = `dir:${node.key}/${part}`;
			let child = node.children!.find((c) => c.key === key);
			if (!child) {
				child = { key, name: part, children: [] };
				node.children!.push(child);
			}
			node = child;
		}
		node.children!.push({
			key: leaf.key,
			name: parts.at(-1) ?? leaf.path,
			leaf,
			isLeaf: true,
			disableCheckbox: leaf.kind === "skip",
		});
	}
	markSkipOnlyFolders(root);
	return root.children!;
}

/** A folder holding only skipped rows has nothing to check either. */
function markSkipOnlyFolders(node: PlanNode): boolean {
	if (node.leaf) return node.leaf.kind === "skip";
	const allSkipped = node.children!.map(markSkipOnlyFolders).every(Boolean);
	if (allSkipped) node.disableCheckbox = true;
	return allSkipped;
}

/** Every actionable row starts checked. */
export function initialCheckedKeys(plan: RestorePlan): string[] {
	return planLeaves(plan)
		.filter((l) => l.kind !== "skip")
		.map((l) => l.key);
}

/**
 * TS executeRestorePlan options plus the unchecked rows. Overwrite is the
 * default (spec 3.2 "一律覆蓋"); `skipExisting` is TS's "Skip Existing".
 */
export function toSelection(
	plan: RestorePlan,
	checked: ReadonlySet<string>,
	skipExisting: boolean,
): RestoreSelection {
	const unchecked = (prefix: string, length: number) =>
		Array.from({ length }, (_, i) => i).filter(
			(i) => !checked.has(`${prefix}:${i}`),
		);
	return {
		overwriteExisting: !skipExisting,
		skipExisting,
		uncheckedCreates: unchecked("c", plan.createOperations.length),
		uncheckedDeletes: unchecked("d", plan.deleteOperations.length),
	};
}

/** TS: nothing to create or delete means no preview at all. */
export function hasActionable(plan: RestorePlan): boolean {
	return plan.createOperations.length > 0 || plan.deleteOperations.length > 0;
}

export function existingCount(plan: RestorePlan): number {
	return plan.createOperations.filter((op) => op.existed).length;
}

/** TS confirmationSummary. */
export function confirmationSummary(t: Translate, plan: RestorePlan): string {
	return t("confirmSummary", {
		create: plan.createOperations.filter((op) => !op.existed).length,
		overwrite: existingCount(plan),
		deleted: plan.deleteOperations.length,
		skipped: plan.skippedOperations.length,
	});
}

/** TS pasteAndRestoreFiles result toast, then the error line if any. */
export function resultSummary(
	t: Translate,
	result: RestoreExecutionResult,
): { text: string; errors: string | null } {
	const parts = [
		result.createdCount > 0
			? t("resultCreated", { count: result.createdCount })
			: "",
		result.overwrittenCount > 0
			? t("resultOverwritten", { count: result.overwrittenCount })
			: "",
		result.skippedExistingCount > 0
			? t("resultSkipped", { count: result.skippedExistingCount })
			: "",
		result.deletedCount > 0
			? t("resultDeleted", { count: result.deletedCount })
			: "",
	].filter(Boolean);
	return {
		text: parts.length > 0 ? parts.join(", ") : t("resultNoChange"),
		errors:
			result.errors.length > 0
				? t("resultErrors", {
						count: result.errors.length,
						errors: result.errors.slice(0, 3).join("; "),
					})
				: null,
	};
}

/** TS restoreBase.ts applyRestoreBase. */
export function applyRestoreBase(
	base: RestoreBase,
	relativePath: string,
): string {
	if (base.kind === "add") return `${base.prefix}/${relativePath}`;
	const slash = relativePath.indexOf("/");
	return slash >= 0 && relativePath.slice(0, slash) === base.segment
		? relativePath.slice(slash + 1)
		: relativePath;
}

/** TS isRelativeEntryPath. */
export function isRelativeEntryPath(p: string): boolean {
	return (
		!!p &&
		!p.startsWith("/") &&
		!/^[A-Za-z]:[\\/]/.test(p) &&
		!p.startsWith("\\")
	);
}

/** TS confirmRestoreBaseOffset prompt, with its before -> after example. */
export function suggestionText(
	t: Translate,
	plan: RestorePlan,
	suggestion: { base: RestoreBase; total: number },
): string {
	const base = suggestion.base;
	const label =
		base.kind === "strip"
			? t("baseStrip", { segment: base.segment })
			: t("baseAdd", { prefix: base.prefix });
	const sample = planLeaves(plan)
		.map((l) =>
			l.kind === "skip"
				? plan.skippedOperations[l.index].rawPath
				: l.path,
		)
		.find((p) => isRelativeEntryPath(p) && p.includes("/"));
	const example = sample
		? `\n\n${t("suggestExample", { from: sample, to: applyRestoreBase(base, sample) })}`
		: "";
	return t("suggestBase", { label, total: suggestion.total }) + example;
}
