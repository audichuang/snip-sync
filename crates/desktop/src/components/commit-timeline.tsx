import { Button, Spinner, toast } from "@heroui/react";
import { GitLog } from "@tomplum/react-git-log";
import { invoke } from "@tauri-apps/api/core";
import { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import type { CommitSelection } from "../generated/CommitSelection";
import type { RepositoryHistory } from "../generated/RepositoryHistory";
import type { GitBrowse } from "../generated/GitBrowse";
import type { GitSourceDto } from "../generated/GitSourceDto";
import {
	chainBetween,
	clickCommit,
	toCommitSelection,
	type RangeEnds,
} from "../lib/commit-range";
import { graphEntries, shortRef } from "../lib/history-graph";
import { errorText } from "../lib/i18n";
import { ChangesTree, SourcePreviewPane } from "./source-browser";

export function CommitTimeline({
	repo,
	onCopy,
}: {
	repo: string;
	onCopy: (selection: CommitSelection) => void;
}) {
	const { t } = useTranslation();
	const [history, setHistory] = useState<RepositoryHistory | null>(null);
	const [loading, setLoading] = useState(false);
	const [error, setError] = useState("");
	const [reference, setReference] = useState("");
	const [query, setQuery] = useState("");
	const [skip, setSkip] = useState(0);
	const [reload, setReload] = useState(0);
	const [ends, setEnds] = useState<RangeEnds | null>(null);
	const [focused, setFocused] = useState<string | null>(null);
	const [details, setDetails] = useState<GitBrowse | null>(null);
	const [detailError, setDetailError] = useState("");
	const [preview, setPreview] = useState<string | null>(null);
	const shift = useRef(false);
	useEffect(() => {
		if (!repo) return;
		let cancelled = false;
		const timer = setTimeout(() => {
			setLoading(true);
			setError("");
			void invoke<RepositoryHistory>("browse_history", {
				repo,
				reference: reference || null,
				query,
				skip,
			})
				.then((data) => {
					if (!cancelled)
						setHistory((prev) => ({
							...data,
							commits:
								skip && prev
									? [
											...prev.commits,
											...data.commits.filter(
												(c) =>
													!prev.commits.some(
														(p) => p.sha === c.sha,
													),
											),
										]
									: data.commits,
						}));
					return null;
				})
				.catch((e: unknown) => {
					if (!cancelled) setError(errorText(t, e));
				})
				.finally(() => {
					if (!cancelled) setLoading(false);
				});
		}, 200);
		return () => {
			cancelled = true;
			clearTimeout(timer);
		};
		// oxlint-disable-next-line react/exhaustive-effect-dependencies -- explicit reload restarts history
	}, [repo, reference, query, skip, reload, t]);
	const commit = history?.commits.find((c) => c.sha === focused);
	const source: GitSourceDto | null = commit
		? commit.parents[0]
			? { kind: "range", base: commit.parents[0], tip: commit.sha }
			: { kind: "commit", sha: commit.sha }
		: null;
	const sourceKey = JSON.stringify(source);
	useEffect(() => {
		if (!focused) return;
		let cancelled = false;
		const timer = setTimeout(() => {
			setDetails(null);
			setDetailError("");
			setPreview(null);
			void invoke<GitBrowse>("browse_git", {
				repo: history?.root ?? repo,
				source: JSON.parse(sourceKey) as GitSourceDto,
			})
				.then((data) => {
					if (!cancelled) {
						setDetails(data);
						setPreview(data.changes[0]?.path ?? null);
					}
					return null;
				})
				.catch((e: unknown) => {
					if (!cancelled) setDetailError(String(e));
				});
		}, 0);
		return () => {
			cancelled = true;
			clearTimeout(timer);
		};
	}, [repo, history?.root, focused, sourceKey]);
	const entries = useMemo(
		() => (history ? graphEntries(history) : []),
		[history],
	);
	const highlighted = useMemo(() => {
		if (!history || !ends) return new Set<string>();
		const chain = chainBetween(history.commits, ends);
		return new Set(chain.length > 0 ? chain : [ends.anchor, ends.end]);
	}, [history, ends]);
	function reset() {
		setReload((n) => n + 1);
		setSkip(0);
		setEnds(null);
		setFocused(null);
		setPreview(null);
		setDetails(null);
		setHistory(null);
	}
	function copy() {
		if (!history || !ends) return;
		const result = toCommitSelection(history.commits, ends);
		if (result.ok) onCopy(result.selection);
		else if (result.reason === "discontinuous")
			toast.danger(
				t("discontinuous", {
					tip: result.tip.slice(0, 8),
					oldest: result.oldest.slice(0, 8),
				}),
			);
	}
	const refs = history?.refs ?? [];
	return (
		<div className="flex min-h-0 flex-1 gap-3">
			<aside className="w-48 shrink-0 overflow-auto rounded border border-border bg-default/30 p-2">
				<h2 className="mb-2 px-2 text-sm font-semibold">
					{t("branchesAndTags")}
				</h2>
				<button
					type="button"
					data-testid="all-refs"
					aria-pressed={!reference}
					className="w-full rounded px-2 py-1.5 text-left text-sm aria-pressed:bg-accent-soft"
					onClick={() => {
						reset();
						setReference("");
					}}
				>
					{t("allBranches")}
				</button>
				{(
					[
						["refs/heads/", "localBranches"],
						["refs/remotes/", "remoteBranches"],
						["refs/tags/", "tags"],
					] as const
				).map(([prefix, label]) => (
					<div key={prefix} className="mt-4">
						<h3 className="px-2 text-xs text-muted">{t(label)}</h3>
						{refs
							.filter((r) => r.name.startsWith(prefix))
							.map((r) => (
								<button
									key={r.name}
									type="button"
									data-testid={`ref-${r.name}`}
									title={r.name}
									aria-pressed={reference === r.name}
									className="block w-full truncate rounded px-2 py-1.5 text-left font-mono text-xs aria-pressed:bg-accent-soft"
									onClick={() => {
										reset();
										setReference(r.name);
									}}
								>
									{shortRef(r.name)}
								</button>
							))}
					</div>
				))}
			</aside>
			<div className="flex min-h-0 min-w-0 flex-1 flex-col gap-2">
				<div className="flex flex-wrap items-center gap-2">
					<input
						data-testid="history-search"
						aria-label={t("searchHistory")}
						placeholder={t("searchHistory")}
						className="min-w-40 flex-1 rounded border border-border px-3 py-2 text-sm"
						value={query}
						onChange={(e) => {
							reset();
							setQuery(e.target.value);
						}}
					/>
					<Button
						data-testid="load-history"
						size="sm"
						variant="secondary"
						isDisabled={loading}
						onPress={() => {
							reset();
							setReload((n) => n + 1);
						}}
					>
						{loading ? <Spinner size="sm" /> : t("refreshChanges")}
					</Button>
					<Button
						data-testid="copy-commits"
						size="sm"
						isDisabled={!ends || loading}
						onPress={copy}
					>
						{t("copyCommits")}
					</Button>
				</div>
				<div className="flex items-center justify-between text-xs text-muted">
					<span>
						{ends
							? t("selectedCommits", { count: highlighted.size })
							: t("timelineHint")}
					</span>
					<span>
						{t("loadedCommits", {
							count: history?.commits.length ?? 0,
						})}
					</span>
				</div>
				{error && (
					<p
						data-testid="history-error"
						role="alert"
						className="text-sm text-danger"
					>
						{error}
					</p>
				)}
				{history?.commits.length === 0 && (
					<p
						data-testid="empty-history"
						className="p-4 text-sm text-muted"
					>
						{t("noHistory")}
					</p>
				)}
				<div
					data-testid="history-graph"
					className="min-h-40 flex-1 overflow-auto rounded border border-border select-none"
					onClickCapture={(e) => {
						shift.current = e.shiftKey;
					}}
				>
					{entries.length > 0 && (
						<GitLog
							entries={entries}
							currentBranch={
								entries.find((e) => e.hash === history?.head)
									?.branch ??
								entries[0]?.branch ??
								"HEAD"
							}
							showGitIndex={false}
							enableSelectedCommitStyling={false}
							defaultGraphWidth={140}
							onSelectCommit={(c) => {
								if (c) {
									setEnds((prev) =>
										clickCommit(
											prev,
											c.hash,
											shift.current,
										),
									);
									setFocused(c.hash);
								}
							}}
						>
							<GitLog.GraphHTMLGrid nodeSize={12} />
							<GitLog.Table
								// oxlint-disable-next-line react/no-unstable-nested-components -- the library calls row as a plain render function
								row={({ commit: c }) => (
									<div
										data-commit={c.hash}
										data-selected={
											highlighted.has(c.hash) || undefined
										}
										className="flex h-10 cursor-pointer items-center gap-2 px-2 text-sm data-selected:bg-accent-soft"
									>
										<span className="font-mono text-xs text-muted">
											{c.hash.slice(0, 8)}
										</span>
										{history?.head === c.hash && (
											<b className="rounded bg-accent-soft px-1 text-xs">
												HEAD
											</b>
										)}
										{refs
											.filter((r) => r.sha === c.hash)
											.map((r) => (
												<span
													key={r.name}
													title={r.name}
													className="max-w-32 truncate rounded border border-border px-1 text-xs text-accent"
												>
													{shortRef(r.name)}
												</span>
											))}
										<span className="min-w-0 flex-1 truncate">
											{c.message}
										</span>
										<span className="text-xs text-muted">
											{c.author?.name}
										</span>
									</div>
								)}
							/>
						</GitLog>
					)}
				</div>
				{history?.hasMore && (
					<Button
						data-testid="load-more-history"
						size="sm"
						variant="ghost"
						isDisabled={loading}
						onPress={() => setSkip(history.commits.length)}
					>
						{t("loadMoreHistory")}
					</Button>
				)}
				{focused && (
					<section
						data-testid="commit-details"
						data-commit-sha={focused}
						className="flex h-64 min-h-48 shrink-0 gap-2"
					>
						<div className="w-64 shrink-0 overflow-auto rounded border border-border p-2">
							<strong
								className="block truncate text-sm"
								title={commit?.subject}
							>
								{commit?.subject}
							</strong>
							<p className="mb-3 text-xs text-muted">
								{commit?.authorName} ·{" "}
								{commit?.authorDate.slice(0, 10)}
							</p>
							{detailError && (
								<p role="alert" className="text-danger">
									{detailError}
								</p>
							)}
							{details && (
								<ChangesTree
									changes={details.changes}
									onPreview={setPreview}
								/>
							)}
						</div>
						<SourcePreviewPane
							key={`${focused}:${preview ?? ""}`}
							repo={history?.root ?? repo}
							source={source}
							path={preview}
						/>
					</section>
				)}
			</div>
		</div>
	);
}
