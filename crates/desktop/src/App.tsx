// Debug page: one button per Tauri command. The real screens land in T-12.
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import { useEffect, useRef, useState, type ReactNode } from "react";
import type { ClipboardPlan } from "./generated/ClipboardPlan";
import type { CommitCopySummary } from "./generated/CommitCopySummary";
import type { CommitSelection } from "./generated/CommitSelection";
import type { CommitSummary } from "./generated/CommitSummary";
import type { CopyDone } from "./generated/CopyDone";
import type { CopyOutcome } from "./generated/CopyOutcome";
import type { CopyRequest } from "./generated/CopyRequest";
import type { DiffTarget } from "./generated/DiffTarget";
import type { GitSourceDto } from "./generated/GitSourceDto";
import type { ReplayResult } from "./generated/ReplayResult";
import type { RestoreExecutionResult } from "./generated/RestoreExecutionResult";
import type { RestoreSelection } from "./generated/RestoreSelection";

function Btn({ onClick, children }: { onClick: () => void; children: ReactNode }) {
	return (
		<button
			type="button"
			className="rounded border border-gray-400 px-2 py-1 text-sm hover:bg-gray-100"
			onClick={onClick}
		>
			{children}
		</button>
	);
}

export default function App() {
	const [repo, setRepo] = useState("");
	const [paths, setPaths] = useState("");
	const [rev, setRev] = useState("HEAD");
	const [base, setBase] = useState("HEAD~3");
	const [count, setCount] = useState(3);
	const [diffIndex, setDiffIndex] = useState("0");
	const [output, setOutput] = useState("");

	const show = (value: unknown) =>
		setOutput(typeof value === "string" ? value : JSON.stringify(value, null, 2));

	async function run<T>(cmd: string, args?: Record<string, unknown>) {
		try {
			show(await invoke<T>(cmd, args));
		} catch (error: unknown) {
			show(`ERROR: ${String(error)}`);
		}
	}

	const repoRef = useRef(repo);
	repoRef.current = repo;
	// Tray events: subscribed for the page's lifetime.
	useEffect(() => {
		const offs = [
			listen<CopyDone>("tray-copied", (e) => show(e.payload)),
			listen<string>("tray-copy-failed", (e) => show(`ERROR: ${e.payload}`)),
			listen("tray-paste", () => {
				const roots = repoRef.current ? [repoRef.current] : [];
				void run<ClipboardPlan>("read_clipboard_plan", { roots });
			}),
		];
		return () => offs.forEach((off) => void off.then((unlisten) => unlisten()));
	}, []);

	const roots = repo ? [repo] : [];
	const copyGit = (source: GitSourceDto) => {
		const request: CopyRequest = { kind: "git", repo, roots, source };
		void run<CopyOutcome>("copy", { request });
	};
	const copyCommits = (selection: CommitSelection) =>
		void run<CommitCopySummary>("copy_commits", { repo, selection });
	const diff = () => {
		const [a, b] = diffIndex.split(":").map(Number);
		const target: DiffTarget =
			b === undefined ? { kind: "restore", index: a } : { kind: "commit", commit: a, file: b };
		void run<string>("diff", { target });
	};
	const selection: RestoreSelection = {
		overwriteExisting: true,
		skipExisting: false,
		uncheckedCreates: [],
		uncheckedDeletes: [],
	};

	return (
		<main className="flex flex-col gap-3 p-4 font-mono text-sm">
			<h1 className="text-lg font-bold">snip-sync debug</h1>
			<div className="flex items-center gap-2">
				<input
					className="flex-1 rounded border px-2 py-1"
					placeholder="repo / workspace root"
					value={repo}
					onChange={(e) => setRepo(e.target.value)}
				/>
				<Btn
					onClick={() =>
						void open({ directory: true }).then((dir) => {
							if (typeof dir === "string") setRepo(dir);
						})
					}
				>
					Browse
				</Btn>
			</div>

			<section className="flex flex-wrap items-center gap-2">
				<input
					className="flex-1 rounded border px-2 py-1"
					placeholder="paths, comma separated"
					value={paths}
					onChange={(e) => setPaths(e.target.value)}
				/>
				<Btn
					onClick={() => {
						const request: CopyRequest = {
							kind: "files",
							roots,
							paths: paths.split(",").map((p) => p.trim()).filter(Boolean),
						};
						void run<CopyOutcome>("copy", { request });
					}}
				>
					copy files
				</Btn>
			</section>

			<section className="flex flex-wrap items-center gap-2">
				<Btn onClick={() => copyGit({ kind: "working" })}>copy working</Btn>
				<Btn onClick={() => copyGit({ kind: "staged" })}>copy staged</Btn>
				<input className="w-28 rounded border px-2 py-1" value={base} onChange={(e) => setBase(e.target.value)} />
				<input className="w-28 rounded border px-2 py-1" value={rev} onChange={(e) => setRev(e.target.value)} />
				<Btn onClick={() => copyGit({ kind: "commit", sha: rev })}>copy commit</Btn>
				<Btn onClick={() => copyGit({ kind: "range", base, tip: rev })}>copy range</Btn>
			</section>

			<section className="flex flex-wrap items-center gap-2">
				<Btn onClick={() => void run<CommitSummary[]>("list_commits", { repo, limit: 50 })}>list_commits</Btn>
				<input
					className="w-16 rounded border px-2 py-1"
					type="number"
					min={1}
					value={count}
					onChange={(e) => setCount(Number(e.target.value))}
				/>
				<Btn onClick={() => copyCommits({ kind: "last", n: count })}>copy_commits -n</Btn>
				<Btn onClick={() => copyCommits({ kind: "range", base, tip: rev })}>copy_commits base..tip</Btn>
			</section>

			<section className="flex flex-wrap items-center gap-2">
				<Btn onClick={() => void run<ClipboardPlan>("read_clipboard_plan", { roots })}>read_clipboard_plan</Btn>
				<Btn onClick={() => void run<RestoreExecutionResult>("apply_restore", { selection })}>
					apply_restore (overwrite)
				</Btn>
				<Btn onClick={() => void run<ReplayResult>("replay_commits")}>replay_commits</Btn>
				<input
					className="w-20 rounded border px-2 py-1"
					placeholder="i or c:f"
					value={diffIndex}
					onChange={(e) => setDiffIndex(e.target.value)}
				/>
				<Btn onClick={diff}>diff</Btn>
			</section>

			<pre className="max-h-[60vh] overflow-auto rounded bg-gray-100 p-2 whitespace-pre-wrap">{output}</pre>
		</main>
	);
}
