//! Git Changes tool window: the row model (groups, repos, the
//! directory tree) and the row renderers.

use super::*;
use crate::dirtree::{DirTree, TreePath, TreeSink};
use crate::ChangesEmpty;

/// One row of the Git Changes tool window (section header or file change).
#[derive(Clone, Debug)]
pub enum ChangeItemRow {
	/// Group node (Staged / Unstaged / Conflicts); with more than one repo
	/// it spans the whole workspace.
	Header {
		label: &'static str,
		count: usize,
		group_id: &'static str,
	},
	/// A repo's node under a group, shown when the workspace has more than
	/// one repo.
	Repo {
		slot: usize,
		group_id: &'static str,
		count: usize,
	},
	/// A directory of one repo's changes in one group ("Group By >
	/// Directory"). `path` is the full path from the repo root; `name` is
	/// what the row shows, a chain of single-child directories joined.
	Dir {
		slot: usize,
		group_id: &'static str,
		path: String,
		name: String,
		count: usize,
		depth: usize,
	},
	/// A repo's read error or truncation notice.
	Note {
		slot: usize,
	},
	/// The node over the read errors of [`UNREADABLE_MIN`] or more repos
	/// that have no rows; collapsed until opened.
	Unreadable {
		count: usize,
	},
	File {
		file_idx: usize,
		depth: usize,
	},
}

/// A Changes note row's text, colour and whether it sits at the top level.
pub(crate) struct ChangeNote {
	pub text: String,
	pub color: u32,
	pub top: bool,
}

impl ChangeItemRow {
	/// Tree level: 0 for a group, `None` for a note (never a parent).
	pub(crate) fn depth(&self) -> Option<usize> {
		match self {
			ChangeItemRow::Header { .. } => Some(0),
			ChangeItemRow::Repo { .. } => Some(1),
			ChangeItemRow::Dir { depth, .. }
			| ChangeItemRow::File { depth, .. } => Some(*depth),
			ChangeItemRow::Note { .. } => None,
			ChangeItemRow::Unreadable { .. } => Some(0),
		}
	}
}

/// Group id [`change_rows`] asks `group_collapsed` about for the
/// unreadable-repos node.
pub(crate) const UNREADABLE: &str = "unreadable";

/// From this many failed repos with no rows, their errors fold into one
/// node at the bottom; fewer stay at the top, one row each.
pub(crate) const UNREADABLE_MIN: usize = 2;

/// Repos of a multi-repo workspace whose read failed with nothing listed.
pub(crate) fn unreadable_slots(
	slots: &[crate::ChangeRepo],
	files: &[crate::FileChangeItem],
) -> Vec<usize> {
	if slots.len() <= 1 {
		return Vec::new();
	}
	(0..slots.len())
		.filter(|&s| {
			matches!(slots[s].state, crate::ChangeRepoState::Failed(_))
				&& crate::slot_range(files, s).is_empty()
		})
		.collect()
}

/// Left padding of a Changes row at tree level `depth`: each level moves
/// one chevron plus gap, so every level's chevrons share one column.
pub(super) fn change_pad(depth: usize) -> f32 {
	4. + 18. * depth as f32
}

/// A tree node's chevron cell (Changes tool window and Git Log details).
pub(super) fn tree_chevron(id: String, collapsed: bool) -> Stateful<Div> {
	div()
		.id(SharedString::from(id))
		.relative()
		.flex_shrink_0()
		.size(px(16.))
		.flex()
		.items_center()
		.justify_center()
		.child(icon(
			if collapsed {
				Icon::ChevronRight
			} else {
				Icon::ChevronDown
			},
			10.,
		))
}

/// The muted count after a tree node's name.
pub(super) fn tree_count(count: usize) -> Div {
	div()
		.flex_shrink_0()
		.ml(px(8.))
		.text_size(px(SMALL_TEXT))
		.text_color(rgb(pal().text_muted))
		.child(count.to_string())
}

/// Fields of a [`ChangeItemRow::Dir`] handed to its renderer.
pub(super) struct ChangeDirRow {
	pub(super) slot: usize,
	pub(super) group_id: &'static str,
	pub(super) path: String,
	pub(super) name: String,
	pub(super) count: usize,
	pub(super) depth: usize,
}

/// How the Changes rows under a repo are laid out: a directory tree whose
/// `expanded(slot, group, dir)` nodes are open, or a flat file list.
pub(crate) struct ChangeLayout<F: Fn(usize, &str, &str) -> bool> {
	pub by_dir: bool,
	pub expanded: F,
}

