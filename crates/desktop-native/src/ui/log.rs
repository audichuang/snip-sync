//! Git Log chrome: filter chips, dropdown menus and the branches pane.

use super::*;

// ───────────────────────── log helpers (IJ-2a) ─────────────────────────

/// Dropdowns of the log: the filter chips and the More (⋮) button.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogMenu {
	Repo,
	Branch,
	User,
	Date,
	More,
}

impl LogMenu {
	pub(super) fn key(self) -> &'static str {
		match self {
			LogMenu::Repo => "repo",
			LogMenu::Branch => "branch",
			LogMenu::User => "user",
			LogMenu::Date => "date",
			LogMenu::More => "more",
		}
	}
}

/// What a log dropdown entry does when clicked.
pub(super) type MenuAction =
	Box<dyn Fn(&mut WorkbenchModel, &mut Context<WorkbenchModel>)>;

/// Date chip presets: probe key, `git log --since` value, label key.
pub(super) const DATE_PRESETS: [(&str, &str, &str); 4] = [
	("1d", "24 hours ago", "log_date_1d"),
	("7d", "7 days ago", "log_date_7d"),
	("30d", "30 days ago", "log_date_30d"),
	("1y", "1 year ago", "log_date_1y"),
];
/// Entries one filter dropdown lists; the branches pane search reaches more.
pub(super) const MAX_LOG_MENU_ITEMS: usize = 200;

/// One row of the branches pane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum BranchRow {
	/// A collapsible group: Local / Remote / Tags (`key` is the i18n key)
	/// or one remote under Remote (`key` is `remote:<name>`).
	Group {
		key: String,
		label: String,
		depth: usize,
		collapsed: bool,
	},
	/// A ref (full name) shown as `label`.
	Ref {
		name: String,
		label: String,
		depth: usize,
	},
}

/// The branches pane like IntelliJ's: Local, Remote grouped by remote
/// name, Tags; names matching `needle` (lowercase) only; at most
/// [`MAX_BRANCH_ROWS`] refs per group; `<remote>/HEAD` is not listed.
pub(super) fn branch_rows(
	refs: &[snip_core::browser::GitReference],
	needle: &str,
	collapsed: &[String],
	loc: Locale,
) -> Vec<BranchRow> {
	let is_collapsed = |k: &str| collapsed.iter().any(|c| c == k);
	// The merged log has a `main` per repository: one row filters them all.
	let mut seen = HashSet::new();
	let refs: Vec<&snip_core::browser::GitReference> = refs
		.iter()
		.filter(|r| seen.insert(r.name.as_str()))
		.collect();
	let mut out = Vec::new();
	for (key, prefix) in [
		("refs_local", "refs/heads/"),
		("refs_remote", "refs/remotes/"),
		("refs_tags", "refs/tags/"),
	] {
		let members: Vec<(&str, &str)> = refs
			.iter()
			.filter_map(|r| {
				Some((r.name.as_str(), r.name.strip_prefix(prefix)?))
			})
			.filter(|(_, short)| {
				!(prefix == "refs/remotes/" && short.ends_with("/HEAD"))
			})
			.filter(|(_, short)| {
				needle.is_empty() || short.to_lowercase().contains(needle)
			})
			.collect();
		if members.is_empty() {
			continue;
		}
		let group_collapsed = is_collapsed(key);
		out.push(BranchRow::Group {
			key: key.into(),
			label: t(key, loc).into(),
			depth: 0,
			collapsed: group_collapsed,
		});
		if group_collapsed {
			continue;
		}
		if prefix != "refs/remotes/" {
			out.extend(members.iter().take(MAX_BRANCH_ROWS).map(
				|(name, short)| BranchRow::Ref {
					name: name.to_string(),
					label: short.to_string(),
					depth: 1,
				},
			));
			continue;
		}
		let mut remotes: Vec<&str> = Vec::new();
		for (_, short) in &members {
			let remote = short.split_once('/').map_or(*short, |(r, _)| r);
			if !remotes.contains(&remote) {
				remotes.push(remote);
			}
		}
		remotes.sort_unstable();
		for remote in remotes {
			let key = format!("remote:{remote}");
			let sub_collapsed = is_collapsed(&key);
			out.push(BranchRow::Group {
				key,
				label: remote.to_string(),
				depth: 1,
				collapsed: sub_collapsed,
			});
			if sub_collapsed {
				continue;
			}
			out.extend(
				members
					.iter()
					.filter_map(|(name, short)| {
						let rest =
							short.strip_prefix(remote)?.strip_prefix('/')?;
						Some(BranchRow::Ref {
							name: name.to_string(),
							label: rest.to_string(),
							depth: 2,
						})
					})
					.take(MAX_BRANCH_ROWS),
			);
		}
	}
	out
}

/// The Branch chip's popup like IntelliJ's: Local, one section per
/// remote and Tags, each opened only when its key is in `open`; with a
/// `needle` (lowercase), the matching refs of every section as one flat
/// list instead, named `origin/x` for a remote branch.
pub(super) fn branch_popup_rows(
	refs: &[snip_core::browser::GitReference],
	needle: &str,
	open: &[String],
	loc: Locale,
) -> Vec<BranchRow> {
	if !needle.is_empty() {
		return branch_rows(refs, needle, &[], loc)
			.into_iter()
			.filter_map(|r| match r {
				BranchRow::Ref { name, .. } => Some(BranchRow::Ref {
					label: short_ref(&name).to_string(),
					name,
					depth: 0,
				}),
				BranchRow::Group { .. } => None,
			})
			.collect();
	}
	// Every section starts closed; Remote itself is not a row, its
	// remotes are.
	let mut closed: Vec<String> = ["refs_local", "refs_tags"]
		.into_iter()
		.map(str::to_string)
		.collect();
	for r in refs {
		if let Some((remote, _)) = r
			.name
			.strip_prefix("refs/remotes/")
			.and_then(|s| s.split_once('/'))
		{
			let key = format!("remote:{remote}");
			if !closed.contains(&key) {
				closed.push(key);
			}
		}
	}
	closed.retain(|k| !open.contains(k));
	branch_rows(refs, "", &closed, loc)
		.into_iter()
		.filter_map(|mut r| {
			match &mut r {
				BranchRow::Group { key, .. } if key == "refs_remote" => {
					return None;
				}
				BranchRow::Group { key, depth, .. }
				| BranchRow::Ref {
					name: key, depth, ..
				} => {
					if key.starts_with("remote:")
						|| key.starts_with("refs/remotes/")
					{
						*depth -= 1;
					}
				}
			}
			Some(r)
		})
		.collect()
}

/// Rows the Paths chip's picker lists at most.
pub(super) const MAX_PATH_PICKS: usize = 300;

/// One row of the Paths chip's picker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PathPick {
	/// Repository-relative path (the pathspec).
	pub(super) rel: String,
	pub(super) name: String,
	/// `None` for a chosen path the tree has not loaded.
	pub(super) is_dir: Option<bool>,
	pub(super) depth: usize,
	/// A folder (opening one the tree has not read loads it).
	pub(super) expandable: bool,
	pub(super) expanded: bool,
}

