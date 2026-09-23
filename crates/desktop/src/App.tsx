import { Button, Input, Tabs, TextField, Toast, toast } from "@heroui/react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import { load } from "@tauri-apps/plugin-store";
import { useEffect, useRef, useState, type Key } from "react";
import { useTranslation } from "react-i18next";
import { CommitTimeline } from "./components/commit-timeline";
import { CopyFilesPanel } from "./components/copy-files-panel";
import { PastePanel, usePaste } from "./components/paste-panel";
import type { CommitCopySummary } from "./generated/CommitCopySummary";
import type { CommitSelection } from "./generated/CommitSelection";
import type { CopyDone } from "./generated/CopyDone";
import type { CopyOutcome } from "./generated/CopyOutcome";
import type { CopyRequest } from "./generated/CopyRequest";
import {
	commitCopyNote,
	copyNote,
	type CopyKind,
	type CopyNote,
} from "./lib/copy-message";
import { errorText, setLanguage } from "./lib/i18n";
import { showCopyNotification } from "./lib/settings";

type Tab = "files" | "commits" | "paste";

// Copy-success toasts honour showCopyNotification like TS; errors always show.
async function showNote(note: CopyNote) {
	const stored = await load("settings.json")
		.then((store) => store.get("settings"))
		.catch(() => {});
	if (showCopyNotification(stored)) toast[note.severity](note.text);
}

export default function App() {
	const { t, i18n } = useTranslation();
	const [repo, setRepo] = useState("");
	// The header path field: typed or pasted paths apply on Enter / blur.
	const [repoDraft, setRepoDraft] = useState("");
	const [tab, setTab] = useState<Tab>("files");
	const paste = usePaste(repo);
	// Which wording the tray's "copy last selection" result gets.
	const lastCopyKind = useRef<CopyKind>("files");

	// Latest values for the long-lived tray listeners below.
	const live = useRef({ paste, t });
	live.current = { paste, t };

	// Tray events: subscribed for the app's lifetime.
	useEffect(() => {
		const offs = [
			listen<CopyDone>("tray-copied", ({ payload }) => {
				const tr = live.current.t;
				void showNote(
					payload.mode === "commits"
						? commitCopyNote(tr, payload)
						: copyNote(tr, lastCopyKind.current, payload),
				);
			}),
			listen<string>("tray-copy-failed", ({ payload }) => {
				toast.danger(errorText(live.current.t, payload));
			}),
			listen("tray-paste", () => {
				setTab("paste");
				void live.current.paste.preview();
			}),
		];
		return () =>
			offs.forEach((off) => void off.then((unlisten) => unlisten()));
	}, []);

	// The tray menu is native; hand it the current language's labels.
	useEffect(() => {
		const labels = {
			paste: t("trayPaste"),
			copyLast: t("trayCopyLast"),
			show: t("trayShow"),
			quit: t("trayQuit"),
		};
		invoke("set_tray_labels", { labels }).catch((error: unknown) => {
			console.error("Failed to update tray labels:", error);
		});
	}, [t, i18n.language]);

	async function handleChooseRepo() {
		const dir = await open({ directory: true });
		if (typeof dir === "string") applyRepo(dir);
	}

	function applyRepo(dir: string) {
		const next = dir.trim();
		setRepoDraft(next);
		if (next === repo) return;
		setRepo(next);
		paste.setState({ step: "idle" });
	}

	async function handleCopy(request: CopyRequest) {
		try {
			const outcome = await invoke<CopyOutcome>("copy", { request });
			const kind: CopyKind = request.kind === "files" ? "files" : "git";
			lastCopyKind.current = kind;
			void showNote(copyNote(t, kind, outcome));
		} catch (error: unknown) {
			toast.danger(errorText(t, error));
		}
	}

	async function handleCopyCommits(selection: CommitSelection) {
		try {
			void showNote(
				commitCopyNote(
					t,
					await invoke<CommitCopySummary>("copy_commits", {
						repo,
						selection,
					}),
				),
			);
		} catch (error: unknown) {
			toast.danger(errorText(t, error));
		}
	}

	return (
		<div className="flex h-screen flex-col gap-3 bg-background p-4 text-foreground">
			<Toast.Provider placement="bottom end" />
			<header className="flex items-center gap-3">
				<h1 className="text-lg font-semibold">{t("appTitle")}</h1>
				<Button
					size="sm"
					variant="secondary"
					onPress={() => void handleChooseRepo()}
				>
					{t("chooseRepo")}
				</Button>
				<TextField
					aria-label={t("repoPath")}
					className="min-w-0 flex-1"
				>
					<Input
						data-testid="repo-path"
						className="font-mono text-sm"
						value={repoDraft}
						placeholder={t("noRepo")}
						onChange={(e) => setRepoDraft(e.target.value)}
						onBlur={() => applyRepo(repoDraft)}
						onKeyDown={(e) => {
							if (e.key === "Enter") applyRepo(repoDraft);
						}}
					/>
				</TextField>
				<Button
					size="sm"
					variant="ghost"
					onPress={() =>
						setLanguage(i18n.language === "en" ? "zh-Hant" : "en")
					}
				>
					{t("language")}
				</Button>
			</header>

			<Tabs
				className="flex min-h-0 flex-1 flex-col"
				selectedKey={tab}
				onSelectionChange={(key: Key) => setTab(key as Tab)}
			>
				<Tabs.ListContainer>
					<Tabs.List
						aria-label={t("appTitle")}
						className="inline-flex w-auto"
					>
						<Tabs.Tab id="files" className="min-w-max">
							{t("tabFiles")}
							<Tabs.Indicator />
						</Tabs.Tab>
						<Tabs.Tab id="commits" className="min-w-max">
							{t("tabCommits")}
							<Tabs.Indicator />
						</Tabs.Tab>
						<Tabs.Tab id="paste" className="min-w-max">
							{t("tabPaste")}
							<Tabs.Indicator />
						</Tabs.Tab>
					</Tabs.List>
				</Tabs.ListContainer>
				<Tabs.Panel id="files" className="pt-3">
					<CopyFilesPanel
						repo={repo}
						onCopy={(r) => void handleCopy(r)}
					/>
				</Tabs.Panel>
				<Tabs.Panel
					id="commits"
					className="flex min-h-0 flex-1 flex-col pt-3"
				>
					<CommitTimeline
						key={repo}
						repo={repo}
						onCopy={(s) => void handleCopyCommits(s)}
					/>
				</Tabs.Panel>
				<Tabs.Panel
					id="paste"
					className="flex min-h-0 flex-1 flex-col pt-3"
				>
					<PastePanel paste={paste} />
				</Tabs.Panel>
			</Tabs>
		</div>
	);
}
