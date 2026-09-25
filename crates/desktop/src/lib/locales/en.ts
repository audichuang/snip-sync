// English strings. Messages marked "TS" are verbatim from the VS Code
// extension (extension.ts / notify.ts) and must stay byte-identical.
export default {
	projectFiles: "Project",
	sourcePreview: "Preview",
	choosePreview: "Select a file to preview its content before copying.",
	fileContent: "Content",
	loadingPreview: "Loading preview…",
	binaryPreview:
		"Binary or non-UTF-8 content cannot be previewed or copied as text.",
	emptyDirectory: "Empty directory",
	branchesAndTags: "Branches & tags",
	allBranches: "All branches",
	localBranches: "Local branches",
	remoteBranches: "Remote branches",
	tags: "Tags",
	searchHistory: "Search commit message or SHA",
	loadedCommits: "{{count}} commits loaded",
	loadMoreHistory: "Load older commits",

	appTitle: "snip-sync",
	language: "中文",
	chooseRepo: "Choose repo / folder",
	noRepo: "No repo or folder selected",
	repoPath: "Repo or folder path",
	tabFiles: "Copy files",
	tabCommits: "Copy commits",
	tabPaste: "Paste",

	// Copy: file mode
	sourceFiles: "Files / folders",
	sourceWorking: "Working tree",
	sourceStaged: "Staged",
	sourceCommit: "Commit",
	sourceRange: "Range",
	addFiles: "Add files",
	addFolders: "Add folders",
	clearPaths: "Clear",
	noPaths: "No files selected yet.",
	commitSha: "Commit",
	rangeBase: "Base",
	rangeTip: "Tip",
	copy: "Copy",
	recentCommits: "Recent commits",
	gitChanges: "Git changes",
	selectedFiles: "{{count}} selected",
	refreshChanges: "Refresh",
	selectAll: "Select all",
	noGitChanges: "No changes in this folder.",

	// Copy: commit mode
	loadHistory: "Load history",
	timelineHint: "Click the first commit, then Shift + click the last one.",
	selectedCommits: "{{count}} commit(s) selected",
	copyCommits: "Copy commits",
	clearSelection: "Clear",
	rootRangeUnsupported:
		"A range that starts at the root commit can only be copied when it ends at HEAD.",
	noHistory: "No commits loaded.",
	discontinuous:
		"The selected commits are not contiguous: {{oldest}} is not on the first-parent chain of {{tip}}.",

	// TS notify.ts / extension.ts
	copiedFiles:
		"{{count}} file(s) copied{{sizeSuffix}}.{{limit}}{{unreadable}}",
	copiedGitFiles: "{{count}} Git file(s) copied{{skipped}}.{{limit}}",
	sizeSkippedParen: " ({{count}} skipped: size exceeded)",
	sizeSkipped: "{{count}} skipped: size exceeded",
	unreadableSkipped: "{{count}} skipped: not UTF-8 text or unreadable",
	unreadableSkippedSentence:
		" {{count}} skipped: not UTF-8 text or unreadable.",
	fileLimitReached: " File limit {{limit}} reached.",
	copyStats:
		"{{message}} {{chars}} chars · {{lines}} lines · {{words}} words · ~{{tokens}} tokens.",
	overTokens: "{{note}} Over {{threshold}} tokens.",
	// Commit mode (spec 4.2; no TS counterpart)
	copiedCommits:
		"{{commits}} commit(s) copied: {{files}} file(s), {{chars}} chars.",
	notCopiedSuffix: " {{count}} file(s) not copied.",

	// Paste
	previewClipboard: "Preview clipboard",
	noActionable: "No actionable files found. Skipped {{count}}.",
	confirmSummary:
		"Snipcode will create {{create}}, overwrite {{overwrite}}, delete {{deleted}}, and skip {{skipped}} operation(s).",
	existingFiles: "{{count}} file(s) already exist.",
	overwriteAll: "Overwrite All",
	skipExisting: "Skip Existing",
	suggestBase:
		"These paths look like they belong elsewhere in this workspace. I can {{label}} for all {{total}} file(s).",
	suggestExample: "Example: {{from}} → {{to}}",
	baseStrip: 'remove the leading "{{segment}}/"',
	baseAdd: 'place everything under "{{prefix}}/"',
	adjustPaths: "Adjust Paths",
	useAsIs: "Use As-Is",
	proceed: "Proceed",
	cancel: "Cancel",
	back: "Back",
	resultCreated: "Created {{count}}",
	resultOverwritten: "Overwritten {{count}}",
	resultSkipped: "Skipped {{count}}",
	resultDeleted: "Deleted {{count}}",
	resultNoChange: "No files changed.",
	resultErrors: "Snipcode failed {{count}} operation(s): {{errors}}",
	actionNew: "New",
	actionOverwrite: "Overwrite",
	actionDelete: "Delete",
	actionSkip: "Skip",
	showDiff: "Diff",
	hideDiff: "Hide diff",
	noDiff: "No differences.",
	commitsToCreate:
		"{{count}} commit(s) will be created on the current branch.",
	notCopiedCount: "{{count}} file(s) not copied",
	replayCommits: "Create commits",
	replayCreated: "Created {{count}} commit(s).",
	replayCreatedOf: "Created {{count}} of {{total}} commit(s).",
	replayFailed: "Commit {{index}} failed ({{message}}): {{error}}",
	changeADDED: "Added",
	changeMODIFIED: "Modified",
	changeDELETED: "Deleted",
	changeRENAMED: "Renamed",

	// Reasons (SkipReason / ReplaySkipReason / NotCopiedReason)
	skipALREADY_ABSENT: "already absent",
	skipUNRESOLVED_PATH: "invalid path or outside the workspace",
	skipAMBIGUOUS_PATH: "ambiguous path",
	skipPLACEHOLDER_BODY: "placeholder content",
	skipNON_UTF8_TARGET: "target is not UTF-8, unreadable or over 8 MiB",
	replayNOT_COPIED: "not copied",
	replayUNSAFE_PATH: "unsafe path",
	replayNON_UTF8_TARGET: "target is not UTF-8",
	notCopiedBINARY: "binary",
	notCopiedNON_UTF8: "not UTF-8",
	notCopiedNON_UTF8_PATH: "path is not UTF-8",
	notCopiedUNSUPPORTED_TYPE: "symlink or submodule",
	notCopiedUNREADABLE: "unreadable",

	// Tray (lib.rs builds the menu from these)
	trayPaste: "Paste from Clipboard",
	trayCopyLast: "Copy Last Selection",
	trayShow: "Open Main Window",
	trayQuit: "Quit snip-sync",

	// Known backend errors (commands/mod.rs), keyed by their exact text
	"err.No files selected.": "No files selected.",
	"err.No workspace folder found.": "No workspace folder found.",
	"err.No Git changes found to copy.": "No Git changes found to copy.",
	"err.Clipboard is empty or does not contain text.":
		"Clipboard is empty or does not contain text.",
	"err.No Snipcode file headers found in clipboard.":
		"No Snipcode file headers found in clipboard.",
	"err.Nothing has been copied yet.": "Nothing has been copied yet.",
};
