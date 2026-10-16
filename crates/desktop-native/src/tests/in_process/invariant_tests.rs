//! The invariant registry's own tests and the random-gesture lanes that drive
//! it (#230). A child of `tests::in_process`, so it uses that module's
//! helpers: `open_sized`, `settle`, `repo`, the remote fixtures.

use super::*;
use crate::invariants::{self, Family, Invariant, Invariant::*};
use crate::overlay::OverlayInvariant;
use gpui::TestAppContext;

/// A payload whose rows land in one folder tree under the destination: two
/// levels of folders and no restore-base header.
const TREE_PAYLOAD: &str = "// FILE: src/app/a.ts\nA\n// FILE: src/app/b.ts\nB\n// FILE: src/lib/c.ts\nC\n";

/// A payload for a folder no repo of the workspace has: the preview offers a
/// mapping row for it.
const PREFIX_PAYLOAD: &str = "// FILE: zzz/one.txt\n1\n";

/// A workspace of two repos with a few commits each.
fn two_repos() -> tempfile::TempDir {
	let ws = tempfile::tempdir().unwrap();
	let alpha = repo(ws.path(), "alpha", &[]);
	for (name, body) in [("a.txt", "one"), ("b.txt", "two")] {
		fs::write(alpha.join(name), body).unwrap();
		git(&alpha, &["add", "."]);
		git(&alpha, &["commit", "-q", "-m", &format!("add {name}")]);
	}
	repo(ws.path(), "beta", &[("loose.txt", "x")]);
	ws
}

/// Pastes `text` through the real `ctrl-v`, reading the model's own test
/// clipboard, and settles until the preview is in.
fn paste_text(
	model: &Entity<WorkbenchModel>,
	cx: &mut VisualTestContext,
	text: &str,
) {
	model.update(cx, |m, _| m.test_clipboard = Some(text.to_string()));
	cx.simulate_keystrokes("ctrl-v");
	settle_until(cx, "the paste preview", |cx| {
		model.read_with(cx, |m, _| {
			m.paste.plan().is_some() && !m.paste.is_loading()
		})
	});
}

/// Answers every mapping row of the open preview with "keep the folder",
/// clicking where the probes say the button is, until rows are planned.
fn keep_every_prefix(
	model: &Entity<WorkbenchModel>,
	cx: &mut VisualTestContext,
) {
	let prefixes: Vec<String> = model.read_with(cx, |m, _| {
		m.paste
			.plan()
			.map(|p| {
				p.prefix_choices.iter().map(|c| c.prefix.clone()).collect()
			})
			.unwrap_or_default()
	});
	for prefix in prefixes {
		click_probed(model, cx, &format!("paste-map-keep:{prefix}"));
		settle_until(cx, "the remapped preview", |cx| {
			model.read_with(cx, |m, _| !m.paste.is_loading())
		});
	}
}

/// The paste a member needs open before it can be broken, and whether its
/// destination is a folder of its own (a tree lands in it without a mapping
/// row, a mapping row asks which repo the first folder is).
fn needs_plan(id: Invariant) -> Option<(&'static str, bool)> {
	match id {
		P5 | P6 | P7 | P8 | P9 => Some((TREE_PAYLOAD, true)),
		R4 => Some((PREFIX_PAYLOAD, false)),
		_ => None,
	}
}

fn dir_key(path: &str) -> crate::paste::DirKey {
	crate::paste::DirKey {
		commit: None,
		root: PathBuf::from("/no/such/root"),
		path: path.into(),
	}
}

/// Puts the model in a state that breaks `id`. `None` for the members the
/// driver evaluates, which have no model state of their own to break.
fn breaker(id: Invariant) -> Option<fn(&mut WorkbenchModel)> {
	fn held(m: &mut WorkbenchModel) {
		m.held_selection = Some(crate::ShownSelection {
			file: None,
			source: None,
			root: None,
			commit: None,
			view: Default::default(),
			log_selected: Vec::new(),
			range_head: None,
			compare: None,
			log_selection_expanded: false,
			log_branches_all: Vec::new(),
			commit_file_only: None,
		});
	}
	fn no_selection(m: &mut WorkbenchModel) {
		m.held_selection = None;
		m.selected_commit = None;
		m.log_selected.clear();
		m.compare = None;
		m.range_head = None;
	}
	fn plan(m: &mut WorkbenchModel) -> &mut crate::paste::PastePreviewPlan {
		m.paste.plan_mut().expect("a plan is open")
	}
	Some(match id {
		L1 => |m| {
			m.held_selection = None;
			m.selected_commit = None;
			m.range_head = Some("abc".into());
		},
		L2 => |m| {
			m.held_selection = None;
			m.rev_tree = None;
			m.commit_view.commit_files.clear();
			m.commit_view.selected_commit_file = Some("no/such/file".into());
		},
		L3 => |m| {
			m.held_selection = None;
			m.rev_tree = None;
			m.commit_view.commit_files.clear();
			m.commit_view.commit_file_sel = vec!["no/such/".into()];
		},
		L4 => |m| {
			m.held_selection = None;
			m.commit_view.commit_files.clear();
			m.commit_view.commit_file_origin = vec![7];
		},
		L5 => |m| {
			m.held_selection = None;
			m.commit_view.commit_files.clear();
			m.commit_view.commit_files_truncated = true;
		},
		L6 => |m| {
			no_selection(m);
			m.preview_pending_from_log = true;
		},
		L7 => |m| {
			m.held_selection = None;
			m.commits.clear();
			m.log_deferred = false;
			m.selected_commit = Some("abc".into());
		},
		L8 => |m| {
			m.held_selection = None;
			m.selected_commit = Some("bbb".into());
			m.commit_view.commit_details =
				Some(crate::history::CommitDetails {
					sha: "aaa".into(),
					parents: Vec::new(),
					message: String::new(),
					author: String::new(),
					author_email: String::new(),
					author_date: String::new(),
					committer: String::new(),
					committer_email: String::new(),
					commit_date: String::new(),
					branches: Vec::new(),
					branches_more: false,
				});
		},
		P1 => |m| {
			m.preview_loading = true;
			m.preview_cancel = None;
		},
		P2 => |m| {
			m.preview = Some(
				crate::reader::Preview::new(
					crate::reader::PreviewSource::WorkingFile,
					Some("f.txt".into()),
					"text".into(),
					false,
					crate::syntax::Language::Plain,
				)
				.with_root(Some(PathBuf::from("/no/such/repo"))),
			);
		},
		P3 => |m| {
			held(m);
			m.preview = None;
		},
		P4 => held,
		P5 => |m| plan(m).cursor_dir = Some(dir_key("gone")),
		P6 => |m| {
			plan(m).collapsed_dirs.insert(dir_key("gone"));
		},
		P7 => |m| plan(m).adjust_paths = true,
		P8 => |m| plan(m).selected_item_idx = 999,
		P9 => |m| m.paste.unset_folder_for_tests(),
		P10 => |m| {
			m.paste.clear(None);
			m.overlays.force_open_for_tests(
				crate::overlay::OverlayKind::PasteDestField,
			);
		},
		R1 => |m| m.selected_repo_idx = Some(999),
		R2 => |m| m.change_repos.clear(),
		R3 => |m| {
			m.history_loaded = true;
			m.log_deferred = false;
			m.log_reload_for = None;
			m.log_scope_key = vec![PathBuf::from("/no/such/repo")];
		},
		R4 => |m| plan(m).forget_roots_for_tests(),
		R5 => |m| {
			m.held_selection = None;
			m.commit_view.log_commit_root =
				Some(PathBuf::from("/no/such/repo"));
		},
		R6 => |m| {
			m.change_repos[0].state = crate::ChangeRepoState::Loading;
		},
		B1 => |m| {
			m.last_branch_current
				.set(Some(Some("no-such-branch".into())));
		},
		Q1 => |m| m.preview_loading = true,
		Q2 => |m| m.history_extending = true,
		Q3 => |m| m.is_loading = true,
		Q4 => |m| m.is_copying = true,
		Q5 => |m| m.remote.busy = true,
		Q6 => |m| {
			m.paste.mark_loading_for_tests();
		},
		Q7 => |m| {
			// A job nobody finishes: its flag is leaked on purpose.
			let (_, flag) = m.lifecycle.register_read(None);
			std::mem::forget(flag);
		},
		M2 => |m| {
			let mut browse = crate::remote::Browse::for_tests(0);
			browse.listing = Some(Ok(crate::remote::FolderListing {
				path: "/".into(),
				folders: Vec::new(),
			}));
			m.remote.busy = false;
			m.remote.browse = Some(browse);
		},
		D1 => |m| {
			// Built field by field: the key scan of `i18n` reads every
			// `Msg::new(` followed by a quote and wants that key translated.
			m.status = crate::i18n::Msg {
				key: "a key the dictionary does not have",
				args: Vec::new(),
				key_args: Vec::new(),
				inner: None,
				list: None,
			};
		},
		O(o) => return Some(overlay_breaker(o)),
		M3 | O7 | O8 => return None,
	})
}

