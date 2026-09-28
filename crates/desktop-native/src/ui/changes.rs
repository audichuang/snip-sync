//! Git Changes tool window: the row model (groups, repos, the
//! directory tree) and the row renderers.

use super::*;

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
	File {
		file_idx: usize,
		depth: usize,
	},
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
		}
	}
}

/// Left padding of a Changes row at tree level `depth`: each level moves
/// one chevron plus gap, so a child's chevron sits under its parent's
/// checkbox and every level's chevrons share one column.
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
/// repos are not listed; a failed read with no rows gets a top-level note.
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
	for &slot in &shown {
		if crate::slot_range(files, slot).is_empty() && note(slot) {
			rows.push(ChangeItemRow::Note { slot });
		}
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
	let tree = DirTree {
		files,
		slot,
		group_id,
		expanded: &layout.expanded,
	};
	tree.push(rows, &mut members, 0, depth);
}

/// A file row the directory tree can place: its repository-relative path.
pub(crate) trait TreePath {
	fn tree_path(&self) -> &str;
}

impl TreePath for crate::FileChangeItem {
	fn tree_path(&self) -> &str {
		&self.path
	}
}

/// A file of a commit (or of a log multi-selection).
impl TreePath for (String, Option<ChangeType>) {
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
	push_change_files(
		&mut rows,
		files,
		(0..files.len()).collect(),
		0,
		COMMIT_GROUP,
		0,
		&layout,
	);
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

/// Builds IntelliJ's "Group By > Directory" rows from the capped file rows
/// alone: no Git reads, and only open directories' children get rows.
pub(super) struct DirTree<'a, T, F> {
	files: &'a [T],
	slot: usize,
	group_id: &'static str,
	expanded: &'a F,
}

impl<T: TreePath, F: Fn(usize, &str, &str) -> bool> DirTree<'_, T, F> {
	/// Path of file `i`. An untracked directory comes as `dir/` and is a
	/// leaf named by its last component.
	pub(super) fn path(&self, i: usize) -> &str {
		self.files[i].tree_path().trim_end_matches('/')
	}

	/// First directory of file `i` from byte `start` on, `None` for a leaf.
	pub(super) fn head(&self, i: usize, start: usize) -> Option<&str> {
		self.path(i)[start..].split_once('/').map(|(d, _)| d)
	}

	/// Rows of `entries`, files whose paths share the same `start`-byte
	/// prefix: directories first, then files, each by name ignoring case.
	pub(super) fn push(
		&self,
		rows: &mut Vec<ChangeItemRow>,
		entries: &mut [usize],
		start: usize,
		depth: usize,
	) {
		let key = |i: usize| match self.head(i, start) {
			Some(dir) => (false, dir),
			None => (true, &self.path(i)[start..]),
		};
		entries.sort_by(|&a, &b| {
			let ((la, na), (lb, nb)) = (key(a), key(b));
			la.cmp(&lb).then_with(|| {
				na.chars()
					.flat_map(char::to_lowercase)
					.cmp(nb.chars().flat_map(char::to_lowercase))
					.then_with(|| na.cmp(nb))
			})
		});
		let mut k = 0;
		while let Some(dir) = entries.get(k).and_then(|&i| self.head(i, start))
		{
			let end = k + entries[k..]
				.iter()
				.take_while(|&&i| self.head(i, start) == Some(dir))
				.count();
			let first = entries[k];
			// A directory whose only child is a directory shows as one row.
			let mut name_end = start + dir.len();
			while let Some(sub) = self.head(first, name_end + 1) {
				if !entries[k..end]
					.iter()
					.all(|&i| self.head(i, name_end + 1) == Some(sub))
				{
					break;
				}
				name_end += 1 + sub.len();
			}
			let path = &self.path(first)[..name_end];
			rows.push(ChangeItemRow::Dir {
				slot: self.slot,
				group_id: self.group_id,
				path: path.to_string(),
				name: path[start..].to_string(),
				count: end - k,
				depth,
			});
			if (self.expanded)(self.slot, self.group_id, path) {
				self.push(rows, &mut entries[k..end], name_end + 1, depth + 1);
			}
			k = end;
		}
		rows.extend(
			entries[k..]
				.iter()
				.map(|&file_idx| ChangeItemRow::File { file_idx, depth }),
		);
	}
}

impl WorkbenchModel {
	/// Changes group node, like IntelliJ's commit tool window: chevron,
	/// tri-state group checkbox, name and count.
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
		let chk_id = format!("change-group-chk:{group_id}");
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
				div()
					.id(SharedString::from(chk_id.clone()))
					.relative()
					.flex_shrink_0()
					.size(px(18.))
					.flex()
					.items_center()
					.justify_center()
					.on_click(cx.listener(move |this, _, _, cx| {
						cx.stop_propagation();
						this.toggle_change_group(group_id, cx);
					}))
					.child(tri_checkbox(self.group_state(group_id)))
					.children(probe(log, chk_id)),
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

