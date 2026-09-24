import {
	Button,
	Input,
	Label,
	TextField,
	ToggleButton,
	ToggleButtonGroup,
} from "@heroui/react";
import { open } from "@tauri-apps/plugin-dialog";
import { invoke } from "@tauri-apps/api/core";
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import type { CopyRequest } from "../generated/CopyRequest";
import type { CommitSummary } from "../generated/CommitSummary";
import type { GitBrowse } from "../generated/GitBrowse";
import type { GitSourceDto } from "../generated/GitSourceDto";

type SourceKind = "files" | GitSourceDto["kind"];

const SOURCES: {
	id: SourceKind;
	label:
		| "sourceFiles"
		| "sourceWorking"
		| "sourceStaged"
		| "sourceCommit"
		| "sourceRange";
}[] = [
	{ id: "files", label: "sourceFiles" },
	{ id: "working", label: "sourceWorking" },
	{ id: "staged", label: "sourceStaged" },
	{ id: "commit", label: "sourceCommit" },
	{ id: "range", label: "sourceRange" },
];

/** File mode (spec 3.1): pick a source, then copy. */
export function CopyFilesPanel({
	repo,
	onCopy,
}: {
	repo: string;
	onCopy: (request: CopyRequest) => void;
}) {
	const { t } = useTranslation();
	const [kind, setKind] = useState<SourceKind>("working");
	const [paths, setPaths] = useState<string[]>([]);
	const [sha, setSha] = useState("HEAD");
	const [base, setBase] = useState("HEAD~1");
	const [tip, setTip] = useState("HEAD");
	const [browse, setBrowse] = useState<GitBrowse | null>(null);
	const [browseError, setBrowseError] = useState("");
	const [selectedPaths, setSelectedPaths] = useState<Set<string> | null>(
		null,
	);
	const [history, setHistory] = useState<CommitSummary[]>([]);
	const [refresh, setRefresh] = useState(0);

	useEffect(() => {
		if (!repo || kind === "files") return;
		let cancelled = false;
		const timer = setTimeout(() => {
			const source: GitSourceDto =
				kind === "commit"
					? { kind, sha }
					: kind === "range"
						? { kind, base, tip }
						: { kind };
			void (async () => {
				try {
					const data = await invoke<GitBrowse>("browse_git", {
						repo,
						source,
					});
					if (!cancelled) {
						setBrowse(data);
						setBrowseError("");
					}
				} catch (error: unknown) {
					if (!cancelled) {
						setBrowse(null);
						setBrowseError(String(error));
					}
				}
			})();
		}, 200);
		return () => {
			cancelled = true;
			clearTimeout(timer);
		};
		// oxlint-disable-next-line react/exhaustive-effect-dependencies -- refresh explicitly reloads the current source
	}, [repo, kind, sha, base, tip, refresh]);

	useEffect(() => {
		if (!repo || kind === "files") return;
		let cancelled = false;
		void (async () => {
			try {
				const data = await invoke<CommitSummary[]>("list_commits", {
					repo,
					limit: 20,
				});
				if (!cancelled) setHistory(data);
			} catch {
				if (!cancelled) setHistory([]);
			}
		})();
		return () => {
			cancelled = true;
		};
		// oxlint-disable-next-line react/exhaustive-effect-dependencies -- refresh reloads the recent commit picker
	}, [repo, kind, refresh]);

	function changeSource(next: SourceKind) {
		setKind(next);
		setBrowse(null);
		setBrowseError("");
		setSelectedPaths(null);
	}
	function changeRevision(setter: (value: string) => void, value: string) {
		setter(value);
		setBrowse(null);
		setBrowseError("");
		setSelectedPaths(null);
	}

	function togglePath(path: string) {
		setSelectedPaths((prev) => {
			const next = new Set(
				prev ?? browse?.changes.map((c) => c.path) ?? [],
			);
			if (next.has(path)) next.delete(path);
			else next.add(path);
			return next;
		});
	}

	async function handleAdd(directory: boolean) {
		const picked = await open({
			multiple: true,
			directory,
			defaultPath: repo,
		});
		const list =
			picked === null ? [] : Array.isArray(picked) ? picked : [picked];
		setPaths((prev) => [...new Set([...prev, ...list])]);
	}

	function handleCopy() {
		// No repo: send no roots so the backend reports "No workspace folder found."
		const roots = repo ? [repo] : [];
		if (kind === "files") {
			onCopy({ kind: "files", roots, paths });
			return;
		}
		const source: GitSourceDto =
			kind === "commit"
				? { kind, sha }
				: kind === "range"
					? { kind, base, tip }
					: { kind };
		onCopy({
			kind: "git",
			repo,
			roots,
			source,
			selected_paths: selectedPaths ? [...selectedPaths] : null,
		});
	}

	const folders = new Map<string, NonNullable<GitBrowse>["changes"]>();
	for (const change of browse?.changes ?? []) {
		const folder = change.path.includes("/")
			? change.path.slice(0, change.path.lastIndexOf("/"))
			: ".";
		folders.set(folder, [...(folders.get(folder) ?? []), change]);
	}
	const selectedCount = selectedPaths
		? selectedPaths.size
		: (browse?.changes.length ?? 0);

	return (
		<div className="flex flex-col gap-4">
			<ToggleButtonGroup
				selectionMode="single"
				disallowEmptySelection
				selectedKeys={[kind]}
				onSelectionChange={(keys) => {
					const [next] = keys;
					if (next !== undefined) changeSource(next as SourceKind);
				}}
			>
				{SOURCES.map((s, i) => (
					<ToggleButton
						key={s.id}
						id={s.id}
						data-testid={`source-${s.id}`}
					>
						{i > 0 && <ToggleButtonGroup.Separator />}
						{t(s.label)}
					</ToggleButton>
				))}
			</ToggleButtonGroup>

			{kind === "files" && (
				<div className="flex flex-col gap-2">
					<div className="flex gap-2">
						<Button
							size="sm"
							variant="secondary"
							onPress={() => void handleAdd(false)}
						>
							{t("addFiles")}
						</Button>
						<Button
							size="sm"
							variant="secondary"
							onPress={() => void handleAdd(true)}
						>
							{t("addFolders")}
						</Button>
						<Button
							size="sm"
							variant="ghost"
							isDisabled={paths.length === 0}
							onPress={() => setPaths([])}
						>
							{t("clearPaths")}
						</Button>
					</div>
					{paths.length === 0 ? (
						<p className="text-sm text-muted">{t("noPaths")}</p>
					) : (
						<ul className="max-h-60 overflow-auto rounded border border-border p-2 font-mono text-xs">
							{paths.map((p) => (
								<li key={p} className="truncate">
									{p}
								</li>
							))}
						</ul>
					)}
				</div>
			)}

			{kind === "commit" && (
				<div className="flex flex-col gap-2">
					<select
						data-testid="history-commit"
						aria-label={t("recentCommits")}
						className="max-w-xl rounded border border-border bg-background p-2 text-sm"
						value={history.some((c) => c.sha === sha) ? sha : ""}
						onChange={(e) => changeRevision(setSha, e.target.value)}
					>
						<option value="">{t("recentCommits")}</option>
						{history.map((c) => (
							<option key={c.sha} value={c.sha}>
								{c.sha.slice(0, 8)} · {c.subject}
							</option>
						))}
					</select>
					<TextField
						className="max-w-sm"
						value={sha}
						onChange={(value) => changeRevision(setSha, value)}
					>
						<Label>{t("commitSha")}</Label>
						<Input data-testid="commit-sha" className="font-mono" />
					</TextField>
				</div>
			)}

			{kind !== "files" && (
				<section
					data-testid="git-browser"
					data-loading={!browse && !browseError}
					className="flex min-h-0 flex-col gap-2 rounded border border-border p-3"
				>
					<div className="flex flex-wrap items-center gap-2 text-sm">
						<strong>
							{browse
								? `${browse.branch} · ${browse.scope || "."}`
								: t("gitChanges")}
						</strong>
						{browse && (
							<span
								className="truncate font-mono text-xs text-muted"
								title={browse.root}
							>
								{browse.root}
							</span>
						)}
						<span className="ml-auto">
							{t("selectedFiles", { count: selectedCount })}
						</span>
						<Button
							size="sm"
							variant="ghost"
							data-testid="refresh-changes"
							onPress={() => {
								setSelectedPaths(null);
								setBrowse(null);
								setBrowseError("");
								setRefresh((n) => n + 1);
							}}
						>
							{t("refreshChanges")}
						</Button>
					</div>
					{browseError && (
						<p className="text-sm text-danger">{browseError}</p>
					)}
					{browse && (
						<>
							<div className="flex gap-3 text-xs">
								<button
									type="button"
									data-testid="select-all-changes"
									onClick={() => setSelectedPaths(null)}
								>
									{t("selectAll")}
								</button>
								<button
									type="button"
									data-testid="clear-changes"
									onClick={() => setSelectedPaths(new Set())}
								>
									{t("clearSelection")}
								</button>
							</div>
							{browse.changes.length === 0 ? (
								<p className="text-sm text-muted">
									{t("noGitChanges")}
								</p>
							) : (
								<div
									data-testid="git-changes"
									className="max-h-64 overflow-auto font-mono text-xs"
								>
									{[...folders]
										.toSorted(([a], [b]) =>
											a.localeCompare(b),
										)
										.map(([folder, changes]) => (
											<details key={folder} open>
												<summary className="cursor-pointer py-1 font-semibold">
													{folder}
												</summary>
												{changes.map((change) => (
													<label
														key={change.path}
														className="flex cursor-pointer items-center gap-2 py-1 pl-4"
													>
														<input
															type="checkbox"
															data-testid={`git-change-${change.path}`}
															checked={
																selectedPaths
																	? selectedPaths.has(
																			change.path,
																		)
																	: true
															}
															onChange={() =>
																togglePath(
																	change.path,
																)
															}
														/>
														<span className="w-20 shrink-0 text-muted">
															{change.changeType}
														</span>
														<span
															className="truncate"
															title={change.path}
														>
															{change.path.slice(
																folder === "."
																	? 0
																	: folder.length +
																			1,
															)}
														</span>
													</label>
												))}
											</details>
										))}
								</div>
							)}
							{history.length > 0 && (
								<div className="border-t border-border pt-2">
									<strong className="text-sm">
										{t("recentCommits")}
									</strong>
									<div className="max-h-36 overflow-auto">
										{history.map((c) => (
											<button
												key={c.sha}
												type="button"
												data-testid={`history-row-${c.sha}`}
												className="flex w-full items-center gap-3 rounded px-2 py-1 text-left text-xs hover:bg-accent-soft"
												onClick={() => {
													changeSource("commit");
													changeRevision(
														setSha,
														c.sha,
													);
												}}
											>
												<span className="font-mono text-muted">
													{c.sha.slice(0, 8)}
												</span>
												<span className="min-w-0 flex-1 truncate">
													{c.subject}
												</span>
												<span className="shrink-0 text-muted">
													{c.authorName}
												</span>
											</button>
										))}
									</div>
								</div>
							)}
						</>
					)}
				</section>
			)}

			{kind === "range" && (
				<div className="flex flex-wrap gap-4">
					<TextField
						className="w-48"
						value={base}
						onChange={(value) => changeRevision(setBase, value)}
					>
						<Label>{t("rangeBase")}</Label>
						<Input data-testid="range-base" className="font-mono" />
					</TextField>
					<TextField
						className="w-48"
						value={tip}
						onChange={(value) => changeRevision(setTip, value)}
					>
						<Label>{t("rangeTip")}</Label>
						<Input data-testid="range-tip" className="font-mono" />
					</TextField>
				</div>
			)}

			<div>
				<Button
					data-testid="copy-files"
					isDisabled={
						kind !== "files" && (!browse || selectedCount === 0)
					}
					onPress={handleCopy}
				>
					{t("copy")}
				</Button>
			</div>
		</div>
	);
}