fn overlay_breaker(id: OverlayInvariant) -> fn(&mut WorkbenchModel) {
	match id {
		OverlayInvariant::O1 => {
			|m| crate::overlay::break_for_tests(m, OverlayInvariant::O1)
		}
		OverlayInvariant::O2 => {
			|m| crate::overlay::break_for_tests(m, OverlayInvariant::O2)
		}
		OverlayInvariant::O3 => {
			|m| crate::overlay::break_for_tests(m, OverlayInvariant::O3)
		}
		OverlayInvariant::O4 => {
			|m| crate::overlay::break_for_tests(m, OverlayInvariant::O4)
		}
		OverlayInvariant::O5 => {
			|m| crate::overlay::break_for_tests(m, OverlayInvariant::O5)
		}
		OverlayInvariant::O6 => {
			|m| crate::overlay::break_for_tests(m, OverlayInvariant::O6)
		}
	}
}

/// The four tests that break members: a test runs in a process of its own,
/// so the groups run side by side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Group {
	Log,
	Paste,
	Repos,
	Surface,
}

impl Group {
	const ALL: [Group; 4] =
		[Group::Log, Group::Paste, Group::Repos, Group::Surface];

	fn of(id: Invariant) -> Group {
		match id.family() {
			Family::LogSelection | Family::Preview => Group::Log,
			Family::PastePanel => Group::Paste,
			Family::RepoSet
			| Family::LogScope
			| Family::Loading
			| Family::RemoteMenu => Group::Repos,
			Family::Messages | Family::Overlays => Group::Surface,
			other => panic!("{other:?} owns no member"),
		}
	}
}

/// A checker that cannot see its own breakage checks nothing: every member
/// that the model alone decides is broken in a state made for it, on a model
/// that held it a moment before.
fn members_see_their_own_breakage(cx: &mut TestAppContext, group: Group) {
	let ws = two_repos();
	let (_dest_tmp, dest_root) = canonical_tmp();
	let dest = repo(&dest_root, "dest", &[]);
	let mut seen = 0;
	for id in invariants::all()
		.into_iter()
		.filter(|id| Group::of(*id) == group)
	{
		let Some(breaker) = breaker(id) else {
			assert!(id.driver_only(), "{id:?} has no breaker");
			continue;
		};
		assert!(!id.driver_only(), "{id:?} is driver-only and has a breaker");
		let restore_dir =
			needs_plan(id).filter(|(_, own)| *own).map(|_| dest.clone());
		let (model, cx) =
			open_sized(cx, ws.path().to_path_buf(), restore_dir, 1600., 900.);
		settle_until(cx, "the Log and the Changes", |cx| {
			model.read_with(cx, |m, _| {
				!m.commits.is_empty()
					&& !m.is_loading
					&& m.change_repos.len() == 2
					&& m.change_repos
						.iter()
						.all(|r| r.state != crate::ChangeRepoState::Loading)
			})
		});
		if let Some((payload, _)) = needs_plan(id) {
			paste_text(&model, cx, payload);
			if payload == TREE_PAYLOAD {
				keep_every_prefix(&model, cx);
			}
			model.read_with(cx, |m, _| {
				let plan = m.paste.plan().unwrap();
				assert!(
					!plan.items.is_empty() || !plan.prefix_choices.is_empty(),
					"{id:?}: the plan has nothing in it"
				);
				if matches!(id, P5 | P6 | P7 | P8) {
					assert!(
						!plan.items.is_empty(),
						"{id:?}: the plan has no rows: {} mapping row(s), {:?}",
						plan.prefix_choices.len(),
						m.status
					);
				}
			});
		}
		let (before, after) = model.update(cx, |m, cx| {
			let before = invariants::eval(id, m, cx);
			breaker(m);
			(before, invariants::eval(id, m, cx))
		});
		assert_eq!(before, None, "{id:?} does not hold on a settled model");
		assert!(after.is_some(), "{id:?} does not see its own breakage");
		seen += 1;
	}
	assert!(seen > 0, "{group:?} breaks nothing");
}

#[gpui::test]
fn every_log_and_preview_invariant_detects_its_own_breakage(
	cx: &mut TestAppContext,
) {
	members_see_their_own_breakage(cx, Group::Log);
}

#[gpui::test]
fn every_paste_invariant_detects_its_own_breakage(cx: &mut TestAppContext) {
	members_see_their_own_breakage(cx, Group::Paste);
}

#[gpui::test]
fn every_repo_and_loading_invariant_detects_its_own_breakage(
	cx: &mut TestAppContext,
) {
	members_see_their_own_breakage(cx, Group::Repos);
}

#[gpui::test]
fn every_message_and_layer_invariant_detects_its_own_breakage(
	cx: &mut TestAppContext,
) {
	members_see_their_own_breakage(cx, Group::Surface);
}

/// A member is broken by exactly one of the four tests above, or is the
/// driver's (M3, O7, O8) and has no model state to break.
#[gpui::test]
fn every_invariant_has_a_breaker_or_is_the_drivers(_cx: &mut TestAppContext) {
	let all = invariants::all();
	let driven: Vec<_> =
		all.iter().filter(|id| breaker(**id).is_none()).collect();
	assert_eq!(driven, [&M3, &O7, &O8]);
	let breakable: usize = Group::ALL
		.iter()
		.map(|g| {
			all.iter()
				.filter(|id| Group::of(**id) == *g && breaker(**id).is_some())
				.count()
		})
		.sum();
	assert_eq!(breakable + driven.len(), all.len());
	assert!(all
		.iter()
		.all(|id| id.driver_only() == breaker(*id).is_none()));
}

