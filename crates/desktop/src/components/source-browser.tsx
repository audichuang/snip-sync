import Tree, { type DataNode } from "@rc-component/tree";
import "@rc-component/tree/assets/index.css";
import { invoke } from "@tauri-apps/api/core";
import { useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import type { DirectoryEntry } from "../generated/DirectoryEntry";
import type { GitChange } from "../generated/GitChange";
import type { GitSourceDto } from "../generated/GitSourceDto";
import type { SourcePreview } from "../generated/SourcePreview";
import { DiffBody } from "./diff-view";

interface FileNode extends DataNode {
	key: string;
	name: string;
	children?: FileNode[];
}
const nodes = (entries: DirectoryEntry[]): FileNode[] =>
	entries.map((e) => ({
		key: e.path,
		name: e.name,
		isLeaf: !e.directory,
		disableCheckbox: e.symlink,
	}));
function addChildren(
	tree: FileNode[],
	key: string,
	children: FileNode[],
): FileNode[] {
	return tree.map((node) =>
		node.key === key
			? { ...node, children }
			: node.children
				? {
						...node,
						children: addChildren(node.children, key, children),
					}
				: node,
	);
}

export function FileExplorer({
	repo,
	paths,
	onPaths,
	onPreview,
}: {
	repo: string;
	paths: string[];
	onPaths: (paths: string[]) => void;
	onPreview: (path: string) => void;
}) {
	const { t } = useTranslation();
	const [tree, setTree] = useState<FileNode[]>([]);
	const [error, setError] = useState("");
	useEffect(() => {
		let cancelled = false;
		void invoke<DirectoryEntry[]>("browse_directory", { repo, path: "" })
			.then((entries) => {
				if (!cancelled) setTree(nodes(entries));
				return null;
			})
			.catch((e: unknown) => {
				if (!cancelled) setError(String(e));
			});
		return () => {
			cancelled = true;
		};
	}, [repo]);
	return (
		<div data-testid="file-explorer" className="min-w-0 overflow-auto p-2">
			{error && (
				<p role="alert" className="text-danger">
					{error}
				</p>
			)}
			<Tree<FileNode>
				className="source-tree"
				treeData={tree}
				checkable
				checkedKeys={paths}
				showIcon={false}
				onCheck={(keys) =>
					onPaths(
						(Array.isArray(keys) ? keys : keys.checked).map(String),
					)
				}
				onSelect={(_, info) => {
					if (info.node.isLeaf) onPreview(info.node.key);
				}}
				loadData={async (node) => {
					try {
						const children = nodes(
							await invoke<DirectoryEntry[]>("browse_directory", {
								repo,
								path: node.key,
							}),
						);
						setTree((prev) =>
							addChildren(prev, node.key, children),
						);
					} catch (e: unknown) {
						setError(String(e));
						throw e;
					}
				}}
				// oxlint-disable-next-line react/no-unstable-nested-components -- rc-tree calls titleRender as a render function
				titleRender={(node) => (
					<span
						data-testid={`explorer-entry-${node.key}`}
						className="inline-flex items-center gap-2 text-sm"
					>
						<span aria-hidden>{node.isLeaf ? "▤" : "▸"}</span>
						{node.name}
					</span>
				)}
			/>
			{tree.length === 0 && !error && (
				<p className="p-2 text-sm text-muted">{t("emptyDirectory")}</p>
			)}
		</div>
	);
}

interface ChangeNode extends DataNode {
	key: string;
	name: string;
	file?: GitChange;
	children?: ChangeNode[];
}
function changeTree(changes: GitChange[]): ChangeNode[] {
	const root: ChangeNode = { key: "", name: "", children: [] };
	for (const change of changes) {
		const parts = change.path.split("/");
		let parent = root;
		for (const part of parts.slice(0, -1)) {
			const key = `${parent.key}${part}/`;
			let child = parent.children?.find((n) => n.key === key);
			if (!child) {
				child = { key, name: part, children: [] };
				parent.children!.push(child);
			}
			parent = child;
		}
		parent.children!.push({
			key: change.path,
			name: parts.at(-1) ?? change.path,
			file: change,
			isLeaf: true,
		});
	}
	return root.children!;
}
const descendants = (node: ChangeNode): string[] =>
	node.file
		? [node.key]
		: (node.children ?? []).flatMap((child) => descendants(child));

export function ChangesTree({
	changes,
	selected,
	onSelected,
	onPreview,
}: {
	changes: GitChange[];
	selected?: Set<string>;
	onSelected?: (paths: Set<string>) => void;
	onPreview: (path: string) => void;
}) {
	const tree = useMemo(() => changeTree(changes), [changes]);
	return (
		<Tree<ChangeNode>
			className="source-tree"
			key={changes.map((c) => c.path).join("\0")}
			treeData={tree}
			defaultExpandAll
			showIcon={false}
			onSelect={(_, info) => {
				if (info.node.file) onPreview(info.node.key);
			}}
			// oxlint-disable-next-line react/no-unstable-nested-components -- rc-tree calls titleRender as a render function
			titleRender={(node) => (
				<span className="inline-flex items-center gap-2 py-0.5 text-sm">
					{selected && onSelected && (
						<input
							type="checkbox"
							aria-label={node.key}
							data-testid={
								node.file
									? `git-change-${node.key}`
									: `git-folder-${node.key}`
							}
							checked={descendants(node).every((p) =>
								selected.has(p),
							)}
							ref={(el) => {
								if (el) {
									const count = descendants(node).filter(
										(p) => selected.has(p),
									).length;
									el.indeterminate =
										count > 0 &&
										count < descendants(node).length;
								}
							}}
							onClick={(event) => event.stopPropagation()}
							onChange={(event) => {
								const next = new Set(selected);
								for (const p of descendants(node)) {
									if (event.target.checked) next.add(p);
									else next.delete(p);
								}
								onSelected(next);
							}}
						/>
					)}
					<span
						data-testid={`preview-file-${node.key}`}
						className="font-mono"
					>
						{node.name}
					</span>
					{node.file && (
						<span className="text-xs text-muted">
							{node.file.changeType}
						</span>
					)}
				</span>
			)}
		/>
	);
}

export function SourcePreviewPane({
	repo,
	path,
	source,
}: {
	repo: string;
	path: string | null;
	source: GitSourceDto | null;
}) {
	const { t } = useTranslation();
	const [result, setResult] = useState<SourcePreview | null>(null);
	const [error, setError] = useState("");
	const [mode, setMode] = useState<"content" | "diff">("diff");
	const sourceKey = JSON.stringify(source);
	useEffect(() => {
		let cancelled = false;
		const timer = setTimeout(() => {
			setResult(null);
			setError("");
			if (!path) return;
			void invoke<SourcePreview>("preview_source", {
				repo,
				path,
				source: JSON.parse(sourceKey) as GitSourceDto | null,
			})
				.then((data) => {
					if (!cancelled) setResult(data);
					return null;
				})
				.catch((e: unknown) => {
					if (!cancelled) setError(String(e));
				});
		}, 0);
		return () => {
			cancelled = true;
			clearTimeout(timer);
		};
	}, [repo, path, sourceKey]);
	return (
		<section
			data-testid="source-preview"
			data-path={path ?? ""}
			className="flex min-h-0 min-w-0 flex-1 flex-col overflow-hidden rounded border border-border bg-background"
		>
			<div className="flex items-center justify-between gap-3 border-b border-border px-4 py-2 text-sm">
				<span className="truncate font-mono" title={path ?? ""}>
					{path ?? t("sourcePreview")}
				</span>
				{source && (
					<div className="flex shrink-0 gap-3">
						<button
							type="button"
							data-testid="preview-content"
							aria-pressed={mode === "content"}
							onClick={() => setMode("content")}
						>
							{t("fileContent")}
						</button>
						<button
							type="button"
							data-testid="preview-diff"
							aria-pressed={mode === "diff"}
							onClick={() => setMode("diff")}
						>
							{t("showDiff")}
						</button>
					</div>
				)}
			</div>
			<div className="min-h-0 flex-1 overflow-auto p-3">
				{path === null && (
					<p className="p-6 text-sm text-muted">
						{t("choosePreview")}
					</p>
				)}
				{path !== null && error && (
					<p role="alert" className="text-danger">
						{error}
					</p>
				)}
				{path !== null && !error && result === null && (
					<p>{t("loadingPreview")}</p>
				)}
				{result && result.content === null && (
					<p>{t("binaryPreview")}</p>
				)}
				{result &&
					result.content !== null &&
					(source && mode === "diff" ? (
						<DiffBody patch={result.patch} />
					) : (
						<pre
							data-testid="source-content"
							className="font-mono text-xs leading-6 whitespace-pre"
						>
							{result.content}
						</pre>
					))}
			</div>
		</section>
	);
}