/// The Paths picker's rows: the project tree's loaded folders and files
/// (nested repositories and non-UTF-8 names left out), folders in
/// `expanded` opened, at most `max` rows.
pub(super) fn path_picker_rows(
	root: &crate::tree::FileTreeNode,
	expanded: &[String],
	max: usize,
) -> Vec<PathPick> {
	pub(super) fn walk(
		node: &crate::tree::FileTreeNode,
		depth: usize,
		expanded: &[String],
		max: usize,
		out: &mut Vec<PathPick>,
	) {
		for child in &node.children {
			if out.len() >= max {
				return;
			}
			if !child.is_valid_utf8 || child.is_nested_repo {
				continue;
			}
			let expandable = child.is_dir;
			let open = expandable && expanded.contains(&child.rel_path);
			out.push(PathPick {
				rel: child.rel_path.clone(),
				name: child.name.clone(),
				is_dir: Some(child.is_dir),
				depth,
				expandable,
				expanded: open,
			});
			if open {
				walk(child, depth + 1, expanded, max, out);
			}
		}
	}
	let mut out = Vec::new();
	walk(root, 0, expanded, max, &mut out);
	out
}

/// Loaded tree nodes the Paths picker reads at most per filter.
pub(super) const MAX_PATH_SCAN: usize = 20_000;

/// The Paths picker while its field has text: the loaded folders and
/// files whose `prefix` + path contains `needle` (lowercase), each under
/// its folders (shown open, not collapsible: the filter decides what
/// shows), at most `max` rows and [`MAX_PATH_SCAN`] nodes read. Folders
/// the tree has not read are not searched.
pub(super) fn path_picker_matches(
	root: &crate::tree::FileTreeNode,
	needle: &str,
	prefix: &str,
	max: usize,
) -> Vec<PathPick> {
	fn walk(
		node: &crate::tree::FileTreeNode,
		depth: usize,
		needle: &str,
		prefix: &str,
		max: usize,
		budget: &mut usize,
		out: &mut Vec<PathPick>,
	) {
		for child in &node.children {
			if out.len() >= max || *budget == 0 {
				return;
			}
			*budget -= 1;
			if !child.is_valid_utf8 || child.is_nested_repo {
				continue;
			}
			// Pushed first so it lands above its matches; dropped again if
			// neither it nor anything under it matches.
			let at = out.len();
			let rel = format!("{prefix}{}", child.rel_path);
			let hit = rel.to_lowercase().contains(needle);
			out.push(PathPick {
				rel,
				name: child.name.clone(),
				is_dir: Some(child.is_dir),
				depth,
				expandable: false,
				expanded: false,
			});
			if child.is_dir {
				walk(child, depth + 1, needle, prefix, max, budget, out);
			}
			if out.len() > at + 1 {
				out[at].expanded = true;
			} else if !hit {
				out.pop();
			}
		}
	}
	let mut out = Vec::new();
	let mut budget = MAX_PATH_SCAN;
	walk(root, 0, needle, prefix, max, &mut budget, &mut out);
	out
}

/// Rows per branches-pane group.
pub(super) const MAX_BRANCH_ROWS: usize = 200;
pub(super) const AUTHOR_W: f32 = 120.;
pub(super) const DATE_W: f32 = 118.;
/// The subject width a log row keeps while the gutter, author, date and ref
/// labels are squeezed, where the window allows.
pub(super) const MIN_SUBJECT_W: f32 = 160.;
/// The narrowest a ref-label group is squeezed to; below it the labels are
/// dropped altogether.
const MIN_LABELS_W: f32 = 80.;
/// The narrowest the graph gutter is clipped to.
const MIN_GUTTER_W: f32 = 72.;

/// The narrowest the author and date cells are squeezed to (their text is
/// clipped) once the graph gutter is at its floor.
const MIN_AUTHOR_W: f32 = 72.;
const MIN_DATE_W: f32 = 88.;

/// The widths one list decides for all its rows, so columns line up from row
/// to row.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct ListCols {
	pub gutter: f32,
	pub author: f32,
	pub date: f32,
}

/// How a log row divides its width: the graph gutter, the ref labels and
/// what is left for the subject.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct RowWidths {
	pub gutter: f32,
	pub labels: f32,
	pub subject: f32,
}

/// What a `list_w` wide row leaves for the gutter, labels and subject after
/// 8px right padding, the 8px gaps between the cells and the optional hash.
fn row_base(list_w: f32, show_hash: bool) -> f32 {
	let gaps = if show_hash { 4. } else { 3. } * 8.;
	let hash_w = if show_hash { 64. } else { 0. };
	list_w - 8. - gaps - hash_w
}

/// The graph gutter, author and date widths of every row in a `list_w` wide
/// list, decided once per list. Pieces give way to the subject in this
/// order: the gutter is clipped (only as far as a squeezed label group, when
/// `has_labels`, and the subject need), then no further than
/// [`MIN_GUTTER_W`]; below that the date and then the author cell shrink
/// until the subject reaches [`MIN_SUBJECT_W`]. On a very narrow list the
/// subject takes what is left. With `has_labels`, the room the gutter floor
/// takes back from the label reserve is made up by the date and author when
/// they can cover all of it (so the labels keep [`MIN_LABELS_W`]); otherwise
/// the labels are dropped and only the subject's lack is taken.
/// [`MIN_SUBJECT_W`] is not guaranteed on
/// lists under about 520px with the hash column on (a 900 window with the
/// hash shown leaves the subject about 97px).
pub(super) fn list_cols(
	list_w: f32,
	gutter_w: f32,
	has_labels: bool,
	show_hash: bool,
) -> ListCols {
	let base = row_base(list_w, show_hash);
	// The 6px before the labels only exists with labels.
	let labels = if has_labels { MIN_LABELS_W + 6. } else { 0. };
	let room = base - AUTHOR_W - DATE_W - MIN_SUBJECT_W - labels;
	let gutter = gutter_w.min(room.max(MIN_GUTTER_W));
	// What the subject still lacks with the gutter placed.
	let mut lack =
		(MIN_SUBJECT_W - (base - gutter - AUTHOR_W - DATE_W)).max(0.);
	// The gutter floor took back part of the label reserve; date and author
	// give that room up so the labels still fit. When they cannot cover all
	// of it the labels are dropped anyway, so they keep their width for the
	// subject-only lack. The deficit already contains what the subject
	// reserve needs, so it replaces the lack rather than adding to it.
	if has_labels {
		let deficit = (gutter - room).max(0.);
		let give = (DATE_W - MIN_DATE_W) + (AUTHOR_W - MIN_AUTHOR_W);
		if deficit <= give {
			lack = lack.max(deficit);
		}
	}
	let date = DATE_W - lack.min(DATE_W - MIN_DATE_W);
	lack -= DATE_W - date;
	let author = AUTHOR_W - lack.min(AUTHOR_W - MIN_AUTHOR_W);
	ListCols {
		gutter,
		author,
		date,
	}
}