/// Every family that has members keeps at least one: a family whose last
/// member is deleted would leave its fields guarded by nothing.
#[gpui::test]
fn every_family_with_state_has_an_invariant(cx: &mut TestAppContext) {
	let ws = two_repos();
	let (model, cx) =
		open_sized(cx, ws.path().to_path_buf(), None, 1600., 900.);
	let fields = model.read_with(cx, |m, _| invariants::classify(m));
	let owned: std::collections::HashSet<Family> = invariants::all()
		.into_iter()
		.map(Invariant::family)
		.collect();
	for family in [
		Family::LogSelection,
		Family::Preview,
		Family::PastePanel,
		Family::RepoSet,
		Family::LogScope,
		Family::Loading,
		Family::RemoteMenu,
		Family::Messages,
		Family::Overlays,
	] {
		assert!(owned.contains(&family), "{family:?} has no member");
		assert!(
			fields.iter().any(|(_, f)| *f == family),
			"{family:?} owns no field"
		);
	}
	// A field is named once.
	let mut names: Vec<_> = fields.iter().map(|(n, _)| *n).collect();
	names.sort_unstable();
	names.dedup();
	assert_eq!(names.len(), fields.len());
}

/// What a driver reads: a member that does not hold in a drawn frame is one
/// `[APP:INVARIANT_BROKEN]` line, printed once while it stays broken and
/// again when it comes back after holding. A window without probes (a
/// normal launch) prints none, and never evaluates them.
#[gpui::test]
fn a_broken_invariant_is_logged_once_for_the_driver(cx: &mut TestAppContext) {
	let ws = two_repos();
	let (model, cx) =
		open_sized(cx, ws.path().to_path_buf(), None, 1600., 900.);
	settle_until(cx, "the Log", |cx| {
		model.read_with(cx, |m, _| !m.commits.is_empty())
	});
	let broken_lines = || -> Vec<(String, String)> {
		crate::test_log::take()
			.iter()
			.filter_map(|l| snip_native_e2e::parse_invariant_broken(l))
			// The state also breaks L5 and L6: one member is read here.
			.filter(|(id, _)| id == "L4")
			.collect()
	};
	let frame = |cx: &mut VisualTestContext| {
		model.update(cx, |_, cx| cx.notify());
		settle(cx);
	};
	let l4 = |m: &mut WorkbenchModel, on: bool| {
		m.held_selection = None;
		m.commit_view.commit_files.clear();
		m.commit_view.commit_file_origin =
			if on { vec![7] } else { Vec::new() };
	};

	// No probes: broken, drawn, and nothing printed.
	let _ = crate::test_log::take();
	model.update(cx, |m, _| l4(m, true));
	frame(cx);
	assert_eq!(broken_lines(), []);
	model.update(cx, |m, _| l4(m, false));

	// Probes on, as an E2E run has them.
	model.update(cx, |m, cx| {
		m.probes = Some(crate::ui::Probes::for_test());
		cx.notify();
	});
	frame(cx);
	let _ = crate::test_log::take();
	model.update(cx, |m, _| l4(m, true));
	frame(cx);
	let lines = broken_lines();
	assert_eq!(lines.len(), 1, "{lines:?}");
	assert_eq!(lines[0].0, "L4");
	assert!(lines[0].1.contains("origin"), "{:?}", lines[0]);
	// Still broken in the next frames: not printed again.
	frame(cx);
	frame(cx);
	assert_eq!(broken_lines(), []);
	// Repaired, then broken again: printed again.
	model.update(cx, |m, _| l4(m, false));
	frame(cx);
	assert_eq!(broken_lines(), []);
	model.update(cx, |m, _| l4(m, true));
	frame(cx);
	let lines = broken_lines();
	assert_eq!(lines.len(), 1, "{lines:?}");
	assert_eq!(lines[0].0, "L4");
}

// ───────────────────────────── the alphabet ─────────────────────────────
//
// What a lane may do is derived from the app, not listed by hand: the keys
// come from `key_bindings()`, the clicks from the controls the last frame
// drew (`Probes::drawn`). A key whose action, or a control whose id prefix,
// has no decision here fails `fuzz_alphabet_classifies_every_key_and_probe`,
// so a new control cannot be missed by the lanes without somebody deciding.

use gpui::{
	Keystroke, Modifiers, MouseButton, MouseDownEvent, MouseUpEvent,
	PlatformInput, Point,
};
use rand::rngs::StdRng;
use rand::seq::IndexedRandom;
use rand::Rng;
use snip_remote::fault::{Fault, FaultRule, Match, RequestKind};
use snip_remote::proto::ErrorCode;

/// How many gestures a lane makes per run.
const STEPS: usize = 24;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mods {
	None,
	Shift,
	Secondary,
}

