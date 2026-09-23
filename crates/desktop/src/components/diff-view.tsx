import { Button, toast } from "@heroui/react";
import { PatchDiff } from "@pierre/diffs/react";
import { invoke } from "@tauri-apps/api/core";
import { useState } from "react";
import { useTranslation } from "react-i18next";
import type { DiffTarget } from "../generated/DiffTarget";
import { errorText } from "../lib/i18n";
import { withGitHeader } from "../lib/patch";

const sameTarget = (a: DiffTarget, b: DiffTarget) =>
	JSON.stringify(a) === JSON.stringify(b);

/** One open diff at a time, fetched from the pending plan on demand. */
export function useDiff() {
	const { t } = useTranslation();
	const [open, setOpen] = useState<{
		target: DiffTarget;
		patch: string;
	} | null>(null);

	async function toggle(target: DiffTarget) {
		if (open && sameTarget(open.target, target)) {
			setOpen(null);
			return;
		}
		try {
			setOpen({
				target,
				patch: await invoke<string>("diff", { target }),
			});
		} catch (error: unknown) {
			toast.danger(errorText(t, error));
		}
	}

	const isOpen = (target: DiffTarget) =>
		open !== null && sameTarget(open.target, target);
	return { patch: open?.patch ?? null, toggle, isOpen };
}

export function DiffButton({
	diff,
	target,
}: {
	diff: ReturnType<typeof useDiff>;
	target: DiffTarget;
}) {
	const { t } = useTranslation();
	return (
		<Button
			size="sm"
			variant="ghost"
			onPress={() => void diff.toggle(target)}
		>
			{diff.isOpen(target) ? t("hideDiff") : t("showDiff")}
		</Button>
	);
}

/** Current file (a/) against the clipboard content (b/). */
export function DiffBody({ patch }: { patch: string }) {
	const { t } = useTranslation();
	if (!patch.includes("@@"))
		return <p className="text-sm text-muted">{t("noDiff")}</p>;
	return (
		<div className="overflow-hidden rounded border border-border">
			<PatchDiff
				patch={withGitHeader(patch)}
				disableWorkerPool
				options={{
					diffStyle: "unified",
					overflow: "wrap",
					disableFileHeader: true,
				}}
			/>
		</div>
	);
}
