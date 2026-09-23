import { Alert, Button, Chip, Spinner, toast } from "@heroui/react";
import Tree from "@rc-component/tree";
import "@rc-component/tree/assets/index.css";
import { invoke } from "@tauri-apps/api/core";
import { useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import type { ClipboardPlan } from "../generated/ClipboardPlan";
import type { CommitReplayPlan } from "../generated/CommitReplayPlan";
import type { FilePlan } from "../generated/FilePlan";
import type { ReplayResult } from "../generated/ReplayResult";
import type { RestoreBase } from "../generated/RestoreBase";
import type { RestoreBaseSuggestion } from "../generated/RestoreBaseSuggestion";
import type { RestoreExecutionResult } from "../generated/RestoreExecutionResult";
import type { RestorePlan } from "../generated/RestorePlan";
import { errorText } from "../lib/i18n";
import {
	buildPlanTree,
	confirmationSummary,
	existingCount,
	hasActionable,
	initialCheckedKeys,
	resultSummary,
	suggestionText,
	toSelection,
	type PlanNode,
} from "../lib/restore-plan";
import { DiffBody, DiffButton, useDiff } from "./diff-view";

type PasteState =
	| { step: "idle" }
	| {
			step: "files";
			plan: RestorePlan;
			suggestion: RestoreBaseSuggestion | null;
			checked: string[];
			skipExisting: boolean;
	  }
	| { step: "commits"; plan: CommitReplayPlan }
	| { step: "filesDone"; result: RestoreExecutionResult }
	| { step: "commitsDone"; result: ReplayResult; total: number };

/** Paste flow state (spec 3.2 / 4.3): preview -> confirm -> result. */
export function usePaste(repo: string) {
	const { t } = useTranslation();
	const [state, setState] = useState<PasteState>({ step: "idle" });
	const [busy, setBusy] = useState(false);
	// Bumped per preview so its view state (open diff) starts fresh.
	const [seq, setSeq] = useState(0);

	async function run<T>(work: () => Promise<T>): Promise<T | undefined> {
		setBusy(true);
		try {
			return await work();
		} catch (error: unknown) {
			toast.danger(errorText(t, error));
			return undefined;
		} finally {
			setBusy(false);
		}
	}

	async function preview(restoreBase?: RestoreBase) {
		const roots = repo ? [repo] : [];
		setSeq((n) => n + 1);
		const plan = await run(() =>
			invoke<ClipboardPlan>("read_clipboard_plan", {
				roots,
				restoreBase: restoreBase ?? null,
			}),
		);
		if (!plan) {
			setState({ step: "idle" });
			return;
		}
		if (plan.mode === "commits") {
			setState({ step: "commits", plan: plan.plan });
			return;
		}
		// TS asks about a folder offset first, so an adjusted plan can still
		// turn out empty; an unadjusted one with a suggestion is shown as is.
		if (!hasActionable(plan.plan) && !plan.suggestion) {
			toast.warning(
				t("noActionable", {
					count: plan.plan.skippedOperations.length,
				}),
			);
			setState({ step: "idle" });
			return;
		}
		setState({
			step: "files",
			plan: plan.plan,
			suggestion: plan.suggestion,
			checked: initialCheckedKeys(plan.plan),
			skipExisting: false,
		});
	}

	async function apply() {
		if (state.step !== "files") return;
		const selection = toSelection(
			state.plan,
			new Set(state.checked),
			state.skipExisting,
		);
		const result = await run(() =>
			invoke<RestoreExecutionResult>("apply_restore", { selection }),
		);
		if (result) setState({ step: "filesDone", result });
	}

	async function replay() {
		if (state.step !== "commits") return;
		const total = state.plan.commits.length;
		const result = await run(() => invoke<ReplayResult>("replay_commits"));
		if (result) setState({ step: "commitsDone", result, total });
	}

	return { state, setState, busy, seq, preview, apply, replay };
}

type Paste = ReturnType<typeof usePaste>;

export function PastePanel({ paste }: { paste: Paste }) {
	const { t } = useTranslation();
	const { state, busy } = paste;
	const reset = () => paste.setState({ step: "idle" });

	return (
		<div className="flex min-h-0 flex-1 flex-col gap-3">
			<div className="flex items-center gap-2">
				<Button onPress={() => void paste.preview()} isDisabled={busy}>
					{busy ? <Spinner size="sm" /> : t("previewClipboard")}
				</Button>
			</div>
			{state.step === "files" && (
				<FilesPreview
					key={paste.seq}
					paste={paste}
					state={state}
					onCancel={reset}
				/>
			)}
			{state.step === "commits" && (
				<CommitsPreview
					key={paste.seq}
					plan={state.plan}
					busy={busy}
					onReplay={paste.replay}
					onCancel={reset}
				/>
			)}
			{state.step === "filesDone" && (
				<FilesResult result={state.result} onBack={reset} />
			)}
			{state.step === "commitsDone" && (
				<CommitsResult
					result={state.result}
					total={state.total}
					onBack={reset}
				/>
			)}
		</div>
	);
}

function OpChip({ node }: { node: PlanNode }) {
	const { t } = useTranslation();
	const leaf = node.leaf;
	if (!leaf) return null;
	const chip = {
		new: { color: "success", label: t("actionNew") },
		overwrite: { color: "warning", label: t("actionOverwrite") },
		delete: { color: "danger", label: t("actionDelete") },
		skip: { color: "default", label: t("actionSkip") },
	} as const;
	const { color, label } = chip[leaf.kind];
	return (
		<Chip size="sm" variant="soft" color={color}>
			{label}
		</Chip>
	);
}

function FilesPreview({
	paste,
	state,
	onCancel,
}: {
	paste: Paste;
	state: Extract<PasteState, { step: "files" }>;
	onCancel: () => void;
}) {
	const { t } = useTranslation();
	const { plan, suggestion } = state;
	const tree = useMemo(() => buildPlanTree(plan), [plan]);
	const diff = useDiff();
	const existing = existingCount(plan);

	return (
		<div className="flex min-h-0 flex-1 flex-col gap-3">
			{suggestion && (
				<Alert status="warning">
					<Alert.Indicator />
					<Alert.Content>
						<Alert.Description className="whitespace-pre-line">
							{suggestionText(t, plan, suggestion)}
						</Alert.Description>
						<div className="mt-2 flex gap-2">
							<Button
								size="sm"
								onPress={() =>
									void paste.preview(suggestion.base)
								}
							>
								{t("adjustPaths")}
							</Button>
							<Button
								size="sm"
								variant="secondary"
								onPress={() => {
									if (hasActionable(plan)) {
										paste.setState({
											...state,
											suggestion: null,
										});
									} else {
										toast.warning(
											t("noActionable", {
												count: plan.skippedOperations
													.length,
											}),
										);
										onCancel();
									}
								}}
							>
								{t("useAsIs")}
							</Button>
						</div>
					</Alert.Content>
				</Alert>
			)}

			<p className="text-sm">{confirmationSummary(t, plan)}</p>

			<div className="min-h-0 flex-1 overflow-auto rounded border border-border p-2">
				<Tree<PlanNode>
					checkable
					selectable={false}
					defaultExpandAll
					treeData={tree}
					checkedKeys={state.checked}
					onCheck={(checked) => {
						const keys = Array.isArray(checked)
							? checked
							: checked.checked;
						paste.setState({ ...state, checked: keys.map(String) });
					}}
					titleRender={(node) => (
						<span className="inline-flex flex-wrap items-center gap-2 text-sm">
							<span className="font-mono">{node.name}</span>
							<OpChip node={node} />
							{node.leaf?.reason && (
								<span className="text-xs text-muted">
									{t(`skip${node.leaf.reason}`)}
								</span>
							)}
							{node.leaf?.kind === "overwrite" && (
								<DiffButton
									diff={diff}
									target={{
										kind: "restore",
										index: node.leaf.index,
									}}
								/>
							)}
						</span>
					)}
				/>
			</div>
			{diff.patch !== null && (
				<div className="max-h-80 shrink-0 overflow-auto">
					<DiffBody patch={diff.patch} />
				</div>
			)}

			{existing > 0 && (
				<div className="flex flex-wrap items-center gap-2 text-sm">
					<span>{t("existingFiles", { count: existing })}</span>
					<Button
						size="sm"
						variant={state.skipExisting ? "secondary" : "primary"}
						onPress={() =>
							paste.setState({ ...state, skipExisting: false })
						}
					>
						{t("overwriteAll")}
					</Button>
					<Button
						size="sm"
						variant={state.skipExisting ? "primary" : "secondary"}
						onPress={() =>
							paste.setState({ ...state, skipExisting: true })
						}
					>
						{t("skipExisting")}
					</Button>
				</div>
			)}

			<div className="flex gap-2">
				<Button
					onPress={() => void paste.apply()}
					isDisabled={
						paste.busy ||
						suggestion !== null ||
						!hasActionable(plan)
					}
				>
					{t("proceed")}
				</Button>
				<Button variant="secondary" onPress={onCancel}>
					{t("cancel")}
				</Button>
			</div>
		</div>
	);
}

function FileRow({
	commit,
	file,
	index,
	diff,
}: {
	commit: number;
	file: FilePlan;
	index: number;
	diff: ReturnType<typeof useDiff>;
}) {
	const { t } = useTranslation();
	const target = { kind: "commit", commit, file: index } as const;
	return (
		<li className="flex flex-wrap items-center gap-2 py-1 text-sm">
			<Chip size="sm" variant="soft">
				{t(`change${file.change}`)}
			</Chip>
			<span className="font-mono">
				{file.oldPath ? `${file.oldPath} → ${file.path}` : file.path}
			</span>
			{file.notCopied && (
				<Chip size="sm" variant="soft" color="warning">
					{t("replayNOT_COPIED")}: {t(`notCopied${file.notCopied}`)}
				</Chip>
			)}
			{file.skipReason && file.skipReason !== "NOT_COPIED" && (
				<Chip size="sm" variant="soft" color="danger">
					{t(`replay${file.skipReason}`)}
				</Chip>
			)}
			{file.action !== "SKIP" && (
				<DiffButton diff={diff} target={target} />
			)}
			{diff.isOpen(target) && diff.patch !== null && (
				<div className="basis-full">
					<DiffBody patch={diff.patch} />
				</div>
			)}
		</li>
	);
}

function CommitsPreview({
	plan,
	busy,
	onReplay,
	onCancel,
}: {
	plan: CommitReplayPlan;
	busy: boolean;
	onReplay: () => Promise<void>;
	onCancel: () => void;
}) {
	const { t } = useTranslation();
	const diff = useDiff();
	return (
		<div className="flex min-h-0 flex-1 flex-col gap-3">
			<p className="text-sm">
				{t("commitsToCreate", { count: plan.commits.length })}
			</p>
			<div className="min-h-0 flex-1 overflow-auto rounded border border-border">
				{plan.commits.map((c, ci) => {
					const notCopied = c.files.filter((f) => f.notCopied).length;
					return (
						<details
							key={`${ci}-${c.authorDate}`}
							className="border-b border-border px-3 py-2"
						>
							<summary className="flex cursor-pointer flex-wrap items-center gap-2 text-sm">
								<span className="text-muted">#{ci + 1}</span>
								<span className="font-medium">
									{c.message.split("\n")[0]}
								</span>
								<span className="text-xs text-muted">
									{c.authorName} &lt;{c.authorEmail}&gt; ·{" "}
									{c.authorDate}
								</span>
								{notCopied > 0 && (
									<Chip
										size="sm"
										variant="soft"
										color="warning"
									>
										{t("notCopiedCount", {
											count: notCopied,
										})}
									</Chip>
								)}
							</summary>
							<pre className="my-2 text-xs whitespace-pre-wrap text-muted">
								{c.message}
							</pre>
							<ul>
								{c.files.map((f, fi) => (
									<FileRow
										key={`${fi}-${f.path}`}
										commit={ci}
										file={f}
										index={fi}
										diff={diff}
									/>
								))}
							</ul>
						</details>
					);
				})}
			</div>
			<div className="flex gap-2">
				<Button onPress={() => void onReplay()} isDisabled={busy}>
					{t("replayCommits")}
				</Button>
				<Button variant="secondary" onPress={onCancel}>
					{t("cancel")}
				</Button>
			</div>
		</div>
	);
}

function FilesResult({
	result,
	onBack,
}: {
	result: RestoreExecutionResult;
	onBack: () => void;
}) {
	const { t } = useTranslation();
	const { text, errors } = resultSummary(t, result);
	return (
		<div className="flex flex-col gap-3">
			<Alert status={errors ? "danger" : "success"}>
				<Alert.Indicator />
				<Alert.Content>
					<Alert.Title>{text}</Alert.Title>
					{errors && <Alert.Description>{errors}</Alert.Description>}
				</Alert.Content>
			</Alert>
			{result.errors.length > 0 && (
				<ul className="font-mono text-xs">
					{result.errors.map((e) => (
						<li key={e}>{e}</li>
					))}
				</ul>
			)}
			<div>
				<Button variant="secondary" onPress={onBack}>
					{t("back")}
				</Button>
			</div>
		</div>
	);
}

function CommitsResult({
	result,
	total,
	onBack,
}: {
	result: ReplayResult;
	total: number;
	onBack: () => void;
}) {
	const { t } = useTranslation();
	const failure = result.failure;
	return (
		<div className="flex flex-col gap-3">
			<Alert status={failure ? "danger" : "success"}>
				<Alert.Indicator />
				<Alert.Content>
					<Alert.Title>
						{failure
							? t("replayCreatedOf", {
									count: result.created.length,
									total,
								})
							: t("replayCreated", {
									count: result.created.length,
								})}
					</Alert.Title>
					{failure && (
						<Alert.Description>
							{t("replayFailed", {
								index: failure.index + 1,
								message: failure.message.split("\n")[0],
								error: failure.error,
							})}
						</Alert.Description>
					)}
				</Alert.Content>
			</Alert>
			{result.created.length > 0 && (
				<ul className="font-mono text-xs">
					{result.created.map((sha) => (
						<li key={sha}>{sha}</li>
					))}
				</ul>
			)}
			<div>
				<Button variant="secondary" onPress={onBack}>
					{t("back")}
				</Button>
			</div>
		</div>
	);
}