impl Mods {
	fn gpui(self) -> Modifiers {
		match self {
			Mods::None => Modifiers::none(),
			Mods::Shift => Modifiers::shift(),
			Mods::Secondary => Modifiers::secondary_key(),
		}
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Button {
	Left,
	Right,
}

const ALL_MODS: &[Mods] = &[Mods::None, Mods::Shift, Mods::Secondary];
const PLAIN: &[Mods] = &[Mods::None];

/// What a control of the last frame is to a lane.
#[derive(Clone, Copy, Debug)]
enum ProbeUse {
	/// Pressed with these modifiers; also with the right button when `right`.
	Click { mods: &'static [Mods], right: bool },
	/// A text field: typed into, and submitted when `submit`.
	Field { submit: bool },
	/// Left alone: a container or a label, which is no control.
	Surface,
	/// Left alone because pressing it does what a lane must not (an OS
	/// dialog, a workspace switch, the OS clipboard). A press on a control
	/// that one of these covers lands on the hazard, so those are left
	/// alone too.
	Hazard(&'static str),
}

const ROW: ProbeUse = ProbeUse::Click {
	mods: ALL_MODS,
	right: true,
};
const ROW_L: ProbeUse = ProbeUse::Click {
	mods: ALL_MODS,
	right: false,
};
const BTN: ProbeUse = ProbeUse::Click {
	mods: PLAIN,
	right: false,
};
const FIELD: ProbeUse = ProbeUse::Field { submit: true };

/// The part of a probe id before its first `:` or `@`: what kind of control.
fn probe_prefix(id: &str) -> &str {
	id.split([':', '@']).next().unwrap_or(id)
}

/// The decision for a control of the last frame, by its id.
fn probe_use(id: &str) -> Option<ProbeUse> {
	const DIALOG: &str = "opens an OS dialog";
	const WORKSPACE: &str =
		"opens, switches or closes a workspace (out of scope: lifecycle drain)";
	const OS_CLIPBOARD: &str = "writes the OS clipboard";
	if let Some(item) = id.strip_prefix("menu-item:") {
		return Some(match item {
			"copy-path" | "copy-relative-path" | "copy-files"
			| "copy-commits" | "copy-revision" => ProbeUse::Hazard(OS_CLIPBOARD),
			"reveal" => ProbeUse::Hazard("opens the OS file manager"),
			_ => BTN,
		});
	}
	Some(match probe_prefix(id) {
		"log-subject" | "repo-row" | "editor-tab" | "change-row"
		| "change-repo" | "commit-row" => ROW,
		"commit-file" | "paste-row" | "tree-row" | "ws-tree-row" => ROW_L,
		// Log toolbar and filters.
		"btn-log-refresh"
		| "btn-head"
		| "btn-compare"
		| "btn-log-more"
		| "btn-log-hide"
		| "btn-log-regex"
		| "btn-log-case"
		| "log-filter-repo"
		| "log-filter-branch"
		| "log-filter-user"
		| "log-filter-date"
		| "log-filter-more"
		| "log-filter-user-clear"
		| "log-filter-repo-clear"
		| "log-filter-branch-clear"
		| "log-filter-date-clear"
		| "log-filter-path"
		| "log-filter-path-clear"
		| "log-repo"
		| "log-repo-check"
		| "log-branch"
		| "log-branch-group"
		| "log-user"
		| "log-date"
		| "log-date-apply"
		| "log-more"
		| "log-path-pick"
		| "log-path-expand"
		| "log-collapsed"
		| "log-branch-input-clear" => BTN,
		// The commit panel.
		"commit-dir"
		| "commit-dir-toggle"
		| "details-group-dir"
		| "details-expand-all"
		| "details-collapse-all"
		| "log-selection-toggle"
		| "selection-commit"
		| "btn-browse-tree"
		| "rev-row" => BTN,
		// The branches pane.
		"branches-toggle"
		| "branches-expand-all"
		| "branches-collapse-all"
		| "branch-group"
		| "ref"
		| "ref-more"
		| "ref-head" => BTN,
		// The left pane.
		"repo-chevron"
		| "btn-add-repo"
		| "btn-discovery-retry"
		| "btn-leave-tree"
		| "btn-changes-group-dir"
		| "change-dir"
		| "change-dir-toggle"
		| "change-group-toggle"
		| "change-repo-toggle" => BTN,
		// The rail of tool windows.
		"rail-project" | "rail-changes" | "rail-log" => BTN,
		"splitter-left"
		| "splitter-bottom"
		| "splitter-log-details"
		| "splitter-log-files" => {
			ProbeUse::Hazard("a drag handle: layout is #228 and #229")
		}
		// The editor.
		"btn-tab-close" | "btn-find-prev" | "btn-find-next"
		| "btn-find-close" | "tree-chevron" | "collapse" | "btn-find-case"
		| "btn-find-regex" | "btn-prev-diff" | "btn-next-diff"
		| "btn-expand-folds" | "btn-diff-mode" | "diff-fold" => BTN,
		// The header and the status bar.
		"btn-refresh" | "btn-paste" | "btn-repo-selector"
		| "btn-ref-selector" | "pick-repo" | "pick-ref" | "selector-group"
		| "btn-workspace-menu" | "btn-locale" | "status-vcs" => BTN,
		// The paste panel.
		"btn-apply" | "btn-cancel" | "paste-include" | "paste-overwrite"
		| "paste-dir" | "paste-dir-toggle" | "paste-dir-include"
		| "paste-commit" | "paste-commit-whole" | "paste-map-pick"
		| "paste-map-keep" | "paste-adjust" | "paste-as-is"
		| "paste-dest-root" | "paste-dest-repo" | "paste-dest-left"
		| "paste-dest-crumb" | "paste-dest-edit" => BTN,
		// The workspace menu: opening a folder is a switch of workspace.
		"remote-host" | "remote-folder" => BTN,
		"remote-recent"
		| "btn-remote-open-here"
		| "btn-remote-open"
		| "workspace-recent"
		| "btn-open-workspace"
		| "btn-workspace-open-confirm"
		| "btn-close-workspace"
		| "ws-tab-close"
		| "workspace-path-input"
		| "ws-tab"
		| "btn-remote-up"
		| "remote-up" => ProbeUse::Hazard(WORKSPACE),
		"btn-open-folder" | "paste-dest-browse" => ProbeUse::Hazard(DIALOG),
		"btn-copy-commits" | "btn-copy-view" | "btn-copy-cancel" => {
			ProbeUse::Hazard(OS_CLIPBOARD)
		}
		// Fields.
		"log-search-input"
		| "log-path-input"
		| "log-branch-input"
		| "branch-filter-input"
		| "find-input"
		| "goto-input"
		| "selector-input"
		| "paste-dest-input"
		| "input-add-repo"
		| "log-date-since"
		| "log-date-until"
		| "log-since-input"
		| "log-until-input" => FIELD,
		// Enter on this one opens the folder as the workspace.
		"remote-path-input" => ProbeUse::Field { submit: false },
		"remote-path"
		| "left-list"
		| "log-list"
		| "log-loading"
		| "log-error"
		| "log-row"
		| "log-sha"
		| "log-author"
		| "log-gutter"
		| "log-ref-label"
		| "ref-badge"
		| "reader"
		| "breadcrumb"
		| "source-badge"
		| "editor-error"
		| "reader-truncated-notice"
		| "find-bar"
		| "status-bar"
		| "lifecycle-jobs"
		| "copy-toast"
		| "speed-search"
		| "discovery-error"
		| "workspace-closed"
		| "change-list-top"
		| "change-note"
		| "change-header"
		| "root-stripe"
		| "commit-panel"
		| "commit-files-pane"
		| "commit-details"
		| "details-branches-all"
		| "paste-panel"
		| "paste-loading"
		| "paste-items"
		| "paste-mappings"
		| "paste-map"
		| "paste-map-target"
		| "paste-restore-base"
		| "paste-dir-detail"
		| "paste-commit-count"
		| "paste-commit-metadata"
		| "paste-diff-notice"
		| "paste-delete-notice"
		| "paste-skip-reason"
		| "paste-dest-bar"
		| "paste-dest-path"
		| "paste-dest-status"
		| "side-left"
		| "side-right"
		| "code-text"
		| "tooltip"
		| "workspace-menu"
		| "log-menu"
		| "selector-popover"
		| "context-menu"
		| "log-empty"
		| "tree-loading"
		| "ref-name"
		| "ref-repo"
		| "log-failed-feeds"
		| "log-filter-bar"
		| "log-chips"
		| "workspace-closed-remote-error" => ProbeUse::Surface,
		p if p.starts_with("commit-details-") || p.starts_with("text-fit") => {
			ProbeUse::Surface
		}
		_ => return None,
	})
}

/// What a key of the keymap is to a lane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeyUse {
	Fuzz,
	Skip(&'static str),
}

/// The decision for a key, by the name of its action.
fn key_use(action_name: &str) -> Option<KeyUse> {
	const OS_CLIPBOARD: &str = "writes the OS clipboard";
	const WORKSPACE: &str =
		"opens, switches or closes a workspace (out of scope: lifecycle drain)";
	let name = action_name.rsplit("::").next().unwrap_or(action_name);
	Some(match name {
		"Quit" => KeyUse::Skip("ends the process"),
		"CopySelection" | "LogCopy" | "ReaderCopy" => {
			KeyUse::Skip(OS_CLIPBOARD)
		}
		"CloseWorkspace" | "OpenWorkspace" | "PrevWorkspaceTab"
		| "NextWorkspaceTab" | "WorkspaceTab1" | "WorkspaceTab2"
		| "WorkspaceTab3" | "WorkspaceTab4" | "WorkspaceTab5"
		| "WorkspaceTab6" | "WorkspaceTab7" | "WorkspaceTab8"
		| "WorkspaceTab9" => KeyUse::Skip(WORKSPACE),
		"MenuConfirm" => KeyUse::Skip(
			"the cursor may rest on a copy item: items are clicked instead",
		),
		"PastePreview" | "ApplyPaste" | "ToggleLocale" | "Refresh"
		| "FocusNext" | "FocusPrev" | "ShowProject" | "ShowChanges"
		| "ToggleLog" | "OpenRepoSelector" | "OpenRefSelector"
		| "HideToolWindow" | "NextDiff" | "PrevDiff" | "HistoryNextPage"
		| "HistoryPrevPage" | "FindInFile" | "GotoLine" | "FindNext"
		| "FindPrev" | "NavUp" | "NavDown" | "NavToggle" | "NavCollapse"
		| "NavExpand" | "ReaderSelectAll" | "ReaderUp" | "ReaderDown"
		| "ReaderPageUp" | "ReaderPageDown" | "TreeUp" | "TreeDown"
		| "TreeExpand" | "TreeCollapse" | "TreeOpen" | "TreeToggle"
		| "ToolPageUp" | "ToolPageDown" | "MenuUp" | "MenuDown" | "LogUp"
		| "LogDown" | "LogExtendUp" | "LogExtendDown" | "LogOpen"
		| "LogSearchFocus" | "LogParent" | "LogChild" | "LogPageDown"
		| "LogPageUp" | "LogHead" => KeyUse::Fuzz,
		_ => return None,
	})
}

/// A key a lane may press: its text, and the context the binding needs.
struct KeyChoice {
	keystroke: String,
	predicate: Option<std::rc::Rc<gpui::KeyBindingContextPredicate>>,
}

/// Every key of the keymap a lane may press, by the text of its keystroke.
/// The keys of a text field are typed through `Type`, not pressed.
fn fuzz_keys() -> Vec<KeyChoice> {
	use gpui::AsKeystroke;
	let mut keys = Vec::new();
	for b in crate::key_bindings() {
		let predicate = b.predicate();
		let in_field = predicate
			.as_deref()
			.is_some_and(|p| p.to_string().contains("TextInput"));
		if in_field || b.keystrokes().len() != 1 {
			continue;
		}
		let name = b.action().name().to_string();
		if key_use(&name) != Some(KeyUse::Fuzz) {
			continue;
		}
		let keystroke = b.keystrokes()[0].as_keystroke().unparse();
		keys.push(KeyChoice {
			keystroke,
			predicate,
		});
	}
	keys
}

// ───────────────────────────── gestures ─────────────────────────────

/// A press and release with no `run_until_parked` after them: what
/// `simulate_click` does, minus the park, so work started by the press is
/// still in flight when the next gesture arrives.
fn click_np(
	cx: &mut VisualTestContext,
	at: Point<gpui::Pixels>,
	mods: Mods,
	button: Button,
) {
	let button = match button {
		Button::Left => MouseButton::Left,
		Button::Right => MouseButton::Right,
	};
	cx.update(|window, cx| {
		let _ = window.dispatch_event(
			PlatformInput::MouseDown(MouseDownEvent {
				position: at,
				modifiers: mods.gpui(),
				button,
				click_count: 1,
				first_mouse: false,
			}),
			cx,
		);
		let _ = window.dispatch_event(
			PlatformInput::MouseUp(MouseUpEvent {
				position: at,
				modifiers: mods.gpui(),
				button,
				click_count: 1,
			}),
			cx,
		);
	});
}

/// One keystroke, no park (`simulate_keystrokes` parks).
fn key_np(cx: &mut VisualTestContext, keystroke: &str) {
	let Ok(keystroke) = Keystroke::parse(keystroke) else {
		return;
	};
	cx.update(|window, cx| {
		window.dispatch_keystroke(keystroke, cx);
	});
}

// ───────────────────────────── the driver ─────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lane {
	Local,
	Paste,
	Remote,
}

impl Lane {
	fn name(self) -> &'static str {
		match self {
			Lane::Local => "local",
			Lane::Paste => "paste",
			Lane::Remote => "remote",
		}
	}

	/// The test that reproduces a run of this lane.
	fn test(self) -> &'static str {
		match self {
			Lane::Local => "fuzz_local_workbench_keeps_derived_state",
			Lane::Paste => "fuzz_paste_preview_follows_the_repo_set",
			Lane::Remote => "fuzz_remote_workspace_under_faults",
		}
	}
}

/// A change to the fixture's repos from outside the app, as a user's shell
/// makes it.
#[derive(Clone, Debug)]
enum FsEdit {
	Write {
		repo: &'static str,
		file: &'static str,
		body: String,
	},
	Commit {
		repo: &'static str,
	},
	Checkout {
		repo: &'static str,
		branch: &'static str,
	},
}

/// One gesture of a lane. A run is the list of them, so a failure can be
/// replayed by hand.
#[derive(Clone, Debug)]
enum Step {
	Click {
		id: String,
		mods: Mods,
		button: Button,
	},
	/// A keystroke of the keymap, or `escape`.
	Key(String),
	/// Clicks the field, then types `text` one key at a time.
	Type {
		id: String,
		text: String,
	},
	/// Clicks the field, then presses enter in it.
	Submit(String),
	/// Lets the executor run up to this many tasks.
	Tick(u8),
	/// Runs everything that can run, then holds the quiescent members.
	Park,
	/// Moves the test clock on, in milliseconds.
	Advance(u32),
	Fs(FsEdit),
	/// Sets what a paste reads: `PAYLOADS[n]`.
	Clipboard(usize),
	/// Arms a fault on the opened remote workspace's worker.
	Fault(FaultRule),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActKind {
	Click,
	Key,
	Type,
	Submit,
	Tick,
	Park,
	Advance,
	Fs,
	Clipboard,
	Paste,
	Fault,
}

/// What a lane does, by weight.
fn weights(lane: Lane) -> &'static [(ActKind, u32)] {
	match lane {
		Lane::Local => &[
			(ActKind::Click, 36),
			(ActKind::Key, 22),
			(ActKind::Type, 6),
			(ActKind::Submit, 3),
			(ActKind::Tick, 6),
			(ActKind::Park, 8),
			(ActKind::Advance, 6),
			(ActKind::Fs, 6),
		],
		Lane::Paste => &[
			(ActKind::Click, 30),
			(ActKind::Key, 16),
			(ActKind::Type, 6),
			(ActKind::Submit, 4),
			(ActKind::Tick, 5),
			(ActKind::Park, 8),
			(ActKind::Advance, 6),
			(ActKind::Clipboard, 10),
			(ActKind::Paste, 10),
		],
		Lane::Remote => &[
			(ActKind::Click, 36),
			(ActKind::Key, 18),
			(ActKind::Type, 4),
			(ActKind::Tick, 6),
			(ActKind::Park, 8),
			(ActKind::Advance, 6),
			(ActKind::Fault, 10),
		],
	}
}

/// What a field is typed: a plain string, an empty one, `(` (a regex that
/// does not compile), a path that is not there, Chinese text.
const WORDS: &[&str] = &[
	"",
	"abc",
	"main",
	"(",
	"no/such/path",
	"中文字",
	"x",
	"a.txt",
];

/// What a paste can read: a nested tree, a folder no repo has, a restore
/// base, a commit export, text that is no payload, and a tree of 201 files
/// (past the size that folds its first level).
fn payloads() -> Vec<String> {
	let mut big = String::new();
	for i in 0..201 {
		big.push_str(&format!("// FILE: big/d{}/f{i}.txt\n{i}\n", i % 3));
	}
	vec![
		"// FILE: x/one.txt\n1\n// FILE: x/deep/two.txt\n2\n// FILE: y/three.txt\n3\n"
			.to_string(),
		PREFIX_PAYLOAD.to_string(),
		"// clipcode-root: a\n// FILE: a/x.txt\nx\n".to_string(),
		mixed_commit_payload(),
		"just some text, no payload".to_string(),
		big,
	]
}

/// A control the last frame drew, with the gestures a lane may make on it.
struct Control {
	id: String,
	gestures: Vec<(Mods, Button)>,
}

struct Cands {
	clicks: Vec<Control>,
	fields: Vec<(String, [i32; 4], bool)>,
	keys: Vec<String>,
	unclassified: Vec<String>,
}

fn drawn(
	model: &Entity<WorkbenchModel>,
	cx: &mut VisualTestContext,
) -> Vec<(String, [i32; 4])> {
	model.read_with(cx, |m, _| {
		let probes = m.probes.as_ref().expect("a lane runs with probes on");
		probes
			.drawn()
			.into_iter()
			.filter_map(|id| probes.bounds(&id).map(|b| (id, b)))
			.collect()
	})
}

/// Where `b` (physical pixels) has its middle, in logical ones; `None` when
/// that point is off the window.
fn middle(
	b: [i32; 4],
	scale: f32,
	window: (f32, f32),
) -> Option<Point<gpui::Pixels>> {
	if b[2] <= 0 || b[3] <= 0 {
		return None;
	}
	let (x, y) = (
		(b[0] + b[2] / 2) as f32 / scale,
		(b[1] + b[3] / 2) as f32 / scale,
	);
	(x > 0. && y > 0. && x < window.0 && y < window.1)
		.then(|| gpui::point(gpui::px(x), gpui::px(y)))
}

const WINDOW: (f32, f32) = (1600., 900.);

/// What the lane can do now: the controls the last frame drew that a
/// decision allows, and the keys whose context the keyboard is in.
fn gather(
	model: &Entity<WorkbenchModel>,
	cx: &mut VisualTestContext,
	keys: &[KeyChoice],
) -> Cands {
	let scale = cx.update(|w, _| w.scale_factor());
	let stack = cx.update(|w, _| w.context_stack());
	let browsing = model.read_with(cx, |m, _| m.remote.browse.is_some());
	let mut out = Cands {
		clicks: Vec::new(),
		fields: Vec::new(),
		keys: Vec::new(),
		unclassified: Vec::new(),
	};
	let frame = drawn(model, cx);
	// Where a press would land on a hazard: a click on a control that a
	// hazard covers does what the hazard does.
	let hazards: Vec<[i32; 4]> = frame
		.iter()
		.filter(|(id, _)| matches!(probe_use(id), Some(ProbeUse::Hazard(_))))
		.map(|(_, b)| *b)
		.collect();
	let covered = |b: [i32; 4]| {
		let (x, y) = (b[0] + b[2] / 2, b[1] + b[3] / 2);
		hazards.iter().any(|h| {
			h[0] <= x && x < h[0] + h[2] && h[1] <= y && y < h[1] + h[3]
		})
	};
	for (id, b) in frame {
		match probe_use(&id) {
			None => out.unclassified.push(probe_prefix(&id).to_string()),
			Some(ProbeUse::Surface | ProbeUse::Hazard(_)) => {}
			Some(ProbeUse::Click { mods, right }) => {
				if middle(b, scale, WINDOW).is_none() || covered(b) {
					continue;
				}
				let mut gestures: Vec<_> =
					mods.iter().map(|m| (*m, Button::Left)).collect();
				if right {
					gestures.push((Mods::None, Button::Right));
				}
				out.clicks.push(Control { id, gestures });
			}
			Some(ProbeUse::Field { submit }) => {
				if middle(b, scale, WINDOW).is_some() && !covered(b) {
					out.fields.push((id, b, submit));
				}
			}
		}
	}
	out.unclassified.sort();
	out.unclassified.dedup();
	let mut texts: Vec<String> = keys
		.iter()
		.filter(|k| {
			k.predicate
				.as_ref()
				.is_none_or(|p| p.depth_of(&stack).is_some())
		})
		// Enter in the menu's path field opens the folder as the workspace.
		.filter(|k| !(browsing && k.keystroke == "enter"))
		.map(|k| k.keystroke.clone())
		.collect();
	texts.push("escape".into());
	texts.sort();
	texts.dedup();
	out.keys = texts;
	out
}

fn pick<'a, T>(rng: &mut StdRng, items: &'a [T]) -> Option<&'a T> {
	items.choose(rng)
}