/// Divides a `list_w` wide row laid out with `cols`. The ref labels
/// (`labels_w`, 0 for none) shrink first, down to [`MIN_LABELS_W`], then go,
/// so the subject keeps [`MIN_SUBJECT_W`] where the window allows. With room
/// to spare nothing changes.
pub(super) fn row_widths(
	list_w: f32,
	cols: ListCols,
	labels_w: f32,
	show_hash: bool,
) -> RowWidths {
	let gutter = cols.gutter;
	let avail = row_base(list_w, show_hash) - cols.author - cols.date - gutter;
	let mut labels = labels_w;
	// The 6px before the labels only exists with labels.
	if labels > 0. && avail - labels - 6. < MIN_SUBJECT_W {
		let room = avail - 6. - MIN_SUBJECT_W;
		labels = if room >= labels_w.min(MIN_LABELS_W) {
			room.min(labels_w)
		} else {
			0.
		};
	}
	let with_labels = if labels > 0. { labels + 6. } else { 0. };
	RowWidths {
		gutter,
		labels,
		subject: (avail - with_labels).max(0.),
	}
}

/// `refs/heads/main` → `main`, `refs/remotes/origin/x` → `origin/x`.
pub(super) fn short_ref(name: &str) -> &str {
	["refs/heads/", "refs/remotes/", "refs/tags/"]
		.iter()
		.find_map(|p| name.strip_prefix(p))
		.unwrap_or(name)
}

/// Width of `s` laid out in the UI font at `size`, from the text system
/// (shaped lines are cached per frame, so a redraw does not reshape).
pub(super) fn text_width(window: &Window, s: &str, size: f32) -> f32 {
	let s = s.replace(['\n', '\r'], " ");
	let run = gpui::TextRun {
		len: s.len(),
		font: gpui::font(UI_FONT),
		color: gpui::black(),
		background_color: None,
		underline: None,
		strikethrough: None,
	};
	f32::from(
		window
			.text_system()
			.shape_line(s.into(), px(size), &[run], None)
			.width,
	)
}

/// [`log_date_in`] in the viewer's time zone, now.
pub(super) fn log_date(iso: &str, loc: Locale) -> String {
	log_date_in(iso, loc, chrono::Utc::now(), &chrono::Local)
}

/// IntelliJ-style short date of an ISO-8601 commit date, shown in `tz`:
/// "Today 20:57", "Yesterday 20:57", else a locale short date.
pub(super) fn log_date_in<Tz: chrono::TimeZone>(
	iso: &str,
	loc: Locale,
	now: chrono::DateTime<chrono::Utc>,
	tz: &Tz,
) -> String {
	use chrono::{Datelike, Timelike};
	let Ok(at) = chrono::DateTime::parse_from_rfc3339(iso) else {
		return short_date(iso);
	};
	let at = at.with_timezone(tz);
	let today = now.with_timezone(tz).date_naive();
	let time = format!("{:02}:{:02}", at.hour(), at.minute());
	let (y, mo, d) = (at.year(), at.month(), at.day());
	match (today - at.date_naive()).num_days() {
		0 => tf("log_today", loc, &[&time]),
		1 => tf("log_yesterday", loc, &[&time]),
		_ => match loc {
			Locale::ZhTw => format!("{y}/{mo}/{d} {time}"),
			Locale::En => format!("{mo}/{d}/{:02}, {time}", y % 100),
		},
	}
}

/// Square ghost icon button of the log toolbars (IntelliJ's 26px action
/// buttons); the label is its tooltip.
pub(super) fn log_icon_button(
	id: &'static str,
	ic: Icon,
	tooltip: impl Into<SharedString>,
	enabled: bool,
	active: bool,
) -> Stateful<Div> {
	div()
		.id(id)
		.relative()
		.flex_shrink_0()
		.size(px(26.))
		.flex()
		.items_center()
		.justify_center()
		.rounded(px(4.))
		.border_1()
		.border_color(transparent_black())
		.tooltip(tip(tooltip))
		.when(active, |d| d.bg(rgb(pal().hover_bg)))
		.when(enabled, |d| {
			d.cursor_pointer().hover(|s| s.bg(rgb(pal().hover_bg)))
		})
		.focus(|s| s.border_color(rgb(pal().focus_ring)))
		.child(
			div()
				.flex()
				.when(!enabled, |d| d.opacity(0.4))
				.child(icon(ic, 16.)),
		)
}

/// The coloured label glyph of a ref.
pub(super) fn label_icon(l: &graph_view::RefLabel) -> gpui::Svg {
	icon_tinted(if l.current { Icon::Head } else { Icon::Tag }, 14., l.color)
}

/// A row's ref labels, right-aligned at the end of the subject: one label
/// icon plus the names of at most two badges, the rest folded into `+N`. Also returns their
/// laid-out width (`measure` gives a name's text width).
pub(super) fn ref_label_elements(
	refs: &[snip_core::graph::RefInfo],
	current_branch: Option<&str>,
	row: &str,
	show_tips: bool,
	measure: &dyn Fn(&str) -> f32,
) -> (Vec<AnyElement>, f32) {
	let (shown, hidden) = graph_view::visible_refs(refs, current_branch);
	// One label for the shown badges: icon (14) + 3 + names, at most 320
	// (the room two separate labels had).
	let mut width = 0.;
	let mut out: Vec<AnyElement> =
		graph_view::combined_label(&shown, current_branch)
			.into_iter()
			.map(|l| {
				width += (17. + measure(&l.text)).min(320.);
				let tooltip = shown
					.iter()
					.flat_map(|b| {
						std::iter::once(b.primary)
							.chain(b.merged.iter().copied())
					})
					.map(|i| graph_view::format_ref_badge(i).0)
					.collect::<Vec<_>>()
					.join("\n");
				div()
					.id(SharedString::from(format!(
						"ref-badge:{row}:{}",
						l.text
					)))
					.flex()
					.items_center()
					.gap(px(3.))
					.max_w(px(320.))
					// Squeezed by the labels container, the text ellipsizes.
					.min_w_0()
					.when(show_tips, |d| d.tooltip(tip(tooltip)))
					.child(label_icon(&l))
					.child(
						clip_text(l.text.clone())
							.text_size(px(SMALL_TEXT))
							.text_color(rgb(pal().log_ref_text)),
					)
					.into_any_element()
			})
			.collect();
	if hidden > 0 {
		width += 8. + measure(&format!("+{hidden}"));
		let all = refs
			.iter()
			.map(|i| graph_view::format_ref_badge(i).0)
			.collect::<Vec<_>>()
			.join("\n");
		out.push(
			div()
				.id(SharedString::from(format!("ref-more:{row}")))
				.flex_shrink_0()
				.text_size(px(SMALL_TEXT))
				.text_color(rgb(pal().log_ref_text))
				.when(show_tips, |d| d.tooltip(tip(all)))
				.child(format!("+{hidden}"))
				.into_any_element(),
		);
	}
	(out, width)
}