/// Changes tool window rows. One repo shows its groups at the top level
/// with the files under them. Several repos invert that: each non-empty
/// group is a workspace-wide node over one row per repo with files in it
/// (in repo name order), and the files sit under the repo rows. Clean
/// repos are not listed; a failed read with no rows gets a top-level note,
/// or with [`UNREADABLE_MIN`] such repos, a note under one node at the end.
/// Under each (group, repo) the files are a directory tree or flat, per
/// `layout`.
pub(crate) fn change_rows<F: Fn(usize, &str, &str) -> bool>(
	slots: &[crate::ChangeRepo],
	files: &[crate::FileChangeItem],
	group_collapsed: impl Fn(&str) -> bool,
	repo_collapsed: impl Fn(usize, &str) -> bool,
	repo_query: &str,
	layout: &ChangeLayout<F>,
) -> Vec<ChangeItemRow> {
	let query = repo_query.to_lowercase();
	let note = |slot: usize| {
		matches!(slots[slot].state, crate::ChangeRepoState::Failed(_))
			|| slots[slot].truncated(crate::slot_range(files, slot).len())
	};
	let members = |slot: usize, group: &str| -> Vec<usize> {
		crate::slot_range(files, slot)
			.filter(|&i| crate::menu::change_group(&files[i]) == Some(group))
			.collect()
	};
	let mut rows = Vec::new();
	if slots.len() <= 1 {
		for slot in 0..slots.len() {
			if note(slot) {
				rows.push(ChangeItemRow::Note { slot });
			}
			for (group_id, label) in crate::menu::CHANGE_GROUPS {
				let members = members(slot, group_id);
				if members.is_empty() {
					continue;
				}
				rows.push(ChangeItemRow::Header {
					label,
					count: members.len(),
					group_id,
				});
				if !group_collapsed(group_id) {
					push_change_files(
						&mut rows, files, members, slot, group_id, 1, layout,
					);
				}
			}
		}
		return rows;
	}
	// Speed search over several repos filters the repo rows by name.
	let shown: Vec<usize> = (0..slots.len())
		.filter(|&s| slots[s].name.to_lowercase().contains(&query))
		.collect();
	let unreadable: Vec<usize> = shown
		.iter()
		.copied()
		.filter(|&slot| crate::slot_range(files, slot).is_empty() && note(slot))
		.collect();
	let fold = unreadable.len() >= UNREADABLE_MIN;
	if !fold {
		rows.extend(
			unreadable.iter().map(|&slot| ChangeItemRow::Note { slot }),
		);
	}
	for (group_id, label) in crate::menu::WORKSPACE_GROUPS {
		let total = files
			.iter()
			.filter(|f| crate::menu::change_group(f) == Some(group_id))
			.count();
		let repos: Vec<(usize, Vec<usize>)> = shown
			.iter()
			.map(|&slot| (slot, members(slot, group_id)))
			.filter(|(_, m)| !m.is_empty())
			.collect();
		if repos.is_empty() {
			continue;
		}
		rows.push(ChangeItemRow::Header {
			label,
			count: total,
			group_id,
		});
		if group_collapsed(group_id) {
			continue;
		}
		for (slot, members) in repos {
			rows.push(ChangeItemRow::Repo {
				slot,
				group_id,
				count: members.len(),
			});
			if repo_collapsed(slot, group_id) {
				continue;
			}
			if note(slot) {
				rows.push(ChangeItemRow::Note { slot });
			}
			push_change_files(
				&mut rows, files, members, slot, group_id, 2, layout,
			);
		}
	}
	if fold {
		rows.push(ChangeItemRow::Unreadable {
			count: unreadable.len(),
		});
		if !group_collapsed(UNREADABLE) {
			rows.extend(
				unreadable
					.into_iter()
					.map(|slot| ChangeItemRow::Note { slot }),
			);
		}
	}
	rows
}

/// Rows of one repo's changes in one group, at tree level `depth`.
pub(super) fn push_change_files<
	T: TreePath,
	F: Fn(usize, &str, &str) -> bool,
>(
	rows: &mut Vec<ChangeItemRow>,
	files: &[T],
	mut members: Vec<usize>,
	slot: usize,
	group_id: &'static str,
	depth: usize,
	layout: &ChangeLayout<F>,
) {
	if !layout.by_dir {
		rows.extend(
			members
				.into_iter()
				.map(|file_idx| ChangeItemRow::File { file_idx, depth }),
		);
		return;
	}
	let tree = DirTree { files };
	let mut sink = ChangeSink {
		rows,
		slot,
		group_id,
		expanded: &layout.expanded,
	};
	tree.push(&mut sink, &mut members, 0, depth);
}

/// Turns the shared directory tree's rows into Changes rows.
struct ChangeSink<'r, 'e, F> {
	rows: &'r mut Vec<ChangeItemRow>,
	slot: usize,
	group_id: &'static str,
	expanded: &'e F,
}

