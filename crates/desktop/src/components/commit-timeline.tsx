import { Button, Spinner, toast } from "@heroui/react";
import { GitLog, type GitLogEntry } from "@tomplum/react-git-log";
import { invoke } from "@tauri-apps/api/core";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import type { CommitSelection } from "../generated/CommitSelection";
import type { CommitSummary } from "../generated/CommitSummary";
import {
	chainBetween,
	clickCommit,
	toCommitSelection,
	type RangeEnds,
} from "../lib/commit-range";
import { errorText } from "../lib/i18n";

const HISTORY_LIMIT = 300;
// The graph needs a branch name; the log is HEAD's history.
const BRANCH = "HEAD";

/** Commit mode (spec 4.1): pick a contiguous range on the timeline. */
export function CommitTimeline({
	repo,
	onCopy,
}: {
	repo: string;
	onCopy: (selection: CommitSelection) => void;
}) {
	const { t } = useTranslation();
	const [commits, setCommits] = useState<CommitSummary[] | null>(null);
	const [loading, setLoading] = useState(false);
	const [ends, setEnds] = useState<RangeEnds | null>(null);
	// GitLog's onSelectCommit carries no event; remember the click's Shift.
	const shiftRef = useRef(false);

	const handleLoad = useCallback(async () => {
		if (!repo) return;
		setLoading(true);
		try {
			setCommits(
				await invoke<CommitSummary[]>("list_commits", {
					repo,
					limit: HISTORY_LIMIT,
				}),
			);
			setEnds(null);
		} catch (error: unknown) {
			toast.danger(errorText(t, error));
		} finally {
			setLoading(false);
		}
	}, [repo, t]);

	useEffect(() => {
		const timer = setTimeout(() => void handleLoad(), 0);
		return () => clearTimeout(timer);
	}, [handleLoad]);

	const entries = useMemo<GitLogEntry[]>(
		() =>
			(commits ?? []).map((c) => ({
				hash: c.sha,
				branch: BRANCH,
				parents: c.parents,
				message: c.subject,
				committerDate: c.authorDate,
				authorDate: c.authorDate,
				author: { name: c.authorName, email: c.authorEmail },
			})),
		[commits],
	);

	const highlighted = useMemo(() => {
		if (!commits || !ends) return new Set<string>();
		const chain = chainBetween(commits, ends);
		return new Set(chain.length > 0 ? chain : [ends.anchor, ends.end]);
	}, [commits, ends]);

	function handleCopy() {
		if (!commits || !ends) return;
		const request = toCommitSelection(commits, ends);
		if (request.ok) {
			onCopy(request.selection);
		} else if (request.reason === "discontinuous") {
			toast.danger(
				t("discontinuous", {
					tip: request.tip.slice(0, 8),
					oldest: request.oldest.slice(0, 8),
				}),
			);
		} else {
			toast.danger(t("rootRangeUnsupported"));
		}
	}

	return (
		<div className="flex min-h-0 flex-1 flex-col gap-3">
			<div className="flex flex-wrap items-center gap-2">
				<Button
					data-testid="load-history"
					size="sm"
					variant="secondary"
					onPress={() => void handleLoad()}
				>
					{loading ? <Spinner size="sm" /> : t("loadHistory")}
				</Button>
				<span className="text-sm text-muted">
					{ends
						? t("selectedCommits", { count: highlighted.size })
						: t("timelineHint")}
				</span>
				<div className="ml-auto flex gap-2">
					<Button
						size="sm"
						variant="ghost"
						isDisabled={!ends}
						onPress={() => setEnds(null)}
					>
						{t("clearSelection")}
					</Button>
					<Button
						data-testid="copy-commits"
						size="sm"
						isDisabled={!ends}
						onPress={handleCopy}
					>
						{t("copyCommits")}
					</Button>
				</div>
			</div>

			{commits !== null && commits.length === 0 && (
				<p className="text-sm text-muted">{t("noHistory")}</p>
			)}
			{commits !== null && commits.length > 0 && (
				<div
					className="min-h-0 flex-1 overflow-auto rounded border border-border select-none"
					onClickCapture={(e) => {
						shiftRef.current = e.shiftKey;
					}}
				>
					<GitLog
						entries={entries}
						currentBranch={BRANCH}
						showGitIndex={false}
						enableSelectedCommitStyling={false}
						defaultGraphWidth={120}
						onSelectCommit={(commit) => {
							// Re-clicking the library's selected row reports undefined.
							if (commit)
								setEnds((prev) =>
									clickCommit(
										prev,
										commit.hash,
										shiftRef.current,
									),
								);
						}}
					>
						<GitLog.GraphHTMLGrid nodeSize={12} />
						<GitLog.Table
							// oxlint-disable-next-line react/no-unstable-nested-components -- react-git-log calls `row` as a plain function, never mounts it
							row={({ commit, backgroundColour }) => (
								<div
									data-commit={commit.hash}
									data-selected={
										highlighted.has(commit.hash) ||
										undefined
									}
									className="flex h-10 cursor-pointer items-center gap-3 px-2 text-sm data-selected:bg-accent-soft"
									style={
										highlighted.has(commit.hash)
											? undefined
											: {
													backgroundColor:
														backgroundColour,
												}
									}
								>
									<span className="font-mono text-xs text-muted">
										{commit.hash.slice(0, 8)}
									</span>
									<span className="min-w-0 flex-1 truncate">
										{commit.message}
									</span>
									<span className="shrink-0 text-xs text-muted">
										{commit.author?.name}
									</span>
									<span className="shrink-0 text-xs text-muted">
										{(commit.authorDate ?? "")
											.slice(0, 16)
											.replace("T", " ")}
									</span>
								</div>
							)}
						/>
					</GitLog>
				</div>
			)}
		</div>
	);
}
