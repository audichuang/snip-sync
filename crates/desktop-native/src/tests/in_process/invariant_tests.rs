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