/// What a lane knows about its fixture.
struct Env {
	lane: Lane,
	/// The workspace folder.
	ws: PathBuf,
	/// A repo outside the workspace that `input-add-repo` can add.
	external: Option<PathBuf>,
	/// The worker the opened remote workspace is on, and the other one.
	workers: Vec<std::sync::Arc<snip_remote::Worker>>,
	payloads: Vec<String>,
	/// The menu's path field was typed into since its browse began (M3).
	typed_in_path_field: bool,
	last_browse: Option<u64>,
}

impl Env {
	fn words_for(&self, id: &str) -> Vec<String> {
		let mut words: Vec<String> =
			WORDS.iter().map(|w| w.to_string()).collect();
		match id {
			"input-add-repo" => words
				.extend(self.external.iter().map(|p| p.display().to_string())),
			"paste-dest-input" => {
				words.push(self.ws.display().to_string());
				words.push(self.ws.join("alpha").display().to_string());
			}
			"remote-path-input" => {
				words.push(self.ws.display().to_string());
			}
			_ => {}
		}
		words
	}

	fn choose(&self, rng: &mut StdRng, c: &Cands) -> Step {
		let table = weights(self.lane);
		let total: u32 = table.iter().map(|(_, w)| w).sum();
		let mut roll = rng.random_range(0..total);
		let mut kind = table[0].0;
		for (k, w) in table {
			if roll < *w {
				kind = *k;
				break;
			}
			roll -= w;
		}
		match kind {
			ActKind::Click => {
				if let Some(control) = pick(rng, &c.clicks) {
					let (mods, button) =
						*pick(rng, &control.gestures).expect("a gesture");
					return Step::Click {
						id: control.id.clone(),
						mods,
						button,
					};
				}
			}
			ActKind::Key => {
				if let Some(k) = pick(rng, &c.keys) {
					return Step::Key(k.clone());
				}
			}
			ActKind::Type => {
				if let Some((id, ..)) = pick(rng, &c.fields) {
					let words = self.words_for(id);
					let text = pick(rng, &words).cloned().unwrap_or_default();
					return Step::Type {
						id: id.clone(),
						text,
					};
				}
			}
			ActKind::Submit => {
				let fields: Vec<_> = c.fields.iter().filter(|f| f.2).collect();
				if let Some((id, ..)) = pick(rng, &fields) {
					return Step::Submit(id.clone());
				}
			}
			ActKind::Tick => return Step::Tick(rng.random_range(1..=6)),
			ActKind::Park => return Step::Park,
			ActKind::Advance => {
				return Step::Advance(
					*pick(rng, &[10, 50, 200, 1000]).expect("a delay"),
				)
			}
			ActKind::Fs => {
				let repos = ["alpha", "beta", "gamma"];
				let repo = *pick(rng, &repos).expect("a repo");
				return Step::Fs(match rng.random_range(0..4) {
					0 => FsEdit::Write {
						repo,
						file: "base.txt",
						body: format!("changed {}", rng.random_range(0..1000)),
					},
					1 => FsEdit::Write {
						repo,
						file: "loose.txt",
						body: "untracked".into(),
					},
					2 => FsEdit::Commit { repo },
					_ => FsEdit::Checkout {
						repo,
						branch: *pick(rng, &["main", "feature"])
							.expect("a branch"),
					},
				});
			}
			ActKind::Clipboard => {
				return Step::Clipboard(
					rng.random_range(0..self.payloads.len()),
				)
			}
			ActKind::Paste => {
				return match c.clicks.iter().find(|x| x.id == "btn-paste") {
					Some(btn) if rng.random_bool(0.5) => Step::Click {
						id: btn.id.clone(),
						mods: Mods::None,
						button: Button::Left,
					},
					_ => Step::Key("ctrl-v".into()),
				}
			}
			ActKind::Fault => {
				let on = *pick(
					rng,
					&[
						RequestKind::GitView,
						RequestKind::ListDir,
						RequestKind::Read,
						RequestKind::Stat,
						RequestKind::ScanRepos,
					],
				)
				.expect("a kind");
				let fault = pick(
					rng,
					&[
						Fault::Refuse(ErrorCode::Busy),
						Fault::Refuse(ErrorCode::NotFound),
						Fault::Refuse(ErrorCode::Timeout),
						Fault::HangUp,
						Fault::TruncatedReply,
						Fault::Garbage,
						Fault::HeartbeatThenHangUp,
						Fault::RunThenHangUp,
					],
				)
				.expect("a fault")
				.clone();
				return Step::Fault(FaultRule {
					on: Match::Kind(on),
					workspace: None,
					skip: 0,
					times: *pick(rng, &[1, 2]).expect("a count"),
					fault,
				});
			}
		}
		// Nothing of that kind to do now.
		Step::Tick(1)
	}