impl WorkbenchModel {
	// ───────────────────────── git log ─────────────────────────

	// ───────────────────────── log (IJ-2a) ─────────────────────────

	/// Filter chip of the log's filter bar: `Name▾`, or `Name: value ✕`.
	pub(super) fn log_chip(
		&self,
		menu: LogMenu,
		label: &str,
		value: Option<String>,
		cx: &mut Context<Self>,
	) -> AnyElement {
		let log = &self.probes;
		// A one-repo workspace's repo chip only holds paths; drivers read
		// "log-filter-repo" as "the workspace has several repositories".
		let key = match menu {
			LogMenu::Repo if self.repos.len() <= 1 => "paths",
			_ => menu.key(),
		};
		let id = format!("log-filter-{key}");
		let clear_id = format!("log-filter-{key}-clear");
		let selector = id.clone();
		let open = self.log_menu == Some(menu);
		let active = value.is_some();
		div()
			.id(SharedString::from(id.clone()))
			.debug_selector(move || selector)
			.relative()
			.flex_shrink_0()
			.h(px(24.))
			.px(px(6.))
			.flex()
			.items_center()
			.gap(px(3.))
			.rounded(px(4.))
			.cursor_pointer()
			.text_size(px(UI_TEXT))
			.when(open, |d| d.bg(rgb(pal().hover_bg)))
			.hover(|s| s.bg(rgb(pal().hover_bg)))
			.on_click(
				cx.listener(move |this, _, _, cx| {
					this.toggle_log_menu(menu, cx)
				}),
			)
			.child(
				div()
					.max_w(px(180.))
					.overflow_hidden()
					.line_clamp(1)
					.text_ellipsis()
					.text_color(rgb(pal().text))
					.child(match &value {
						Some(v) => format!("{label}: {v}"),
						None => label.to_string(),
					}),
			)
			.child(if active {
				div()
					.id(SharedString::from(clear_id.clone()))
					.relative()
					.flex()
					.rounded(px(3.))
					.hover(|s| s.bg(rgb(pal().divider)))
					.tooltip(tip(t("log_clear_filter", self.locale)))
					.on_click(cx.listener(move |this, _, _, cx| {
						cx.stop_propagation();
						this.clear_log_chip(menu, cx);
					}))
					.child(icon(Icon::Close, 12.))
					.children(probe(log, clear_id))
					.into_any_element()
			} else {
				icon(Icon::ChevronDown, 12.).into_any_element()
			})
			.when(open, |d| d.child(self.log_menu_panel(menu, cx)))
			.children(probe(log, id))
			.into_any_element()
	}

	pub(super) fn clear_log_chip(
		&mut self,
		menu: LogMenu,
		cx: &mut Context<Self>,
	) {
		self.log_menu = None;
		match menu {
			LogMenu::Repo => {
				self.log_path_input.update(cx, |i, cx| i.set_text("", cx));
				// Leaving a repository scope drops its paths too.
				if self.log_repo_filter.is_empty() {
					self.clear_log_paths(cx)
				} else {
					self.set_log_repos(Vec::new(), cx)
				}
			}
			LogMenu::Branch => self.filter_by_ref(None, cx),
			LogMenu::User => self.set_log_author(None, cx),
			LogMenu::Date => self.set_log_since(None, cx),
			LogMenu::More => cx.notify(),
		}
	}