impl<F: Fn(usize, &str, &str) -> bool> TreeSink for ChangeSink<'_, '_, F> {
	fn dir(
		&mut self,
		path: &str,
		name: &str,
		members: &[usize],
		depth: usize,
	) -> bool {
		self.rows.push(ChangeItemRow::Dir {
			slot: self.slot,
			group_id: self.group_id,
			path: path.to_string(),
			name: name.to_string(),
			count: members.len(),
			depth,
		});
		(self.expanded)(self.slot, self.group_id, path)
	}

	fn file(&mut self, file_idx: usize, depth: usize) {
		self.rows.push(ChangeItemRow::File { file_idx, depth });
	}
}

impl crate::dirtree::TreePath for crate::FileChangeItem {
	fn tree_path(&self) -> &str {
		&self.path
	}
}

/// A file of a commit (or of a log multi-selection).
impl crate::dirtree::TreePath for (String, Option<ChangeType>) {
	fn tree_path(&self) -> &str {
		&self.0
	}
}

/// Group id of the Git Log's changed-files rows (one tree, no groups).
pub(crate) const COMMIT_GROUP: &str = "commit";

/// Rows of the Git Log's changed-files pane: the Changes tool window's
/// directory tree (dirs first, single-child chains in one row) with no
/// groups or repos above it, or its flat list. Directories named in
/// `collapsed` are closed; every other one is open.
pub(crate) fn commit_file_rows(
	files: &[(String, Option<ChangeType>)],
	by_dir: bool,
	collapsed: &[String],
) -> Vec<ChangeItemRow> {
	let layout = ChangeLayout {
		by_dir,
		expanded: |_: usize, _: &str, dir: &str| {
			!collapsed.iter().any(|c| c == dir)
		},
	};
	let mut rows = Vec::with_capacity(files.len());
	let mut members: Vec<usize> = (0..files.len()).collect();
	if !by_dir {
		members.sort_by(|&x, &y| natural_cmp(&files[x].0, &files[y].0));
	}
	push_change_files(&mut rows, files, members, 0, COMMIT_GROUP, 0, &layout);
	rows
}

/// Every directory the changed-files tree of `files` has (Collapse All).
pub(crate) fn commit_file_dirs(
	files: &[(String, Option<ChangeType>)],
) -> Vec<String> {
	commit_file_rows(files, true, &[])
		.into_iter()
		.filter_map(|r| match r {
			ChangeItemRow::Dir { path, .. } => Some(path),
			_ => None,
		})
		.collect()
}

/// Case-insensitive natural order: digit runs compare by value (g2 < g10); ties fall back to byte order.
pub(crate) fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
	use std::cmp::Ordering;
	fn digits(it: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
		let mut out = String::new();
		while let Some(c) = it.peek().copied().filter(char::is_ascii_digit) {
			out.push(c);
			it.next();
		}
		out
	}
	let (mut ai, mut bi) = (a.chars().peekable(), b.chars().peekable());
	while let (Some(x), Some(y)) = (ai.peek().copied(), bi.peek().copied()) {
		let order = if x.is_ascii_digit() && y.is_ascii_digit() {
			let (da, db) = (digits(&mut ai), digits(&mut bi));
			let (ta, tb) =
				(da.trim_start_matches('0'), db.trim_start_matches('0'));
			(ta.len(), ta).cmp(&(tb.len(), tb))
		} else {
			ai.next();
			bi.next();
			x.to_lowercase().cmp(y.to_lowercase())
		};
		if order != Ordering::Equal {
			return order;
		}
	}
	ai.count().cmp(&bi.count()).then_with(|| a.cmp(b))
}

