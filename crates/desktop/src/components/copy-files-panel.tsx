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
import { ChangesTree, FileExplorer, SourcePreviewPane } from "./source-browser";

type SourceKind = "files" | GitSourceDto["kind"];
const SOURCES = [
	{ id: "files", label: "sourceFiles" },
	{ id: "working", label: "sourceWorking" },
	{ id: "staged", label: "sourceStaged" },
	{ id: "commit", label: "sourceCommit" },
	{ id: "range", label: "sourceRange" },
] as const;

export function CopyFilesPanel({
	repo,
	onCopy,
}: {
	repo: string;
	onCopy: (request: CopyRequest) => void;
}) {
	const { t } = useTranslation();
	const [kind, setKind] = useState<SourceKind>("files");
	const [paths, setPaths] = useState<string[]>([]);
	const [extraPaths, setExtraPaths] = useState<string[]>([]);
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
	const [preview, setPreview] = useState<string | null>(null);
	const source: GitSourceDto | null =
		kind === "files"
			? null
			: kind === "commit"
				? { kind, sha }
				: kind === "range"
					? { kind, base, tip }
					: { kind };
	const sourceKey = JSON.stringify(source);

	useEffect(() => {
		if (!repo || kind === "files") return;
		let cancelled = false;
		const timer = setTimeout(() => {
			void invoke<GitBrowse>("browse_git", {
				repo,
				source: JSON.parse(sourceKey) as GitSourceDto,
			})
				.then((data) => {
					if (!cancelled) {
						setBrowse(data);
						setBrowseError("");
					}
					return null;
				})
				.catch((error: unknown) => {
					if (!cancelled) {
						setBrowse(null);
						setBrowseError(String(error));
					}
				});
		}, 200);
		return () => {
			cancelled = true;
			clearTimeout(timer);
		};
		// oxlint-disable-next-line react/exhaustive-effect-dependencies -- explicit refresh reloads the source
	}, [repo, kind, sourceKey, refresh]);

	useEffect(() => {
		if (!repo) return;
		let cancelled = false;
		void invoke<CommitSummary[]>("list_commits", { repo, limit: 20 })
			.then((data) => {
				if (!cancelled) setHistory(data);
				return null;
			})
			.catch(() => {
				if (!cancelled) setHistory([]);
			});
		return () => {
			cancelled = true;
		};
		// oxlint-disable-next-line react/exhaustive-effect-dependencies -- explicit refresh reloads history
	}, [repo, refresh]);

	function resetBrowse() {
		setBrowse(null);
		setBrowseError("");
		setSelectedPaths(null);
		setPreview(null);
	}
	function changeSource(next: SourceKind) {
		setKind(next);
		resetBrowse();
	}
	function revision(setter: (value: string) => void, value: string) {
		setter(value);
		resetBrowse();
	}
	async function addPaths(directory: boolean) {
		const picked = await open({
			multiple: true,
			directory,
			defaultPath: repo,
		});
		const list =
			picked === null ? [] : Array.isArray(picked) ? picked : [picked];
		setExtraPaths((prev) => [...new Set([...prev, ...list])]);
	}
	const selected =
		selectedPaths ?? new Set(browse?.changes.map((c) => c.path) ?? []);
	const selectedCount =
		kind === "files" ? paths.length + extraPaths.length : selected.size;
	function copy() {
		const roots = repo ? [repo] : [];
		if (source) {
			onCopy({
				kind: "git",
				repo,
				roots,
				source,
				selected_paths: [...selected],
			});
			return;
		}
		const top = paths.filter(
			(p) =>
				!paths.some(
					(parent) => parent !== p && p.startsWith(`${parent}/`),
				),
		);
		onCopy({
			kind: "files",
			roots,
			paths: [...top.map((p) => `${repo}/${p}`), ...extraPaths],
		});
	}
	return (
		<div className="flex min-h-0 flex-1 flex-col gap-3">
			<div className="flex flex-wrap items-center justify-between gap-2">
				<ToggleButtonGroup
					selectionMode="single"
					disallowEmptySelection
					selectedKeys={[kind]}
					onSelectionChange={(keys) => {
						const [next] = keys;
						if (next !== undefined && next !== kind)
							changeSource(next as SourceKind);
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
				<div className="flex items-center gap-3">
					<span className="text-sm text-muted">
						{t("selectedFiles", { count: selectedCount })}
					</span>
					<Button
						data-testid="copy-files"
						isDisabled={
							selectedCount === 0 || (kind !== "files" && !browse)
						}
						onPress={copy}
					>
						{t("copy")}
					</Button>
				</div>
			</div>
			{kind === "commit" && (
				<div className="flex gap-3">
					<select
						data-testid="history-commit"
						aria-label={t("recentCommits")}
						value={history.some((c) => c.sha === sha) ? sha : ""}
						onChange={(e) => revision(setSha, e.target.value)}
						className="min-w-0 rounded border border-border p-2 text-sm"
					>
						<option value="">{t("recentCommits")}</option>
						{history.map((c) => (
							<option key={c.sha} value={c.sha}>
								{c.sha.slice(0, 8)} · {c.subject}
							</option>
						))}
					</select>
					<TextField
						value={sha}
						onChange={(value) => revision(setSha, value)}
					>
						<Label>{t("commitSha")}</Label>
						<Input data-testid="commit-sha" />
					</TextField>
				</div>
			)}
			{kind === "range" && (
				<div className="flex gap-3">
					<TextField
						value={base}
						onChange={(value) => revision(setBase, value)}
					>
						<Label>{t("rangeBase")}</Label>
						<Input data-testid="range-base" />
					</TextField>
					<TextField
						value={tip}
						onChange={(value) => revision(setTip, value)}
					>
						<Label>{t("rangeTip")}</Label>
						<Input data-testid="range-tip" />
					</TextField>
				</div>
			)}
			<div className="flex min-h-0 flex-1 gap-3">
				<aside
					className="flex min-h-0 w-80 shrink-0 resize-x flex-col overflow-auto rounded border border-border bg-default/30"
					style={{ minWidth: 220, maxWidth: "50%" }}
				>
					<div className="flex items-center justify-between border-b border-border px-3 py-2">
						<strong className="text-sm">
							{kind === "files"
								? t("projectFiles")
								: t("gitChanges")}
						</strong>
						<Button
							size="sm"
							variant="ghost"
							data-testid="refresh-changes"
							onPress={() => {
								resetBrowse();
								setRefresh((n) => n + 1);
							}}
						>
							{t("refreshChanges")}
						</Button>
					</div>
					{kind === "files" ? (
						<>
							{repo && (
								<FileExplorer
									key={`${repo}:${refresh}`}
									repo={repo}
									paths={paths}
									onPaths={setPaths}
									onPreview={setPreview}
								/>
							)}
							<div className="mt-auto border-t border-border p-2">
								<div className="flex flex-wrap gap-1">
									<Button
										size="sm"
										variant="ghost"
										onPress={() => void addPaths(false)}
									>
										{t("addFiles")}
									</Button>
									<Button
										size="sm"
										variant="ghost"
										onPress={() => void addPaths(true)}
									>
										{t("addFolders")}
									</Button>
									<Button
										size="sm"
										variant="ghost"
										onPress={() => {
											setPaths([]);
											setExtraPaths([]);
										}}
									>
										{t("clearPaths")}
									</Button>
								</div>
								{extraPaths.map((p) => (
									<p key={p} className="truncate text-xs">
										{p}
									</p>
								))}
							</div>
						</>
					) : (
						<div
							data-testid="git-browser"
							data-loading={!browse && !browseError}
							className="flex min-h-0 flex-1 flex-col"
						>
							{browse && (
								<div className="border-b border-border p-2 text-xs">
									<strong>
										{browse.branch} · {browse.scope || "."}
									</strong>
									<p
										className="truncate text-muted"
										title={browse.root}
									>
										{browse.root}
									</p>
									<div className="mt-2 flex gap-3">
										<button
											type="button"
											data-testid="select-all-changes"
											onClick={() =>
												setSelectedPaths(null)
											}
										>
											{t("selectAll")}
										</button>
										<button
											type="button"
											data-testid="clear-changes"
											onClick={() =>
												setSelectedPaths(new Set())
											}
										>
											{t("clearSelection")}
										</button>
									</div>
								</div>
							)}
							{browseError && (
								<p
									role="alert"
									className="p-3 text-sm text-danger"
								>
									{browseError}
								</p>
							)}
							{browse && (
								<div
									data-testid="git-changes"
									className="min-h-32 flex-1 overflow-auto p-2"
								>
									{browse.changes.length > 0 ? (
										<ChangesTree
											changes={browse.changes}
											selected={selected}
											onSelected={setSelectedPaths}
											onPreview={setPreview}
										/>
									) : (
										<p className="text-sm text-muted">
											{t("noGitChanges")}
										</p>
									)}
								</div>
							)}
							{history.length > 0 && (
								<details
									open
									className="border-t border-border p-2"
								>
									<summary className="text-sm">
										{t("recentCommits")}
									</summary>
									<div className="max-h-36 overflow-auto">
										{history.map((c) => (
											<button
												type="button"
												key={c.sha}
												data-testid={`history-row-${c.sha}`}
												className="block w-full truncate py-1 text-left text-xs"
												onClick={() => {
													changeSource("commit");
													setSha(c.sha);
												}}
											>
												{c.sha.slice(0, 8)} ·{" "}
												{c.subject}
											</button>
										))}
									</div>
								</details>
							)}
						</div>
					)}
				</aside>
				<SourcePreviewPane
					key={`${sourceKey}:${preview ?? ""}`}
					repo={repo}
					path={preview}
					source={source}
				/>
			</div>
		</div>
	);
}