	/// The dropdown under a filter chip or the More button.
	pub(super) fn log_menu_panel(
		&self,
		menu: LogMenu,
		cx: &mut Context<Self>,
	) -> AnyElement {
		let loc = self.locale;
		let log = &self.probes;
		let item = |id: String,
		            label: String,
		            checked: bool,
		            on: MenuAction,
		            cx: &mut Context<Self>| {
			let selector = id.clone();
			div()
				.id(SharedString::from(id.clone()))
				.debug_selector(move || selector)
				.relative()
				.h(px(24.))
				.px(px(8.))
				.flex()
				.items_center()
				.gap(px(6.))
				.rounded(px(4.))
				.cursor_pointer()
				.hover(|s| s.bg(rgb(pal().hover_bg)))
				.on_click(cx.listener(move |this, _, _, cx| {
					cx.stop_propagation();
					on(this, cx)
				}))
				.child(
					div()
						.flex_shrink_0()
						.w(px(14.))
						.when(checked, |d| d.child(icon(Icon::Checked, 14.))),
				)
				.child(fill_text(label))
				.children(probe(log, id))
				.into_any_element()
		};
		let mut items: Vec<AnyElement> = Vec::new();
		match menu {
			// One chip for both: the filter field, the repositories (a row
			// keeps only its repository, the checkbox adds or drops it),
			// then the paths.
			LogMenu::Repo => {
				items.push(
					div()
						.id("log-path-input")
						.relative()
						.mx(px(4.))
						.px(px(4.))
						.rounded(px(4.))
						.border_1()
						.border_color(rgb(pal().button_border))
						.child(self.log_path_input.clone())
						.children(probe(log, "log-path-input"))
						.into_any_element(),
				);
				items.push(
					div()
						.px(px(8.))
						.py(px(2.))
						.text_size(px(SMALL_TEXT))
						.text_color(rgb(pal().text_muted))
						.child(t("log_paths_hint", loc))
						.into_any_element(),
				);
				// The project's folders and files as far as loaded; chosen
				// paths the tree does not show (typed, or in a closed
				// folder) are listed above it.
				let chosen = &self.log_filter.paths;
				// Typed text filters the repositories and the loaded tree
				// (IntelliJ).
				let needle =
					self.log_path_input.read(cx).text().trim().to_lowercase();
				if self.repos.len() > 1 {
					let all = self.log_repo_filter.is_empty();
					items.push(item(
						"log-repo:all".into(),
						t("log_repo_all", loc).to_string(),
						all,
						Box::new(|this, cx| this.set_log_repos(Vec::new(), cx)),
						cx,
					));
					let scope = self.log_scope();
					for (ix, repo) in self
						.repos
						.iter()
						.enumerate()
						.filter(|(_, r)| {
							r.name.to_lowercase().contains(&needle)
						})
						.take(MAX_LOG_MENU_ITEMS)
					{
						let checked =
							!all && scope.iter().any(|(r, _)| *r == repo.root);
						let root = repo.root.clone();
						let id = format!("log-repo:{}", repo.name);
						let check_id = format!("log-repo-check:{}", repo.name);
						items.push(
							div()
								.id(SharedString::from(id.clone()))
								.relative()
								.h(px(24.))
								.px(px(8.))
								.flex()
								.items_center()
								.gap(px(6.))
								.rounded(px(4.))
								.cursor_pointer()
								.hover(|s| s.bg(rgb(pal().hover_bg)))
								// The row picks only this repository; its checkbox
								// adds or removes it.
								.on_click({
									let root = root.clone();
									cx.listener(move |this, _, _, cx| {
										cx.stop_propagation();
										this.set_log_repos(
											vec![root.clone()],
											cx,
										)
									})
								})
								.child(
									div()
										.id(SharedString::from(
											check_id.clone(),
										))
										.relative()
										.flex_shrink_0()
										.on_click(cx.listener(
											move |this, _, _, cx| {
												cx.stop_propagation();
												this.toggle_log_repo(
													root.clone(),
													cx,
												)
											},
										))
										.child(checkbox(checked))
										.children(probe(log, check_id)),
								)
								.child(
									div()
										.flex_shrink_0()
										.size(px(8.))
										.rounded(px(2.))
										.bg(graph_view::palette_rgb(ix)),
								)
								.child(fill_text(repo.name.clone()))
								.children(probe(log, id))
								.into_any_element(),
						);
					}
					items.push(
						div()
							.my(px(4.))
							.h(px(1.))
							.bg(rgb(pal().divider))
							.into_any_element(),
					);
				}
				// The loaded tree is the toolbar repository's; a log scoped
				// to another one gets typed paths only.
				let other_repo = !self.log_is_merged()
					&& self.log_scope().first().map(|(r, _)| r.clone())
						!= self.repo_root();
				let tree_rows = match self.file_tree.as_ref() {
					_ if self.log_is_merged() => {
						self.merged_path_picks(&needle)
					}
					_ if other_repo => Vec::new(),
					Some(tree) if tree.is_loaded && !needle.is_empty() => {
						path_picker_matches(tree, &needle, "", MAX_PATH_PICKS)
					}
					Some(tree) if tree.is_loaded => path_picker_rows(
						tree,
						&self.log_paths_expanded,
						MAX_PATH_PICKS,
					),
					_ => {
						items.push(
							div()
								.px(px(8.))
								.text_size(px(SMALL_TEXT))
								.text_color(rgb(pal().text_muted))
								.child(t("log_paths_tree_empty", loc))
								.into_any_element(),
						);
						Vec::new()
					}
				};
				let extra: Vec<PathPick> = chosen
					.iter()
					.filter(|p| !tree_rows.iter().any(|r| &r.rel == *p))
					.filter(|p| p.to_lowercase().contains(&needle))
					.map(|p| PathPick {
						rel: p.clone(),
						name: p.clone(),
						is_dir: None,
						depth: 0,
						expandable: false,
						expanded: false,
					})
					.collect();
				for pick in extra.into_iter().chain(tree_rows) {
					items.push(self.path_pick_row(pick, cx));
				}
			}
			LogMenu::Branch => {
				items.push(
					div()
						.id("log-branch-input")
						.relative()
						.mx(px(4.))
						.mb(px(2.))
						.px(px(4.))
						.rounded(px(4.))
						.border_1()
						.border_color(rgb(pal().button_border))
						.child(self.log_branch_menu_input.clone())
						.children(probe(log, "log-branch-input"))
						.into_any_element(),
				);
				let needle = self
					.log_branch_menu_input
					.read(cx)
					.text()
					.trim()
					.to_lowercase();
				let head = (self.head_sha.is_some() || self.log_is_merged())
					&& "head".contains(needle.as_str());
				let rows = branch_popup_rows(
					&self.refs,
					&needle,
					&self.log_branch_menu_open,
					loc,
				);
				let refs = head
					.then(|| BranchRow::Ref {
						name: "HEAD".into(),
						label: "HEAD".into(),
						depth: 0,
					})
					.into_iter()
					.chain(rows)
					.take(MAX_LOG_MENU_ITEMS);
				for row in refs {
					match row {
						BranchRow::Group {
							key,
							label,
							collapsed,
							..
						} => {
							let id = format!("log-branch-group:{key}");
							let selector = id.clone();
							items.push(
								div()
									.id(SharedString::from(id.clone()))
									.debug_selector(move || selector)
									.relative()
									.h(px(24.))
									.px(px(8.))
									.flex()
									.items_center()
									.gap(px(6.))
									.rounded(px(4.))
									.cursor_pointer()
									.hover(|s| s.bg(rgb(pal().hover_bg)))
									.on_click(cx.listener(
										move |this, _, _, cx| {
											cx.stop_propagation();
											let open =
												&mut this.log_branch_menu_open;
											match open
												.iter()
												.position(|k| *k == key)
											{
												Some(i) => {
													open.remove(i);
												}
												None => open.push(key.clone()),
											}
											cx.notify();
										},
									))
									.child(div().flex_shrink_0().w(px(14.)))
									.child(fill_text(label))
									.child(icon(
										if collapsed {
											Icon::ChevronRight
										} else {
											Icon::ChevronDown
										},
										12.,
									))
									.children(probe(log, id))
									.into_any_element(),
							);
						}
						BranchRow::Ref { name, label, depth } => {
							let checked = self.active_ref_filter.as_deref()
								== Some(&name);
							let target = name.clone();
							items.push(
								div()
									.pl(px(depth as f32 * 14.))
									.child(item(
										format!("log-branch:{name}"),
										label,
										checked,
										Box::new(move |this, cx| {
											this.dismiss_log_menu(cx);
											this.filter_by_ref(
												Some(target.clone()),
												cx,
											)
										}),
										cx,
									))
									.into_any_element(),
							);
						}
					}
				}
			}
			LogMenu::User => {
				if let Some(email) = self.git_user_email.clone() {
					let checked =
						self.log_filter.author.as_deref() == Some(&email);
					items.push(item(
						"log-user:me".into(),
						format!("{} ({email})", t("log_user_me", loc)),
						checked,
						Box::new(move |this, cx| {
							this.set_log_author(Some(email.clone()), cx)
						}),
						cx,
					));
				}
				for name in self.log_authors(MAX_LOG_MENU_ITEMS) {
					let checked =
						self.log_filter.author.as_deref() == Some(&name);
					let target = name.clone();
					items.push(item(
						format!("log-user:{name}"),
						name,
						checked,
						Box::new(move |this, cx| {
							this.set_log_author(Some(target.clone()), cx)
						}),
						cx,
					));
				}
			}
			LogMenu::Date => {
				for (key, since, label) in DATE_PRESETS {
					let checked =
						self.log_filter.since.as_deref() == Some(since);
					items.push(item(
						format!("log-date:{key}"),
						t(label, loc).to_string(),
						checked,
						Box::new(move |this, cx| {
							this.set_log_since(Some(since), cx)
						}),
						cx,
					));
				}
				// IntelliJ's "Select…": a custom range, both days included.
				let field =
					|id: &'static str,
					 input: &gpui::Entity<crate::text_input::TextInput>| {
						div()
							.id(id)
							.relative()
							.w(px(118.))
							.px(px(4.))
							.rounded(px(4.))
							.border_1()
							.border_color(rgb(pal().button_border))
							.child(input.clone())
							.children(probe(log, id))
					};
				items.push(
					div()
						.mt(px(4.))
						.px(px(8.))
						.pt(px(4.))
						.border_t_1()
						.border_color(rgb(pal().divider))
						.text_size(px(SMALL_TEXT))
						.text_color(rgb(pal().text_muted))
						.child(t("log_date_custom", loc))
						.into_any_element(),
				);
				items.push(
					div()
						.px(px(8.))
						.py(px(4.))
						.flex()
						.items_center()
						.gap(px(4.))
						.child(field("log-date-since", &self.log_since_input))
						.child("–")
						.child(field("log-date-until", &self.log_until_input))
						.child(
							button(
								"log-date-apply",
								t("log_date_apply", loc),
								Btn::Default,
								true,
								0,
							)
							.h(px(22.))
							.px(px(8.))
							.on_click(cx.listener(|this, _, _, cx| {
								cx.stop_propagation();
								this.apply_log_date_range(cx)
							}))
							.children(probe(log, "log-date-apply")),
						)
						.into_any_element(),
				);
				if self.log_date_error {
					items.push(
						div()
							.px(px(8.))
							.text_size(px(SMALL_TEXT))
							.text_color(rgb(pal().error))
							.child(t("log_date_invalid", loc))
							.into_any_element(),
					);
				}
			}
			LogMenu::More => {
				for (key, label, checked) in [
					("details", "log_more_details", self.log_details_visible),
					(
						"branches",
						"log_more_branches",
						self.log_branches_visible,
					),
					("hash", "log_more_hash", self.log_show_hash),
				] {
					items.push(item(
						format!("log-more:{key}"),
						t(label, loc).to_string(),
						checked,
						Box::new(move |this, cx| {
							this.log_menu = None;
							match key {
								"details" => {
									this.log_details_visible =
										!this.log_details_visible
								}
								"branches" => {
									this.log_branches_visible =
										!this.log_branches_visible
								}
								_ => this.log_show_hash = !this.log_show_hash,
							}
							app_log!("[APP:LOG_VIEW: {}]", key);
							cx.notify();
						}),
						cx,
					));
				}
			}
		}
		let panel = div()
			.id("log-menu")
			.occlude()
			.min_w(px(200.))
			.max_w(px(360.))
			.max_h(px(if menu == LogMenu::Repo { 480. } else { 320. }))
			.overflow_y_scroll()
			.flex()
			.flex_col()
			.p(px(4.))
			.bg(rgb(pal().panel_bg))
			.border_1()
			.border_color(rgb(pal().button_border))
			.rounded(px(6.))
			.shadow_lg()
			.text_size(px(UI_TEXT))
			.text_color(rgb(pal().text))
			.on_mouse_down_out(
				cx.listener(|this, _, _, cx| this.close_log_menu(cx)),
			)
			.children(items);
		div()
			.absolute()
			.top(px(26.))
			.left_0()
			.child(
				deferred(anchored().snap_to_window().child(panel))
					.with_priority(1),
			)
			.into_any_element()
	}