impl WorkbenchModel {
	/// Changes group node, like IntelliJ's commit tool window: chevron,
	/// name and count. Its menu copies the whole group.
	pub(super) fn change_header_row(
		&self,
		label_key: &'static str,
		count: usize,
		group_id: &'static str,
		row_idx: usize,
		cx: &mut Context<Self>,
	) -> AnyElement {
		let loc = self.locale;
		let log = &self.probes;
		let collapsed = self.group_collapsed(group_id);
		let cursor = row_idx == self.selected_list_row;
		// One click toggles a workspace group; a single repo's group keeps
		// IntelliJ's double click.
		let clicks = if self.change_repos.len() > 1 { 1 } else { 2 };
		let id = format!("change-header:{group_id}");
		let toggle_id = format!("change-group-toggle:{group_id}");
		div()
			.id(SharedString::from(id.clone()))
			.relative()
			.flex()
			.flex_row()
			.items_center()
			.w_full()
			.h(px(ROW_H))
			.pl(px(4.))
			.pr(px(6.))
			.gap(px(2.))
			.rounded(px(4.))
			.cursor_pointer()
			.when(cursor, |d| d.bg(rgb(self.left_selection_bg())))
			.when(!cursor, |d| d.hover(|s| s.bg(rgb(pal().hover_bg))))
			.on_click(cx.listener(move |this, ev: &gpui::ClickEvent, _, cx| {
				this.selected_list_row = row_idx;
				if ev.click_count() == clicks {
					this.toggle_group_collapsed(group_id, cx);
				}
				cx.notify();
			}))
			.on_mouse_down(
				MouseButton::Right,
				cx.listener(move |this, ev: &MouseDownEvent, w, cx| {
					this.selected_list_row = row_idx;
					let items = this.change_group_menu(group_id);
					this.open_left_menu(items, ev, w, cx);
				}),
			)
			.child(
				div()
					.id(SharedString::from(toggle_id.clone()))
					.relative()
					.flex_shrink_0()
					.size(px(16.))
					.flex()
					.items_center()
					.justify_center()
					.on_click(cx.listener(move |this, _, _, cx| {
						cx.stop_propagation();
						this.selected_list_row = row_idx;
						this.toggle_group_collapsed(group_id, cx);
					}))
					.child(icon(
						if collapsed {
							Icon::ChevronRight
						} else {
							Icon::ChevronDown
						},
						10.,
					))
					.children(probe(log, toggle_id)),
			)
			.child(
				clip_text(t(label_key, loc))
					.ml(px(4.))
					.font_weight(FontWeight::SEMIBOLD),
			)
			.child(
				div()
					.flex_shrink_0()
					.ml(px(6.))
					.text_size(px(SMALL_TEXT))
					.text_color(rgb(pal().text_muted))
					.child(count.to_string()),
			)
			.children(probe(log, id))
			// Drawn whole at the list's top only when the list is scrolled
			// to its start: a wheel sweep reads it as the top.
			.children(
				(row_idx == 0)
					.then(|| probe(log, "change-list-top"))
					.flatten(),
			)
			.into_any_element()
	}

	/// Probe ids of a Changes slot: whether the unqualified legacy ids
	/// (`change-row:<source>:<path>`) are emitted, which name only the open
	/// repo's rows, and the repo name that qualifies every row's id
	/// (`change-row@<repo>:<source>:<path>`).
	pub(super) fn change_probe_scope(&self, slot: usize) -> (bool, String) {
		let legacy = self.selected_change_slot() == Some(slot);
		let name = self
			.change_repos
			.get(slot)
			.map(|s| s.name.clone())
			.unwrap_or_default();
		(legacy, name)
	}

	/// Repo row under a workspace group: chevron, a square in the repo's Git Log
	/// root-stripe color, the name, its count and the branch as a pill.
	pub(super) fn change_repo_row(
		&self,
		slot: usize,
		group_id: &'static str,
		count: usize,
		row_idx: usize,
		cx: &mut Context<Self>,
	) -> AnyElement {
		let log = &self.probes;
		let Some(repo) = self.change_repos.get(slot) else {
			return div().into_any_element();
		};
		let collapsed = self.repo_changes_collapsed(slot, group_id);
		let cursor = row_idx == self.selected_list_row;
		let name = repo.name.clone();
		let failed = matches!(repo.state, crate::ChangeRepoState::Failed(_));
		let color = graph_view::palette_rgb(self.log_repo_color(&repo.root));
		// Detached HEAD shows its short sha.
		let branch = self
			.repos
			.iter()
			.find(|r| r.root == repo.root)
			.and_then(|r| r.summary.as_ref().ok())
			.and_then(|s| {
				s.branch
					.clone()
					.or_else(|| s.head.as_ref().map(|h| short(h).to_string()))
			})
			.unwrap_or_default();
		let id = format!("change-repo:{group_id}:{name}");
		let toggle_id = format!("change-repo-toggle:{group_id}:{name}");
		let tooltip = self.change_repo_tooltip(&repo.root);
		div()
			.id(SharedString::from(id.clone()))
			.relative()
			.flex()
			.flex_row()
			.items_center()
			.w_full()
			.h(px(ROW_H))
			.pl(px(change_pad(1)))
			.pr(px(6.))
			.gap(px(2.))
			.rounded(px(4.))
			.cursor_pointer()
			.when(cursor, |d| d.bg(rgb(self.left_selection_bg())))
			.when(!cursor, |d| d.hover(|s| s.bg(rgb(pal().hover_bg))))
			.tip(tooltip)
			.on_click(cx.listener(move |this, _, _, cx| {
				this.selected_list_row = row_idx;
				this.toggle_repo_collapsed(slot, group_id, cx);
			}))
			.on_mouse_down(
				MouseButton::Right,
				cx.listener(move |this, ev: &MouseDownEvent, w, cx| {
					this.selected_list_row = row_idx;
					let items = this.change_repo_menu(slot, group_id);
					this.open_left_menu(items, ev, w, cx);
				}),
			)
			.child(
				tree_chevron(toggle_id.clone(), collapsed)
					.children(probe(log, toggle_id)),
			)
			.child(if failed {
				div()
					.flex_shrink_0()
					.ml(px(4.))
					.child(icon(Icon::Warning, 12.))
			} else {
				div()
					.flex_shrink_0()
					.ml(px(5.))
					.mr(px(1.))
					.size(px(10.))
					.rounded(px(3.))
					.bg(color)
			})
			.child(
				self.speed_label(name)
					.ml(px(6.))
					.flex_shrink()
					.text_color(rgb(pal().text)),
			)
			.child(tree_count(count))
			.when(!branch.is_empty(), |d| {
				d.child(
					clip_text(branch)
						.flex_shrink_0()
						.max_w(gpui::relative(0.45))
						.ml(px(8.))
						.px(px(6.))
						.rounded(px(4.))
						.bg(rgb(pal().ref_bg))
						.text_size(px(SMALL_TEXT))
						.text_color(rgb(pal().text)),
				)
			})
			.children(probe(log, id))
			.into_any_element()
	}