	fn apply(
		&mut self,
		model: &Entity<WorkbenchModel>,
		cx: &mut VisualTestContext,
		step: &Step,
	) {
		let scale = cx.update(|w, _| w.scale_factor());
		let at = |cx: &mut VisualTestContext, id: &str| {
			drawn(model, cx)
				.into_iter()
				.find(|(i, _)| i == id)
				.and_then(|(_, b)| middle(b, scale, WINDOW))
		};
		match step {
			Step::Click { id, mods, button } => {
				if let Some(p) = at(cx, id) {
					click_np(cx, p, *mods, *button);
				}
			}
			Step::Key(k) => key_np(cx, k),
			Step::Type { id, text } => {
				if let Some(p) = at(cx, id) {
					click_np(cx, p, Mods::None, Button::Left);
					for c in text.chars() {
						key_np(cx, &c.to_string());
					}
					if id == "remote-path-input" {
						self.typed_in_path_field = true;
					}
				}
			}
			Step::Submit(id) => {
				if let Some(p) = at(cx, id) {
					click_np(cx, p, Mods::None, Button::Left);
					key_np(cx, "enter");
				}
			}
			Step::Tick(n) => {
				for _ in 0..*n {
					cx.executor().tick();
				}
			}
			Step::Park => {}
			Step::Advance(ms) => cx
				.executor()
				.advance_clock(Duration::from_millis(u64::from(*ms))),
			Step::Fs(edit) => self.edit(edit),
			Step::Clipboard(n) => {
				let text = self.payloads[*n].clone();
				model.update(cx, |m, _| m.test_clipboard = Some(text));
			}
			Step::Fault(rule) => {
				if let Some(w) = self.workers.first() {
					w.inject_fault_for_tests(rule.clone());
				}
			}
		}
	}