	/// The merged log's Paths picker: its repositories on top; the selected
	/// one opens into the project tree's loaded folders. A path is
	/// `<repository>/<path>`, a repository alone keeps all its history.
	/// A `needle` (lowercase) keeps the matching paths only.
	pub(super) fn merged_path_picks(&self, needle: &str) -> Vec<PathPick> {
		let selected = self.repo().map(|r| r.name.clone());
		let tree = self.file_tree.as_ref().filter(|t| t.is_loaded);
		let mut out = Vec::new();
		for (_, name) in self.log_scope() {
			let mine = tree.filter(|_| selected.as_deref() == Some(&name));
			if !needle.is_empty() {
				let hits = mine
					.map(|t| {
						path_picker_matches(
							t,
							needle,
							&format!("{name}/"),
							MAX_PATH_PICKS,
						)
					})
					.unwrap_or_default();
				// The repositories themselves are listed above the paths.
				if hits.is_empty() {
					continue;
				}
				out.push(PathPick {
					rel: name.clone(),
					name: name.clone(),
					is_dir: Some(true),
					depth: 0,
					expandable: false,
					expanded: !hits.is_empty(),
				});
				out.extend(hits.into_iter().map(|mut pick| {
					pick.depth += 1;
					pick
				}));
				if out.len() >= MAX_PATH_PICKS {
					out.truncate(MAX_PATH_PICKS);
					break;
				}
				continue;
			}
			// The repositories are listed above the paths; only the
			// toolbar's one, whose tree is loaded, opens here.
			if mine.is_none() {
				continue;
			}
			let expanded =
				mine.is_some() && self.log_paths_expanded.contains(&name);
			out.push(PathPick {
				rel: name.clone(),
				name: name.clone(),
				is_dir: Some(true),
				depth: 0,
				expandable: mine.is_some(),
				expanded,
			});
			if let (true, Some(tree)) = (expanded, mine) {
				let prefix = format!("{name}/");
				let open: Vec<String> = self
					.log_paths_expanded
					.iter()
					.filter_map(|p| p.strip_prefix(&prefix).map(str::to_string))
					.collect();
				out.extend(
					path_picker_rows(tree, &open, MAX_PATH_PICKS)
						.into_iter()
						.map(|mut pick| {
							pick.rel = format!("{prefix}{}", pick.rel);
							pick.depth += 1;
							pick
						}),
				);
			}
			if out.len() >= MAX_PATH_PICKS {
				out.truncate(MAX_PATH_PICKS);
				break;
			}
		}
		out
	}

	/// One row of the Paths picker: chevron (a loaded folder), checkbox,
	/// icon and name; clicking the row checks or unchecks the path.
	pub(super) fn path_pick_row(
		&self,
		pick: PathPick,
		cx: &mut Context<Self>,
	) -> AnyElement {
		let log = &self.probes;
		let checked = self.log_filter.paths.contains(&pick.rel);
		let id = format!("log-path-pick:{}", pick.rel);
		let exp_id = format!("log-path-expand:{}", pick.rel);
		let rel = pick.rel.clone();
		let exp_rel = pick.rel.clone();
		div()
			.id(SharedString::from(id.clone()))
			.relative()
			.h(px(24.))
			.pl(px(4. + pick.depth as f32 * 16.))
			.pr(px(8.))
			.flex()
			.items_center()
			.gap(px(5.))
			.rounded(px(4.))
			.cursor_pointer()
			.hover(|s| s.bg(rgb(pal().hover_bg)))
			.on_click(cx.listener(move |this, _, _, cx| {
				cx.stop_propagation();
				// A whole repository of the merged log is its scope, not
				// a path: the single-repository log.
				let root = this
					.log_is_merged()
					.then(|| this.repos.iter().find(|r| r.name == rel))
					.flatten()
					.map(|r| r.root.clone());
				match root {
					Some(root) => this.set_log_repos(vec![root], cx),
					None => this.toggle_log_path(rel.clone(), cx),
				}
			}))
			.child(if pick.expandable {
				div()
					.id(SharedString::from(exp_id.clone()))
					.relative()
					.flex()
					.rounded(px(3.))
					.hover(|s| s.bg(rgb(pal().divider)))
					.on_click(cx.listener(move |this, _, _, cx| {
						cx.stop_propagation();
						let open = &mut this.log_paths_expanded;
						match open.iter().position(|p| *p == exp_rel) {
							Some(i) => {
								open.remove(i);
							}
							None => {
								open.push(exp_rel.clone());
								this.load_picker_dir(&exp_rel, cx);
							}
						}
						cx.notify();
					}))
					.child(icon(
						if pick.expanded {
							Icon::ChevronDown
						} else {
							Icon::ChevronRight
						},
						12.,
					))
					.children(probe(log, exp_id))
					.into_any_element()
			} else {
				div().w(px(12.)).flex_shrink_0().into_any_element()
			})
			.child(checkbox(checked))
			.when_some(pick.is_dir, |d, dir| {
				d.child(icon(
					if dir {
						Icon::Folder
					} else {
						file_icon(&pick.rel)
					},
					14.,
				))
			})
			.child(fill_text(pick.name))
			.children(probe(log, id))
			.into_any_element()
	}