	/// Repo row under a workspace group: chevron, tri-state checkbox for
	/// the repo's changes in that group, a square in the repo's Git Log
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
		let chk_id = format!("change-repo-chk:{group_id}:{name}");
		let toggle_id = format!("change-repo-toggle:{group_id}:{name}");
		let tooltip = repo.root.display().to_string();
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
			.when(self.chrome.menu.is_none(), |d| d.tooltip(tip(tooltip)))
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
			.child(
				div()
					.id(SharedString::from(chk_id.clone()))
					.relative()
					.flex_shrink_0()
					.size(px(18.))
					.flex()
					.items_center()
					.justify_center()
					.on_click(cx.listener(move |this, _, _, cx| {
						cx.stop_propagation();
						this.selected_list_row = row_idx;
						this.toggle_change_repo(slot, group_id, cx);
					}))
					.child(tri_checkbox(self.repo_state(slot, group_id)))
					.children(probe(log, chk_id)),
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

	/// A repo's read error or the notice that its list was cut at the cap.
	pub(super) fn change_note_row(&self, slot: usize) -> AnyElement {
		let loc = self.locale;
		let Some(repo) = self.change_repos.get(slot) else {
			return div().into_any_element();
		};
		let kept = crate::slot_range(&self.files, slot).len();
		let (text, color) = match &repo.state {
			crate::ChangeRepoState::Failed(err) => {
				(tf("changes_repo_error", loc, &[err]), pal().error)
			}
			_ => (
				tf("changes_truncated", loc, &[&kept, &repo.total]),
				pal().text_muted,
			),
		};
		// A multi-repo failure with no rows sits at the top level, under no
		// repo row, so it names its repo.
		let top = self.change_repos.len() > 1 && kept == 0;
		let text = if top {
			format!("{}: {text}", repo.name)
		} else {
			text
		};
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
			// Otherwise it lines up with the checkboxes of the rows it
			// speaks for: a single repo's groups, or a repo row's children.
			.pl(px(if top {
				8.
			} else if self.change_repos.len() > 1 {
				change_pad(2) + 18.
			} else {
				change_pad(0) + 18.
			}))
			.pr(px(6.))
			.text_size(px(SMALL_TEXT))
			.text_color(rgb(color))
			.when(self.chrome.menu.is_none(), |d| d.tooltip(tip(text.clone())))
			.child(icon(Icon::Warning, 12.))
			.child(fill_text(text))
			.children(probe(&self.probes, id))
			.into_any_element()
	}

	/// Directory node under a repo ("Group By > Directory"): chevron,
	/// tri-state checkbox over every file beneath it, folder icon, the
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
		let chk_id = format!("change-dir-chk:{key}");
		let toggle_id = format!("change-dir-toggle:{key}");
		let state = self.dir_state(slot, group_id, &path);
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
			.when(self.chrome.menu.is_none(), |d| d.tooltip(tip(tooltip)))
			.on_click(cx.listener(move |this, _, _, cx| {
				this.selected_list_row = row_idx;
				this.toggle_dir_collapsed(slot, group_id, &p1, cx);
			}))
			.child(
				tree_chevron(toggle_id.clone(), collapsed)
					.children(probe(log, toggle_id)),
			)
			.child(
				div()
					.id(SharedString::from(chk_id.clone()))
					.relative()
					.flex_shrink_0()
					.size(px(18.))
					.flex()
					.items_center()
					.justify_center()
					.on_click(cx.listener(move |this, _, _, cx| {
						cx.stop_propagation();
						this.selected_list_row = row_idx;
						this.toggle_change_dir(slot, group_id, &p2, cx);
					}))
					.child(tri_checkbox(state))
					.children(probe(log, chk_id)),
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
		// Like the project tree, a name that is not UTF-8 cannot be selected.
		let checkable = item.is_valid_utf8();
		let repo_chk_id = if checkable {
			format!("change-chk@{repo}:{source_str}:{path}")
		} else {
			format!("change-chk-invalid@{repo}:{source_str}:{ix}")
		};
		let (chk_id, src_chk_id) = match (legacy, checkable) {
			(false, _) => (None, None),
			(true, true) => (
				Some(format!("change-chk:{path}")),
				Some(format!("change-chk:{source_str}:{path}")),
			),
			(true, false) => (
				Some(format!("change-chk-invalid:{ix}")),
				Some(format!("change-chk-invalid:{source_str}:{ix}")),
			),
		};
		let tooltip = if checkable {
			format!("{path}  ({letter})")
		} else {
			format!("{path}  ({letter})\n{}", t("change_not_utf8", self.locale))
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
			// checkbox lines up with its sibling directories' checkboxes.
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
			.when(self.chrome.menu.is_none(), |d| d.tooltip(tip(tooltip)))
			.on_click(cx.listener(move |this, _, _, cx| {
				this.selected_list_row = row_idx;
				this.select_change(ix, cx);
			}))
			.child(
				div()
					.id(SharedString::from(repo_chk_id.clone()))
					.relative()
					.flex_shrink_0()
					.size(px(18.))
					.flex()
					.items_center()
					.justify_center()
					.when(checkable, |d| {
						d.on_click(cx.listener(move |this, _, _, cx| {
							cx.stop_propagation();
							this.selected_list_row = row_idx;
							this.toggle_file(ix, cx);
						}))
					})
					.child(if checkable {
						checkbox(item.selected).into_any_element()
					} else {
						div()
							.text_size(px(9.))
							.text_color(rgb(pal().text_muted))
							.child("×")
							.into_any_element()
					})
					.children(probe(log, repo_chk_id))
					.children(chk_id.and_then(|id| probe(log, id)))
					.children(src_chk_id.and_then(|id| probe(log, id))),
			)
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
}