	/// A shell's change to a repo. A failure (nothing to commit, a dirty
	/// tree under a checkout) is what a shell would have seen too.
	fn edit(&self, edit: &FsEdit) {
		let run = |repo: &str, args: &[&str]| {
			let _ = Command::new("git")
				.current_dir(self.ws.join(repo))
				.args(["-c", "user.name=t", "-c", "user.email=t@t"])
				.args(args)
				.output();
		};
		match edit {
			FsEdit::Write { repo, file, body } => {
				let _ = fs::write(self.ws.join(repo).join(file), body);
			}
			FsEdit::Commit { repo } => {
				run(repo, &["add", "-A"]);
				run(repo, &["commit", "-q", "-m", "fuzz"]);
			}
			FsEdit::Checkout { repo, branch } => {
				run(repo, &["checkout", "-q", branch]);
			}
		}
	}
}

/// The first of Q1 to Q7 that does not hold: what a run waits for.
fn busy(m: &WorkbenchModel, cx: &gpui::App) -> Option<String> {
	[Q1, Q2, Q3, Q4, Q5, Q6, Q7].into_iter().find_map(|id| {
		invariants::eval(id, m, cx).map(|d| format!("{}: {d}", id.label()))
	})
}

/// Runs until nothing is loading, at most 200 rounds, each moving the test
/// clock a second on (the scan retries after 1, 2 and 4 s, a restore up to
/// a minute). A run that is still busy then has found a violation: what
/// kept it busy is the answer, not a timeout.
fn settle_all(
	lane: Lane,
	model: &Entity<WorkbenchModel>,
	cx: &mut VisualTestContext,
) -> Option<String> {
	let mut last = None;
	for _ in 0..200 {
		cx.run_until_parked();
		last = model.read_with(cx, |m, cx| busy(m, cx));
		if last.is_none() {
			return None;
		}
		cx.executor().advance_clock(Duration::from_secs(1));
		if lane == Lane::Remote {
			// A worker is a thread of its own: let it answer.
			std::thread::sleep(snip_native_e2e::scaled(Duration::from_millis(
				10,
			)));
		}
	}
	last.map(|why| format!("still busy after 200 rounds of settling: {why}"))
}