	/// IntelliJ's branches pane: search, HEAD, collapsible Local / Remote /
	/// Tags groups (no counts).
	pub(super) fn render_branches(&self, cx: &mut Context<Self>) -> AnyElement {
		let loc = self.locale;
		let log = &self.probes;
		let needle = self
			.branch_filter_input
			.read(cx)
			.text()
			.trim()
			.to_lowercase();
		let current = self
			.repo()
			.and_then(|r| r.summary.as_ref().ok())
			.and_then(|s| s.branch.clone());
		let mut rows: Vec<AnyElement> = Vec::new();
		let entry = |id: String,
		             label: String,
		             glyph: AnyElement,
		             indent: f32,
		             target: Option<String>,
		             active: bool,
		             cx: &mut Context<Self>| {
			div()
				.id(SharedString::from(id.clone()))
				.relative()
				.flex_shrink_0()
				.h(px(24.))
				.mx(px(4.))
				.pl(px(indent))
				.pr(px(6.))
				.flex()
				.items_center()
				.gap(px(6.))
				.rounded(px(4.))
				.cursor_pointer()
				.when(active, |d| d.bg(rgb(pal().selection_inactive_bg)))
				.when(!active, |d| d.hover(|s| s.bg(rgb(pal().hover_bg))))
				.tooltip(tip(label.clone()))
				.on_click(cx.listener(move |this, _, _, cx| {
					this.filter_by_ref(target.clone(), cx)
				}))
				.child(glyph)
				.child(clip_text(label).text_color(rgb(pal().text)))
				.children(probe(log, id))
				.into_any_element()
		};
		if (self.head_sha.is_some() || self.log_is_merged())
			&& needle.is_empty()
		{
			rows.push(entry(
				"ref:HEAD".into(),
				t("log_head_current", loc).to_string(),
				div().w(px(14.)).into_any_element(),
				10.,
				Some("HEAD".into()),
				self.active_ref_filter.as_deref() == Some("HEAD"),
				cx,
			));
		}
		for row in
			branch_rows(&self.refs, &needle, &self.branch_groups_collapsed, loc)
		{
			match row {
				BranchRow::Group {
					key,
					label,
					depth,
					collapsed,
				} => {
					let gid = format!("branch-group:{key}");
					rows.push(
						div()
							.id(SharedString::from(gid.clone()))
							.relative()
							.flex_shrink_0()
							.h(px(24.))
							.mx(px(4.))
							.pl(px(6. + depth as f32 * 16.))
							.pr(px(6.))
							.flex()
							.items_center()
							.gap(px(4.))
							.rounded(px(4.))
							.cursor_pointer()
							.hover(|s| s.bg(rgb(pal().hover_bg)))
							.on_click(cx.listener(move |this, _, _, cx| {
								let groups = &mut this.branch_groups_collapsed;
								match groups.iter().position(|k| *k == key) {
									Some(i) => {
										groups.remove(i);
									}
									None => groups.push(key.clone()),
								}
								cx.notify();
							}))
							.child(icon(
								if collapsed {
									Icon::ChevronRight
								} else {
									Icon::ChevronDown
								},
								12.,
							))
							.when(depth > 0, |d| {
								d.child(icon(Icon::Folder, 14.))
							})
							.child(clip_text(label).text_color(rgb(pal().text)))
							.children(probe(log, gid))
							.into_any_element(),
					);
				}
				BranchRow::Ref { name, label, depth } => {
					let is_current = name.strip_prefix("refs/heads/").is_some()
						&& current.as_deref()
							== name.strip_prefix("refs/heads/");
					let glyph = if name.starts_with("refs/tags/") {
						icon_tinted(Icon::Tag, 14., pal().ref_tag)
							.into_any_element()
					} else if is_current {
						icon_tinted(Icon::Head, 14., pal().ref_head)
							.into_any_element()
					} else {
						icon(Icon::Branch, 14.).into_any_element()
					};
					let active = self.active_ref_filter.as_deref()
						== Some(name.as_str());
					rows.push(entry(
						format!("ref:{name}"),
						label,
						glyph,
						8. + depth as f32 * 16.,
						Some(name),
						active,
						cx,
					));
				}
			}
		}
		div()
			.flex()
			.flex_col()
			.flex_shrink_0()
			.w(px(self.log_branches_w()))
			.h_full()
			.border_r_1()
			.border_color(rgb(pal().divider))
			.child(
				div()
					.flex_shrink_0()
					.h(px(32.))
					.px(px(6.))
					.flex()
					.items_center()
					.gap(px(4.))
					.child(icon(Icon::Search, 14.))
					.child(
						div()
							.id("branch-filter-input")
							.relative()
							.flex_1()
							.min_w_0()
							.child(self.branch_filter_input.clone())
							.children(probe(log, "branch-filter-input")),
					),
			)
			.child(
				div()
					.id("refs-scroll")
					.flex()
					.flex_col()
					.flex_1()
					.min_h_0()
					.overflow_y_scroll()
					.text_size(px(UI_TEXT))
					.pb(px(4.))
					.children(rows),
			)
			.into_any_element()
	}

