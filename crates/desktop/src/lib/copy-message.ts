// Copy notifications, ported from the VS Code extension's notify.ts and the
// message builders in extension.ts (copySelectedFiles / copyGitChanges).
import type { CommitCopySummary } from "../generated/CommitCopySummary";
import type { CopyOutcome } from "../generated/CopyOutcome";
import type { Translate } from "./i18n.ts";

export const TOKEN_WARN_THRESHOLD = 1_000_000;
export const TOKEN_DANGER_THRESHOLD = 2_000_000;

export type Severity = "success" | "warning" | "danger";

export interface CopyNote {
	severity: Severity;
	text: string;
}

// Pinned to en-US like TS: the IntelliJ mirror must not disagree on `1,234`.
export const grouped = (n: number) => n.toLocaleString("en-US");

/** Which copy produced the outcome: file mode words it differently for git. */
export type CopyKind = "files" | "git";

/** TS copySelectedFiles / copyGitChanges message, before the stats line. */
export function copyMessage(t: Translate, kind: CopyKind, r: CopyOutcome): string {
	const limit = r.fileLimitReached ? t("fileLimitReached", { limit: r.fileCountLimit }) : "";
	if (kind === "files") {
		return t("copiedFiles", {
			count: r.copiedFileCount,
			sizeSuffix:
				r.skippedFileSizeCount > 0 ? t("sizeSkippedParen", { count: r.skippedFileSizeCount }) : "",
			limit,
			unreadable:
				r.skippedUnreadableCount > 0
					? t("unreadableSkippedSentence", { count: r.skippedUnreadableCount })
					: "",
		});
	}
	const reasons = [
		...(r.skippedFileSizeCount > 0 ? [t("sizeSkipped", { count: r.skippedFileSizeCount })] : []),
		...(r.skippedUnreadableCount > 0
			? [t("unreadableSkipped", { count: r.skippedUnreadableCount })]
			: []),
	];
	return t("copiedGitFiles", {
		count: r.copiedFileCount,
		skipped: reasons.length > 0 ? ` (${reasons.join(", ")})` : "",
		limit,
	});
}

/** TS notifyCopied: stats line plus the token-size severity. */
export function copyNote(t: Translate, kind: CopyKind, r: CopyOutcome): CopyNote {
	const note = t("copyStats", {
		message: copyMessage(t, kind, r),
		chars: grouped(r.chars),
		lines: grouped(r.lines),
		words: grouped(r.words),
		tokens: grouped(r.tokens),
	});
	if (r.tokens >= TOKEN_DANGER_THRESHOLD) {
		return {
			severity: "danger",
			text: t("overTokens", { note, threshold: grouped(TOKEN_DANGER_THRESHOLD) }),
		};
	}
	if (r.tokens >= TOKEN_WARN_THRESHOLD) {
		return {
			severity: "warning",
			text: t("overTokens", { note, threshold: grouped(TOKEN_WARN_THRESHOLD) }),
		};
	}
	return { severity: "success", text: note };
}

/** Commit-mode notification (spec 4.2): commits, files, chars, not copied. */
export function commitCopyNote(t: Translate, s: CommitCopySummary): CopyNote {
	const text =
		t("copiedCommits", { commits: s.commitCount, files: s.fileCount, chars: grouped(s.chars) }) +
		(s.notCopiedCount > 0 ? t("notCopiedSuffix", { count: s.notCopiedCount }) : "");
	return { severity: s.notCopiedCount > 0 ? "warning" : "success", text };
}