	/// The collapsed node over the read errors of many repos: chevron,
	/// warning, label and how many repos could not be read.
	pub(super) fn change_unreadable_row(
		&self,
		count: usize,
		row_idx: usize,
		cx: &mut Context<Self>,
	) -> AnyElement {
		let collapsed = self.group_collapsed(UNREADABLE);
		let cursor = row_idx == self.selected_list_row;
		let id = "change-unreadable";
		let toggle_id = "change-unreadable-toggle";
		div()
			.id(id)
			.relative()
			.flex()
			.flex_row()
			.items_center()
			.w_full()
			.h(px(ROW_H))
			.pl(px(4.))
			.pr(px(6.))
			.gap(px(2.))
			.rounded(px(4.))
			.cursor_pointer()
			.when(cursor, |d| d.bg(rgb(self.left_selection_bg())))
			.when(!cursor, |d| d.hover(|s| s.bg(rgb(pal().hover_bg))))
			.on_click(cx.listener(move |this, _, _, cx| {
				this.selected_list_row = row_idx;
				this.toggle_group_collapsed(UNREADABLE, cx);
			}))
			.child(
				tree_chevron(toggle_id.into(), collapsed)
					.children(probe(&self.probes, toggle_id)),
			)
			.child(
				div()
					.flex_shrink_0()
					.ml(px(4.))
					.child(icon(Icon::Warning, 12.)),
			)
			.child(
				div()
					.ml(px(6.))
					.flex_shrink()
					.text_color(rgb(pal().error))
					.child(t("changes_unreadable", self.locale)),
			)
			.child(tree_count(count))
			.children(probe(&self.probes, id))
			.into_any_element()
	}

	/// The text and colour of a Changes note row, in `loc`. Both the row and
	/// its live tooltip call this, so they cannot disagree.
	pub(crate) fn change_note_text(
		&self,
		slot: usize,
		loc: crate::i18n::Locale,
	) -> Option<ChangeNote> {
		let repo = self.change_repos.get(slot)?;
		let kept = crate::slot_range(&self.files, slot).len();
		let (text, color) = match &repo.state {
			crate::ChangeRepoState::Failed(err) => (
				crate::i18n::Msg::with_msg_arg(
					"changes_repo_error",
					[String::new()],
					0,
					err.clone(),
				)
				.render(loc),
				pal().error,
			),
			_ => (
				tf("changes_truncated", loc, &[&kept, &repo.total]),
				pal().text_muted,
			),
		};
		// A multi-repo failure with no rows sits at the top level, under no
		// repo row, so it names its repo; with enough of them, under the
		// unreadable-repos node.
		let top = self.change_repos.len() > 1 && kept == 0;
		let text = if top {
			format!("{}: {text}", repo.name)
		} else {
			text
		};
		Some(ChangeNote { text, color, top })
	}

	/// The same note found by its repo's root, for a tooltip that outlives
	/// the row's position: a rescan can move or drop the slot (#208).
	pub(crate) fn change_note_text_for(
		&self,
		root: &std::path::Path,
		loc: crate::i18n::Locale,
	) -> Option<ChangeNote> {
		let slot = self.change_repos.iter().position(|r| r.root == root)?;
		self.change_note_text(slot, loc)
	}