/// The members that hold at any time, and the driver's M3.
fn broken_now(
	env: &mut Env,
	model: &Entity<WorkbenchModel>,
	cx: &mut VisualTestContext,
) -> Vec<String> {
	let text = cx.update(|_, cx| {
		model.read(cx).remote_path_input.read(cx).text().to_string()
	});
	model.read_with(cx, |m, cx| {
		let mut broken: Vec<String> =
			invariants::check(m, cx, invariants::When::Always)
				.into_iter()
				.map(|v| format!("{}: {}", v.id.label(), v.detail))
				.collect();
		// M3: the menu's path field shows what this code wrote into it, until
		// the user types; a new browse starts the count over.
		let seq = m.remote.browse.as_ref().map(|b| b.seq());
		if seq != env.last_browse {
			env.last_browse = seq;
			env.typed_in_path_field = false;
		}
		if let Some(browse) = m.remote.browse.as_ref() {
			if !env.typed_in_path_field && text != browse.prefill() {
				broken.push(format!(
					"M3: the path field shows {text:?}, the browse wrote {:?}",
					browse.prefill()
				));
			}
		}
		broken
	})
}

/// The members that hold once nothing is loading, and what only the driver
/// can see: which panels are drawn, and where the keyboard is.
fn broken_quiet(
	env: &mut Env,
	model: &Entity<WorkbenchModel>,
	cx: &mut VisualTestContext,
) -> Vec<String> {
	if let Some(why) = settle_all(env.lane, model, cx) {
		return vec![why];
	}
	let mut broken = broken_now(env, model, cx);
	broken.extend(model.read_with(cx, |m, cx| {
		invariants::check(m, cx, invariants::When::Quiescent)
			.into_iter()
			.filter(|v| v.id.when() == invariants::When::Quiescent)
			.map(|v| format!("{}: {}", v.id.label(), v.detail))
			.collect::<Vec<_>>()
	}));
	for (id, check) in [
		(
			"O7",
			overlays::assert_overlay_panels_match
				as fn(&mut VisualTestContext, &Entity<WorkbenchModel>),
		),
		("O8", overlays::assert_focus_not_in_closed_layer),
	] {
		let result =
			std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
				check(cx, model)
			}));
		if let Err(payload) = result {
			let why = payload
				.downcast_ref::<String>()
				.cloned()
				.or_else(|| {
					payload.downcast_ref::<&str>().map(|s| s.to_string())
				})
				.unwrap_or_default();
			broken.push(format!("{id}: {why}"));
		}
	}
	broken
}

fn fail(env: &Env, at: usize, steps: &[Step], broken: &[String]) -> ! {
	panic!(
		"the {lane} lane broke an invariant at step {at} of {total}\n\
		 violations: {broken:#?}\n\
		 steps: {steps:#?}\n\
		 reproduce: SEED=<the `failing seed` printed above> ITERATIONS=1 \
		 cargo nextest run -p snip-desktop-native {test}\n\
		 (the steps are the replay; a remote worker's reply times are not in the seed)",
		lane = env.lane.name(),
		total = STEPS,
		test = env.lane.test(),
	)
}

/// Makes `STEPS` gestures, holding the invariants after each and, at every
/// `Park` and at the end, once nothing is loading.
fn drive(
	env: &mut Env,
	model: &Entity<WorkbenchModel>,
	cx: &mut VisualTestContext,
	mut rng: StdRng,
) {
	let keys = fuzz_keys();
	let mut steps: Vec<Step> = Vec::new();
	// A control with no decision is left alone, and the run says so at its
	// end: one run lists every prefix it met.
	let mut unclassified: Vec<String> = Vec::new();
	for at in 0..STEPS {
		let cands = gather(model, cx, &keys);
		unclassified.extend(cands.unclassified.iter().cloned());
		let step = env.choose(&mut rng, &cands);
		steps.push(step.clone());
		env.apply(model, cx, &step);
		let broken = if matches!(step, Step::Park) {
			broken_quiet(env, model, cx)
		} else {
			broken_now(env, model, cx)
		};
		if !broken.is_empty() {
			fail(env, at, &steps, &broken);
		}
	}
	let broken = broken_quiet(env, model, cx);
	if !broken.is_empty() {
		fail(env, STEPS, &steps, &broken);
	}
	unclassified.sort();
	unclassified.dedup();
	assert!(
		unclassified.is_empty(),
		"controls with no decision in `probe_use`: {unclassified:?}"
	);
}

// ───────────────────────────── the lanes ─────────────────────────────

/// `alpha` (main: four commits, one of them in a subfolder, one touching two
/// files; a branch `feature`), `beta` (on `feature`, three commits) and
/// `gamma` (one commit and an untracked file). Names have no space and no `:`.
fn lane_repos() -> tempfile::TempDir {
	let ws = tempfile::tempdir().unwrap();
	let alpha = repo(ws.path(), "alpha", &[]);
	let commit = |dir: &Path, files: &[(&str, &str)], msg: &str| {
		for (name, body) in files {
			if let Some(parent) = Path::new(name).parent() {
				fs::create_dir_all(dir.join(parent)).unwrap();
			}
			fs::write(dir.join(name), body).unwrap();
		}
		git(dir, &["add", "."]);
		git(dir, &["commit", "-q", "-m", msg]);
	};
	commit(&alpha, &[("a.txt", "one")], "add a");
	git(&alpha, &["branch", "feature"]);
	commit(
		&alpha,
		&[("a.txt", "two"), ("b.txt", "b")],
		"change a, add b",
	);
	commit(&alpha, &[("src/a/b.txt", "deep")], "add src/a/b.txt");
	let beta = repo(ws.path(), "beta", &[]);
	git(&beta, &["checkout", "-q", "-b", "feature"]);
	commit(&beta, &[("f1.txt", "1")], "f1");
	commit(&beta, &[("f2.txt", "2")], "f2");
	repo(ws.path(), "gamma", &[("loose.txt", "untracked")]);
	ws
}

/// A window with probes on and a clipboard the paste key can read.
fn lane_window<'a>(
	cx: &'a mut TestAppContext,
	ws: &Path,
	clipboard: &str,
) -> (Entity<WorkbenchModel>, &'a mut VisualTestContext) {
	// The hosts of `~/.ssh/config` are the machine's, and a click on one
	// would start a real ssh: a local lane has none.
	let (model, cx) = crate::remote::with_test_hosts(Vec::new(), || {
		open_sized(cx, ws.to_path_buf(), None, WINDOW.0, WINDOW.1)
	});
	model.update(cx, |m, cx| {
		m.probes = Some(crate::ui::Probes::for_test());
		m.test_clipboard = Some(clipboard.to_string());
		cx.notify();
	});
	settle(cx);
	(model, cx)
}

fn env(lane: Lane, ws: &Path) -> Env {
	Env {
		lane,
		ws: ws.to_path_buf(),
		external: None,
		workers: Vec::new(),
		payloads: payloads(),
		typed_in_path_field: false,
		last_browse: None,
	}
}

/// Random gestures over the Log, Changes, Project and the layers, with
/// edits made behind the app's back.
#[gpui::test(iterations = 4)]
fn fuzz_local_workbench_keeps_derived_state(
	cx: &mut TestAppContext,
	rng: StdRng,
) {
	let ws = lane_repos();
	let (model, cx) = lane_window(cx, ws.path(), "no payload here");
	let mut env = env(Lane::Local, ws.path());
	drive(&mut env, &model, cx, rng);
}
