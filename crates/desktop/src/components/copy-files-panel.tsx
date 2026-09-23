import {
	Button,
	Input,
	Label,
	TextField,
	ToggleButton,
	ToggleButtonGroup,
} from "@heroui/react";
import { open } from "@tauri-apps/plugin-dialog";
import { useState } from "react";
import { useTranslation } from "react-i18next";
import type { CopyRequest } from "../generated/CopyRequest";
import type { GitSourceDto } from "../generated/GitSourceDto";

type SourceKind = "files" | GitSourceDto["kind"];

const SOURCES: { id: SourceKind; label: "sourceFiles" | "sourceWorking" | "sourceStaged" | "sourceCommit" | "sourceRange" }[] = [
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
	const [kind, setKind] = useState<SourceKind>("files");
	const [paths, setPaths] = useState<string[]>([]);
	const [sha, setSha] = useState("HEAD");
	const [base, setBase] = useState("HEAD~1");
	const [tip, setTip] = useState("HEAD");

	async function handleAdd(directory: boolean) {
		const picked = await open({ multiple: true, directory, defaultPath: repo });
		const list = picked === null ? [] : Array.isArray(picked) ? picked : [picked];
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
		onCopy({ kind: "git", repo, roots, source });
	}

	return (
		<div className="flex flex-col gap-4">
			<ToggleButtonGroup
				selectionMode="single"
				disallowEmptySelection
				selectedKeys={[kind]}
				onSelectionChange={(keys) => {
					const [next] = keys;
					if (next !== undefined) setKind(next as SourceKind);
				}}
			>
				{SOURCES.map((s, i) => (
					<ToggleButton key={s.id} id={s.id}>
						{i > 0 && <ToggleButtonGroup.Separator />}
						{t(s.label)}
					</ToggleButton>
				))}
			</ToggleButtonGroup>

			{kind === "files" && (
				<div className="flex flex-col gap-2">
					<div className="flex gap-2">
						<Button size="sm" variant="secondary" onPress={() => void handleAdd(false)}>
							{t("addFiles")}
						</Button>
						<Button size="sm" variant="secondary" onPress={() => void handleAdd(true)}>
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
				<TextField className="max-w-sm" value={sha} onChange={setSha}>
					<Label>{t("commitSha")}</Label>
					<Input className="font-mono" />
				</TextField>
			)}

			{kind === "range" && (
				<div className="flex flex-wrap gap-4">
					<TextField className="w-48" value={base} onChange={setBase}>
						<Label>{t("rangeBase")}</Label>
						<Input className="font-mono" />
					</TextField>
					<TextField className="w-48" value={tip} onChange={setTip}>
						<Label>{t("rangeTip")}</Label>
						<Input className="font-mono" />
					</TextField>
				</div>
			)}

			<div>
				<Button onPress={handleCopy}>{t("copy")}</Button>
			</div>
		</div>
	);
}