	/// A repo's read error or the notice that its list was cut at the cap.
	pub(super) fn change_note_row(
		&self,
		slot: usize,
		cx: &mut Context<Self>,
	) -> AnyElement {
		let loc = self.locale;
		let Some(repo) = self.change_repos.get(slot) else {
			return div().into_any_element();
		};
		let Some(ChangeNote { text, color, top }) =
			self.change_note_text(slot, loc)
		else {
			return div().into_any_element();
		};
		let root = repo.root.clone();
		let folded = top
			&& unreadable_slots(&self.change_repos, &self.files).len()
				>= UNREADABLE_MIN;
		let id = format!("change-note:{}", repo.name);
		div()
			.id(SharedString::from(id.clone()))
			.relative()
			.flex()
			.flex_row()
			.items_center()
			.gap(px(6.))
			.w_full()
			.h(px(ROW_H))
			// Otherwise it lines up with the icons of the rows it
			// speaks for: a single repo's groups, or a repo row's children.
			.pl(px(if folded {
				change_pad(1) + 4.
			} else if top {
				8.
			} else if self.change_repos.len() > 1 {
				change_pad(2) + 18.
			} else {
				change_pad(0) + 18.
			}))
			.pr(px(6.))
			.text_size(px(SMALL_TEXT))
			.text_color(rgb(color))
			// Read again at draw time: the language can change under a
			// tooltip that stays open (#208).
			.live_tip({
				let model = cx.weak_entity();
				move |cx| {
					model
						.upgrade()
						.and_then(|m| {
							let m = m.read(cx);
							m.change_note_text_for(&root, m.locale)
						})
						.map(|n| n.text.into())
						.unwrap_or_default()
				}
			})
			.child(icon(Icon::Warning, 12.))
			.child(fill_text(text))
			.debug_selector({
				let id = id.clone();
				move || id.clone()
			})
			.children(probe(&self.probes, id))
			.into_any_element()
	}

	/// Directory node under a repo ("Group By > Directory"): chevron,
	/// folder icon, the
	/// (possibly compacted) name and the muted count of files beneath it.
	pub(super) fn change_dir_row(
		&self,
		row: ChangeDirRow,
		row_idx: usize,
		cx: &mut Context<Self>,
	) -> AnyElement {
		let log = &self.probes;
		let ChangeDirRow {
			slot,
			group_id,
			path,
			name,
			count,
			depth,
		} = row;
		let repo = self
			.change_repos
			.get(slot)
			.map_or(String::new(), |s| s.name.clone());
		let collapsed = !self.change_dir_expanded(slot, group_id, &path);
		let cursor = row_idx == self.selected_list_row;
		let key = format!("{group_id}:{repo}:{path}");
		let id = format!("change-dir:{key}");
		let toggle_id = format!("change-dir-toggle:{key}");
		let tooltip = path.clone();
		let (p1, p2) = (path.clone(), path);
		div()
			.id(SharedString::from(id.clone()))
			.relative()
			.flex()
			.flex_row()
			.items_center()
			.w_full()
			.h(px(ROW_H))
			.pl(px(change_pad(depth)))
			.pr(px(6.))
			.gap(px(2.))
			.rounded(px(4.))
			.cursor_pointer()
			.when(cursor, |d| d.bg(rgb(self.left_selection_bg())))
			.when(!cursor, |d| d.hover(|s| s.bg(rgb(pal().hover_bg))))
			.tip(tooltip)
			.on_click(cx.listener(move |this, _, _, cx| {
				this.selected_list_row = row_idx;
				this.toggle_dir_collapsed(slot, group_id, &p1, cx);
			}))
			.on_mouse_down(
				MouseButton::Right,
				cx.listener(move |this, ev: &MouseDownEvent, w, cx| {
					this.selected_list_row = row_idx;
					let items = this.change_dir_menu(slot, group_id, &p2);
					this.open_left_menu(items, ev, w, cx);
				}),
			)
			.child(
				tree_chevron(toggle_id.clone(), collapsed)
					.children(probe(log, toggle_id)),
			)
			.child(
				div()
					.flex_shrink_0()
					.ml(px(4.))
					.child(icon(Icon::Folder, 14.)),
			)
			.child(
				self.speed_label(name)
					.ml(px(4.))
					.flex_shrink()
					.text_color(rgb(pal().text)),
			)
			.child(tree_count(count))
			.children(probe(log, id))
			.into_any_element()
	}