	/// The thin icon toolbar at the log's left edge (IntelliJ's branches
	/// toolbar).
	pub(super) fn render_branch_toolbar(
		&self,
		cx: &mut Context<Self>,
	) -> AnyElement {
		let loc = self.locale;
		let log = &self.probes;
		let shown = self.log_branches_visible;
		div()
			.flex()
			.flex_col()
			.items_center()
			.flex_shrink_0()
			.w(px(32.))
			.h_full()
			.py(px(4.))
			.gap(px(2.))
			.border_r_1()
			.border_color(rgb(pal().divider))
			.child(
				log_icon_button(
					"branches-toggle",
					if shown {
						Icon::Back
					} else {
						Icon::ChevronRight
					},
					t(
						if shown {
							"tip_branches_hide"
						} else {
							"tip_branches_show"
						},
						loc,
					),
					true,
					false,
				)
				.on_click(cx.listener(|this, _, _, cx| {
					this.log_branches_visible = !this.log_branches_visible;
					app_log!("[APP:LOG_VIEW: branches]");
					cx.notify();
				}))
				.children(probe(log, "branches-toggle")),
			)
			.child(
				log_icon_button(
					"branches-expand-all",
					Icon::ExpandAll,
					t("tip_expand_all", loc),
					shown,
					false,
				)
				.when(shown, |d| {
					d.on_click(cx.listener(|this, _, _, cx| {
						this.branch_groups_collapsed.clear();
						cx.notify();
					}))
				})
				.children(probe(log, "branches-expand-all")),
			)
			.child(
				log_icon_button(
					"branches-collapse-all",
					Icon::CollapseAll,
					t("tip_collapse_all", loc),
					shown,
					false,
				)
				.when(shown, |d| {
					d.on_click(cx.listener(|this, _, _, cx| {
						this.branch_groups_collapsed =
							["refs_local", "refs_remote", "refs_tags"]
								.map(String::from)
								.to_vec();
						cx.notify();
					}))
				})
				.children(probe(log, "branches-collapse-all")),
			)
			.into_any_element()
	}
}

#[cfg(test)]
mod row_width_tests {
	use super::*;

	// A 24-lane gutter (the widest) and the widest combined label.
	const WIDE_GUTTER: f32 = 24. * 16. + 8.;

	fn cols(list_w: f32, gutter_w: f32, labels: bool, hash: bool) -> ListCols {
		list_cols(list_w, gutter_w, labels, hash)
	}

	#[test]
	fn roomy_rows_are_unchanged() {
		let c = cols(1200., 40., true, false);
		assert_eq!(
			c,
			ListCols {
				gutter: 40.,
				author: AUTHOR_W,
				date: DATE_W
			}
		);
		let w = row_widths(1200., c, 100., false);
		assert_eq!(w.gutter, 40.);
		assert_eq!(w.labels, 100.);
		assert_eq!(w.subject, 1200. - 8. - 24. - 40. - 238. - 106.);
		let c = cols(1200., 40., false, true);
		let w = row_widths(1200., c, 0., true);
		assert_eq!(w.subject, 1200. - 8. - 32. - 40. - 238. - 64.);
	}

	#[test]
	fn multi_repo_graph_keeps_a_subject() {
		// 574 and 433 are the lists a 1080 and a 900 window leave (measured
		// in the in-process tests `wide_multi_repo_log_keeps_a_subject_at_*`).
		for list_w in [433., 480., 574., 720., 900.] {
			for show_hash in [false, true] {
				// Known limit (see `list_cols`): the hash column costs 72px,
				// and a list this narrow with it is below what any squeeze
				// can save (433px with hash leaves a 97px subject).
				if show_hash && list_w < 520. {
					continue;
				}
				for has_labels in [false, true] {
					let c = cols(list_w, WIDE_GUTTER, has_labels, show_hash);
					for labels_w in [0., 60., 337.] {
						let w = row_widths(list_w, c, labels_w, show_hash);
						assert!(
							w.subject >= MIN_SUBJECT_W,
							"{list_w} {show_hash} {labels_w}: {c:?} {w:?}"
						);
						assert!(
							c.gutter <= WIDE_GUTTER && w.labels <= labels_w
						);
					}
				}
			}
		}
	}

	#[test]
	fn a_1080_window_keeps_ref_labels_and_a_subject() {
		// 574px is the list a 1080 window leaves, hash off, long labels.
		let c = cols(574., WIDE_GUTTER, true, false);
		let w = row_widths(574., c, 337., false);
		assert!(w.labels >= MIN_LABELS_W, "{c:?} {w:?}");
		assert!(w.subject >= MIN_SUBJECT_W, "{c:?} {w:?}");
	}

	#[test]
	fn a_narrow_list_squeezes_date_then_author_after_the_gutter() {
		// 433px (a 900 window): the gutter is at its floor, the date and the
		// author cells give the subject its minimum.
		let c = cols(433., WIDE_GUTTER, true, false);
		assert_eq!(c.gutter, MIN_GUTTER_W);
		assert!(c.date < DATE_W && c.date >= MIN_DATE_W);
		assert!(c.author >= MIN_AUTHOR_W);
		// The labels are dropped here anyway (the deficit is more than the
		// date and author can give), so the author is not squeezed for them.
		assert_eq!(c.author, 81.);
		let w = row_widths(433., c, 337., false);
		assert_eq!(w.labels, 0.);
		assert!(w.subject >= MIN_SUBJECT_W, "{w:?}");
		// Only the date gives way while it alone is enough.
		let c = cols(FLOOR_W - 10., WIDE_GUTTER, false, false);
		assert_eq!(c.author, AUTHOR_W);
		assert!(c.date < DATE_W);
	}

	// Below this list width the gutter floor plus full author and date
	// leave less than MIN_SUBJECT_W.
	const FLOOR_W: f32 =
		8. + 24. + AUTHOR_W + DATE_W + MIN_GUTTER_W + MIN_SUBJECT_W;

	#[test]
	fn below_the_squeeze_the_subject_takes_the_rest() {
		let c = cols(300., WIDE_GUTTER, true, false);
		assert_eq!(
			(c.gutter, c.author, c.date),
			(MIN_GUTTER_W, MIN_AUTHOR_W, MIN_DATE_W)
		);
		let w = row_widths(300., c, 337., false);
		assert_eq!(w.labels, 0.);
		assert_eq!(w.subject, 300. - 8. - 24. - 72. - 72. - 88.);
	}

	#[test]
	fn a_list_without_labels_keeps_more_gutter() {
		let with = cols(574., WIDE_GUTTER, true, false);
		let without = cols(574., WIDE_GUTTER, false, false);
		assert!(without.gutter > with.gutter);
		assert_eq!(without.gutter, 574. - 8. - 24. - 238. - MIN_SUBJECT_W);
	}

	#[test]
	fn every_row_of_a_list_shares_one_set_of_columns() {
		// Dropping the labels gives the subject the room, not the gutter.
		let c = cols(574., WIDE_GUTTER, true, false);
		let (none, dropped) = (
			row_widths(560., c, 0., false),
			row_widths(560., c, 337., false),
		);
		assert_eq!(dropped.gutter, none.gutter);
		assert_eq!(dropped.labels, 0.);
		assert_eq!(dropped.subject, none.subject);
	}

	#[test]
	fn labels_shrink_before_the_gutter() {
		let c = cols(800., 120., true, false);
		assert_eq!(c.gutter, 120.);
		let w = row_widths(800., c, 337., false);
		assert!(w.labels < 337. && w.labels >= 80.);
		assert_eq!(w.subject, MIN_SUBJECT_W);
	}

	#[test]
	fn tiny_windows_never_go_negative() {
		let c = cols(100., WIDE_GUTTER, true, true);
		let w = row_widths(100., c, 337., true);
		assert!(w.subject >= 0. && w.labels == 0.);
	}
}