	pub(super) fn change_row(
		&self,
		ix: usize,
		depth: usize,
		row_idx: usize,
		cx: &mut Context<Self>,
	) -> AnyElement {
		let log = &self.probes;
		let Some(item) = self.files.get(ix) else {
			return div().into_any_element();
		};
		let (letter, change_color) = change_style(item.change_type);
		// IntelliJ conveys status by filename colour: untracked files share
		// the Unstaged group but keep their own colour.
		let color = if item.is_conflict {
			pal().git_conflict
		} else if item.source == SourceKind::Working {
			pal().git_untracked
		} else {
			change_color
		};
		let deleted = item.change_type == Some(ChangeType::Deleted);
		let path = item.path.clone();
		// Untracked directories come as `dir/`; label them by their own name.
		let is_dir = path.ends_with('/');
		let (dir, name) = match path.trim_end_matches('/').rsplit_once('/') {
			Some((d, n)) => (d.to_string(), n.to_string()),
			None => (String::new(), path.trim_end_matches('/').to_string()),
		};
		let cursor = row_idx == self.selected_list_row;
		let source_str = if item.is_conflict {
			"conflicted"
		} else {
			match item.source {
				SourceKind::Staged => "staged",
				SourceKind::Unstaged => "unstaged",
				SourceKind::Working => "untracked",
				_ => "working",
			}
		};
		let (legacy, repo) = self.change_probe_scope(item.repo as usize);
		let repo_row_id = format!("change-row@{repo}:{source_str}:{path}");
		let (row_id, src_row_id) = if legacy {
			(
				Some(format!("change-row:{path}")),
				Some(format!("change-row:{source_str}:{path}")),
			)
		} else {
			(None, None)
		};
		// Like the project tree, a name that is not UTF-8 cannot be copied.
		let utf8 = item.is_valid_utf8();
		let tooltip_base = format!("{path}  ({letter})");
		let model = cx.weak_entity();
		let tooltip = move |cx: &App| -> SharedString {
			let loc = model
				.upgrade()
				.map(|m| m.read(cx).locale)
				.unwrap_or_default();
			if utf8 {
				tooltip_base.clone().into()
			} else {
				format!("{tooltip_base}\n{}", t("change_not_utf8", loc)).into()
			}
		};

		div()
			.id(SharedString::from(format!("change-row-el:{ix}:{path}")))
			.relative()
			.flex()
			.flex_row()
			.items_center()
			.w_full()
			.h(px(ROW_H))
			// Leaves keep their parent's chevron column empty, so a file's
			// icon lines up with its sibling directories' icons.
			.pl(px(change_pad(depth) + 18.))
			.pr(px(6.))
			.gap(px(6.))
			.cursor_pointer()
			.rounded(px(4.))
			.when(cursor, |d| d.bg(rgb(self.left_selection_bg())))
			.when(!cursor, |d| d.hover(|s| s.bg(rgb(pal().hover_bg))))
			.on_mouse_down(
				MouseButton::Right,
				cx.listener(move |this, ev: &MouseDownEvent, w, cx| {
					this.selected_list_row = row_idx;
					let items = this.change_row_menu(ix);
					this.open_left_menu(items, ev, w, cx);
				}),
			)
			.live_tip(tooltip)
			.on_click(cx.listener(move |this, _, _, cx| {
				this.selected_list_row = row_idx;
				this.select_change(ix, cx);
			}))
			.child(icon(
				if is_dir {
					Icon::Folder
				} else {
					file_icon(&path)
				},
				14.,
			))
			.child(
				self.speed_label(name)
					.flex_shrink_0()
					.max_w(gpui::relative(0.7))
					.text_color(rgb(color))
					.when(deleted, |d| d.line_through()),
			)
			// The directory tree already shows where the file lives; the
			// flat list names its folder after it.
			.when(!self.chrome.changes_by_dir, |d| {
				d.child(
					fill_text(dir)
						.text_size(px(SMALL_TEXT))
						.text_color(rgb(pal().text_muted)),
				)
			})
			.children(probe(log, repo_row_id))
			.children(row_id.and_then(|id| probe(log, id)))
			.children(src_row_id.and_then(|id| probe(log, id)))
			.into_any_element()
	}

	pub(crate) fn changes_empty_state(&self) -> Option<ChangesEmpty> {
		let res =
			(|| {
				if !self.change_item_rows().is_empty() {
					return None;
				}
				if !self.workspace_open {
					if self.is_loading {
						return Some(ChangesEmpty::Loading);
					}
					return Some(ChangesEmpty::NoWorkspace);
				}
				let speed_active = !self.chrome.speed.trim().is_empty();
				let any_failed = self.change_repos.iter().any(|s| {
					matches!(s.state, crate::ChangeRepoState::Failed(_))
				});
				let any_truncated =
					self.change_repos.iter().enumerate().any(|(slot, r)| {
						r.truncated(crate::slot_range(&self.files, slot).len())
					});

				if speed_active
					&& (!self.files.is_empty() || any_failed || any_truncated)
				{
					return Some(ChangesEmpty::NoMatch);
				}
				if self.remote.session.is_some() {
					if let Some(msg) = &self.remote.scan_error {
						return Some(ChangesEmpty::ScanFailed(msg.clone()));
					}
				}
				if self.is_loading
					|| self.discovery_status.is_none()
					|| self.discovery_status
						== Some(snip_core::workspace::ScanStatus::More)
				{
					return Some(ChangesEmpty::Scanning);
				}
				if self.repos.is_empty() {
					if self.discovery_status
						== Some(snip_core::workspace::ScanStatus::Complete)
						&& self.discovery_errors.is_empty()
					{
						return Some(ChangesEmpty::NoRepository);
					} else {
						return Some(ChangesEmpty::ScanFailed(
							self.discovery_error_msg(),
						));
					}
				}
				if self.change_repos.len() != self.repos.len()
					|| !self.repos.iter().all(|r| {
						self.change_repos.iter().any(|c| c.root == r.root)
					}) {
					return Some(ChangesEmpty::Loading);
				}
				if self
					.change_repos
					.iter()
					.any(|s| matches!(s.state, crate::ChangeRepoState::Loading))
				{
					return Some(ChangesEmpty::Loading);
				}
				if !self.discovery_errors.is_empty() {
					return Some(ChangesEmpty::ScanFailed(
						self.discovery_error_msg(),
					));
				}
				if let Some(err) =
					self.change_repos.iter().find_map(|s| match &s.state {
						crate::ChangeRepoState::Failed(e) => Some(e.clone()),
						_ => None,
					}) {
					return Some(ChangesEmpty::ScanFailed(
						crate::i18n::Msg::with_msg_arg(
							"error_repo_status",
							[String::new()],
							0,
							err,
						),
					));
				}
				if any_truncated {
					return Some(ChangesEmpty::ScanFailed(
						self.discovery_error_msg(),
					));
				}
				// Clean only when truly clean; repos the scan never reached may have changes.
				// Incomplete leaves out only depth-limited folders with no read failure,
				// so clean loaded repos report a non-error partial-clean state.
				if self.discovery_status
					== Some(snip_core::workspace::ScanStatus::Incomplete)
				{
					return Some(ChangesEmpty::CleanPartial);
				}
				if self.discovery_status
					!= Some(snip_core::workspace::ScanStatus::Complete)
				{
					return Some(ChangesEmpty::ScanFailed(
						self.discovery_error_msg(),
					));
				}
				Some(ChangesEmpty::Clean)
			})();

		let state_str = match &res {
			None => None,
			Some(ChangesEmpty::NoWorkspace) => Some("no_workspace"),
			Some(ChangesEmpty::Scanning) => Some("scanning"),
			Some(ChangesEmpty::Loading) => Some("loading"),
			Some(ChangesEmpty::NoRepository) => Some("no_repository"),
			Some(ChangesEmpty::ScanFailed(_)) => Some("scan_failed"),
			Some(ChangesEmpty::NoMatch) => Some("no_match"),
			Some(ChangesEmpty::CleanPartial) => Some("clean_partial"),
			Some(ChangesEmpty::Clean) => Some("clean"),
		};
		if state_str != self.last_changes_empty.get() {
			self.last_changes_empty.set(state_str);
			if let Some(s) = state_str {
				app_log!("[APP:CHANGES_EMPTY: state={s}]");
			}
		}
		res
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn natural_cmp_orders_numbers_by_value() {
		let mut names = vec!["g10", "g2", "G1", "g11", "a"];
		names.sort_by(|a, b| natural_cmp(a, b));
		assert_eq!(names, ["a", "G1", "g2", "g10", "g11"]);
	}

	#[test]
	fn commit_file_rows_flat_and_tree_are_natural() {
		let files: Vec<(String, Option<ChangeType>)> =
			["g10/x.rs", "g2/y.rs", "G1/z.rs", "g11.rs", "a.rs"]
				.iter()
				.map(|p| (p.to_string(), None))
				.collect();
		let flat: Vec<String> = commit_file_rows(&files, false, &[])
			.into_iter()
			.filter_map(|r| match r {
				ChangeItemRow::File { file_idx, .. } => {
					Some(files[file_idx].0.clone())
				}
				_ => None,
			})
			.collect();
		assert_eq!(flat, ["a.rs", "G1/z.rs", "g2/y.rs", "g10/x.rs", "g11.rs"]);
		let dirs: Vec<String> = commit_file_rows(&files, true, &[])
			.into_iter()
			.filter_map(|r| match r {
				ChangeItemRow::Dir { name, .. } => Some(name),
				_ => None,
			})
			.collect();
		assert_eq!(dirs, ["G1", "g2", "g10"]);
	}
}
