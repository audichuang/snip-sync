//! Git Log chrome: filter chips, dropdown menus and the branches pane.

use super::fit::{
	fit_row, floor_px, natural_px, record_dropped, role_measure, text_width_in,
	Claim, FitCell, FitId, FitRole, Part, RowFit, CHIP_CHROME_W, FIT_MARGIN,
	LABEL_ICON_GAP,
};
use super::*;

// ───────────────────────── log helpers (IJ-2a) ─────────────────────────

/// Dropdowns of the log: the filter chips and the More (⋮) button.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LogMenu {
	Repo,
	Branch,
	User,
	Date,
	More,
	FilterMore,
}

impl LogMenu {
	pub(super) fn key(self) -> &'static str {
		match self {
			LogMenu::Repo => "repo",
			LogMenu::Branch => "branch",
			LogMenu::User => "user",
			LogMenu::Date => "date",
			LogMenu::More => "more",
			LogMenu::FilterMore => "more",
		}
	}
}

/// What the filter bar remembers between frames. Its chips are laid out by
/// [`split_chips`] for the width of the container it measured last frame; the
/// container's canvas draws one more frame when this one's width differs
/// ([`refresh_if_stale`], at most 3 times for a new width).
#[derive(Clone, Default)]
pub struct LogFilterLayout {
	/// The chips' container, as drawn (0 until the first frame).
	pub container_w: std::rc::Rc<std::cell::Cell<f32>>,
	/// The width the chips were split for in this frame.
	pub used_w: std::rc::Rc<std::cell::Cell<f32>>,
	/// Which chips the last frame drew on the bar (bit `i` is
	/// `ALL_LOG_FILTER_MENUS[i]`).
	pub last_mask: std::rc::Rc<std::cell::Cell<u8>>,
	/// Redraws asked for since the container's width last changed.
	pub refresh_count: std::rc::Rc<std::cell::Cell<u8>>,
}

pub(crate) const ALL_LOG_FILTER_MENUS: [LogMenu; 4] =
	[LogMenu::Repo, LogMenu::Branch, LogMenu::User, LogMenu::Date];
pub(crate) const LOG_FILTER_GAP: f32 = 4.0;
/// The widest chip text; a longer one is cut with an ellipsis.
pub(crate) const CHIP_TEXT_MAX_W: f32 = 180.0;
/// An overflow chip's padding (4 + 4), the gap before its icon, the icon;
/// a hidden filter with a value adds a 5px dot and its gap in front of the
/// icon, and a 1px border on each side.
const MORE_PAD_W: f32 = 8.;
const MORE_ICON_W: f32 = 10.;
const MORE_DOT_W: f32 = 5. + 2.;
const MORE_BORDER_W: f32 = 2.;

/// The roles of the filter bar's chips, by menu.
pub(super) fn chip_role(menu: LogMenu) -> FitRole {
	match menu {
		LogMenu::Repo => FitRole::ChipRepo,
		LogMenu::Branch => FitRole::ChipBranch,
		LogMenu::User => FitRole::ChipUser,
		LogMenu::Date => FitRole::ChipDate,
		LogMenu::More | LogMenu::FilterMore => FitRole::ChipMore,
	}
}

/// The chips and the overflow chip, as the solver places them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ChipPart {
	Chip(LogMenu),
	More,
}

/// What a filter chip asks of the split.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct ChipInput {
	pub natural: f32,
	pub floor: f32,
	pub valued: bool,
}

/// Which chips stay on a bar `avail` px wide (a bit mask over
/// `ALL_LOG_FILTER_MENUS`) and the width each gets. Chips that fit whole all
/// stay. Else the overflow chip comes first, then every chip with a value at
/// its floor, the shorter text first, so a short value is whole before a
/// long one gets more; then the chips with a value grow, the shorter first;
/// chips without a value come last, and are the first to go into the
/// overflow chip. A hidden value marks the overflow chip with a dot; when
/// nothing with a value is hidden it needs none, and the split is made again
/// without.
pub(super) fn split_chips(
	chips: &[ChipInput; 4],
	more: f32,
	more_with_dot: f32,
	avail: f32,
) -> (u8, RowFit<ChipPart>) {
	// Every part pays the gap before it, the first one too: widen the bar
	// by one gap.
	let avail = avail + LOG_FILTER_GAP;
	let parts = |more_w: f32| -> Vec<Part<ChipPart>> {
		chips
			.iter()
			.enumerate()
			.map(|(i, c)| {
				Part::new(
					ChipPart::Chip(ALL_LOG_FILTER_MENUS[i]),
					c.natural,
					c.floor,
					LOG_FILTER_GAP,
				)
			})
			.chain(std::iter::once(Part::new(
				ChipPart::More,
				more_w,
				more_w,
				LOG_FILTER_GAP,
			)))
			.collect()
	};
	let chip = |i: usize| ChipPart::Chip(ALL_LOG_FILTER_MENUS[i]);
	let whole: f32 = chips.iter().map(|c| c.natural + LOG_FILTER_GAP).sum();
	if whole <= avail + 1e-3 {
		let claims: Vec<Claim<ChipPart>> = (0..4)
			.map(|i| Claim::Grow(chip(i), chips[i].natural))
			.collect();
		return (0b1111, fit_row(avail, &parts(more), &claims));
	}
	let pass = |more_w: f32| {
		let mut valued: Vec<usize> =
			(0..4).filter(|&i| chips[i].valued).collect();
		valued.sort_by(|&a, &b| chips[a].natural.total_cmp(&chips[b].natural));
		let mut claims = vec![Claim::Atomic(vec![ChipPart::More])];
		claims.extend(valued.iter().map(|&i| Claim::Atomic(vec![chip(i)])));
		claims.push(Claim::ShortestFirst(
			valued.iter().map(|&i| chip(i)).collect(),
		));
		claims.extend(
			(0..4)
				.filter(|&i| !chips[i].valued)
				.map(|i| Claim::Atomic(vec![chip(i)])),
		);
		let fit = fit_row(avail, &parts(more_w), &claims);
		// Without room for the overflow chip nothing goes on the bar.
		let mask = if fit.shown(ChipPart::More) {
			(0..4)
				.filter(|&i| fit.shown(chip(i)))
				.fold(0u8, |m, i| m | (1 << i))
		} else {
			0
		};
		(mask, fit)
	};
	let any_valued = chips.iter().any(|c| c.valued);
	let (mask, fit) = pass(if any_valued { more_with_dot } else { more });
	let hides_valued = (0..4).any(|i| mask & (1 << i) == 0 && chips[i].valued);
	if any_valued && !hides_valued {
		return pass(more);
	}
	(mask, fit)
}

/// The chips of `mask` and the rest, each in their usual order.
pub(super) fn split_by_mask(mask: u8) -> (Vec<LogMenu>, Vec<LogMenu>) {
	(0..4).fold((Vec::new(), Vec::new()), |(mut on, mut off), i| {
		if mask & (1 << i) != 0 {
			on.push(ALL_LOG_FILTER_MENUS[i])
		} else {
			off.push(ALL_LOG_FILTER_MENUS[i])
		}
		(on, off)
	})
}

/// The filter bar of one frame: the split and the widths.
pub(super) struct ChipFit {
	pub mask: u8,
	pub fit: RowFit<ChipPart>,
	/// The overflow chip's width without and with the dot of a hidden value.
	pub more: f32,
	pub more_dot: f32,
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
	/// A ref (full name) shown as `label`, with a muted `repo` suffix when
	/// the log merges several repositories (the repo name, or the count
	/// when the same ref exists in several of them).
	Ref {
		name: String,
		label: String,
		depth: usize,
		repo: Option<String>,
	},
}

/// The branches pane like IntelliJ's: Local, Remote grouped by remote
/// name, Tags; names matching `needle` (lowercase) only; at most
/// [`MAX_BRANCH_ROWS`] refs per group; `<remote>/HEAD` is not listed.
pub(super) fn branch_rows(
	refs: &[snip_core::browser::GitReference],
	feeds: &[String],
	needle: &str,
	collapsed: &[String],
	loc: Locale,
) -> Vec<BranchRow> {
	let is_collapsed = |k: &str| collapsed.iter().any(|c| c == k);
	// Which repositories (feed indices) hold each ref, before the dedup.
	let mut owners: HashMap<&str, Vec<usize>> = HashMap::new();
	for r in refs {
		if let Some(feed) = crate::multi_log::split_id(&r.sha).1 {
			let v = owners.entry(r.name.as_str()).or_default();
			if !v.contains(&feed) {
				v.push(feed);
			}
		}
	}
	let repo_of = |name: &str| -> Option<String> {
		if feeds.is_empty() {
			return None;
		}
		match owners.get(name).map(Vec::as_slice) {
			Some([one]) => feeds.get(*one).cloned(),
			Some(many) if many.len() > 1 => {
				Some(tf("branch_repo_count", loc, &[many.len()]))
			}
			_ => None,
		}
	};
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
					repo: repo_of(name),
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
							repo: repo_of(name),
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
	feeds: &[String],
	needle: &str,
	open: &[String],
	loc: Locale,
) -> Vec<BranchRow> {
	if !needle.is_empty() {
		return branch_rows(refs, feeds, needle, &[], loc)
			.into_iter()
			.filter_map(|r| match r {
				BranchRow::Ref { name, repo, .. } => Some(BranchRow::Ref {
					label: short_ref(&name).to_string(),
					name,
					depth: 0,
					repo,
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
	branch_rows(refs, feeds, "", &closed, loc)
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

/// The numbers of a Log row's layout. The least a text keeps is not here but
/// in the rule table (`FitRole::rule`, ADR 0005); these are the fixed parts
/// of a row and the widths a column grows to when there is room.
pub(super) mod row_policy {
	/// The row's right padding.
	pub const ROW_PAD_R: f32 = 8.;
	/// The gap before the subject and before the author, date and hash.
	pub const CELL_GAP: f32 = 8.;
	/// The gap between two ref labels.
	pub const LABEL_GAP: f32 = 8.;
	/// The gap before a collapsed chip and before the first ref label.
	pub const SUBJECT_LABEL_GAP: f32 = 6.;
	pub const HASH_W: f32 = 64.;
	/// The narrowest the graph gutter is clipped to before the other
	/// columns give way.
	pub const MIN_GUTTER_W: f32 = 72.;
	pub const AUTHOR_W: f32 = 120.;
	pub const DATE_W: f32 = 118.;
	/// The subject's comfortable width: columns grow past their floors
	/// only once it has this much.
	pub const SUBJECT_COMFORT_W: f32 = 160.;
	/// A ref label is never wider than this.
	pub const LABEL_MAX_W: f32 = 320.;
}
use row_policy::*;

/// The list's width until it reports its own (the first frame).
pub(super) const NOMINAL_LIST_W: f32 = 800.;

/// What the list's columns are decided from, other than its width: the worst
/// row of each kind ([`fit_list`]).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ListFitParts {
	pub date_need: f32,
	pub author_floor: Option<f32>,
	pub subject_floor_bound: f32,
	pub collapsed: Option<f32>,
	pub labels: Vec<LabelPart>,
}

impl ListFitParts {
	pub(super) fn input(
		&self,
		list_w: f32,
		hash: bool,
		gutter: f32,
	) -> ListFitInput {
		ListFitInput {
			list_w,
			hash,
			gutter,
			date_need: self.date_need,
			author_floor: self.author_floor,
			subject_floor_bound: self.subject_floor_bound,
			collapsed: self.collapsed,
			labels: self.labels.clone(),
		}
	}
}

/// What the Log keeps between frames to place its rows.
#[derive(Default)]
pub struct LogFit {
	/// The log panel's width: the side panes and the search box read it.
	pub(super) panel: StaleWidth,
	/// The list's width, which its columns are decided from.
	pub(super) list: StaleWidth,
	/// [`ListFitParts`] for a fingerprint of everything they read.
	pub(super) cache:
		std::cell::RefCell<Option<(u64, std::rc::Rc<ListFitParts>)>>,
}

/// The cells of a Log row the solver places, left to right.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum LogCell {
	Gutter,
	Subject,
	Collapsed,
	BranchLabel,
	TagLabel,
	RefMore,
	Author,
	Date,
	Hash,
}

impl LogCell {
	/// The graph gutter is not text: it has no role.
	pub(super) fn role(self) -> Option<FitRole> {
		match self {
			LogCell::Gutter => None,
			LogCell::Subject => Some(FitRole::Subject),
			LogCell::Collapsed => Some(FitRole::Collapsed),
			LogCell::BranchLabel => Some(FitRole::BranchLabel),
			LogCell::TagLabel => Some(FitRole::TagLabel),
			LogCell::RefMore => Some(FitRole::RefMore),
			LogCell::Author => Some(FitRole::Author),
			LogCell::Date => Some(FitRole::Date),
			LogCell::Hash => Some(FitRole::Hash),
		}
	}

	const ALL: [LogCell; 9] = [
		LogCell::Gutter,
		LogCell::Subject,
		LogCell::Collapsed,
		LogCell::BranchLabel,
		LogCell::TagLabel,
		LogCell::RefMore,
		LogCell::Author,
		LogCell::Date,
		LogCell::Hash,
	];
}

/// One ref label (or the `+N`) of a row, with the widths it may have.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct LabelPart {
	pub cell: LogCell,
	pub natural: f32,
	pub floor: f32,
}

/// The widths one list decides for all its rows, so columns line up from row
/// to row. `None` is a column left out of every row.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct ListCols {
	pub gutter: f32,
	pub author: Option<f32>,
	pub date: Option<f32>,
}

/// What the list's columns depend on beyond its width: the worst row of each
/// kind (see [`fit_list`]).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ListFitInput {
	pub list_w: f32,
	pub hash: bool,
	/// The graph gutter's natural width.
	pub gutter: f32,
	/// The widest date string of the locale, whole.
	pub date_need: f32,
	/// The most any author name needs at its readable floor; `None` with
	/// no row.
	pub author_floor: Option<f32>,
	/// A subject's floor, for the widest letters ("漢" or "W").
	pub subject_floor_bound: f32,
	/// The widest collapsed chip.
	pub collapsed: Option<f32>,
	/// The ref labels of the row whose floors add up to the most.
	pub labels: Vec<LabelPart>,
}

/// What a row asks of the solver beyond the list's columns.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct RowFitInput {
	pub hash: bool,
	pub subject_natural: f32,
	pub subject_floor: f32,
	pub collapsed: Option<f32>,
	pub labels: Vec<LabelPart>,
}

fn label_parts(labels: &[LabelPart]) -> Vec<Part<LogCell>> {
	labels
		.iter()
		.enumerate()
		.map(|(n, l)| {
			let gap = if n == 0 { SUBJECT_LABEL_GAP } else { LABEL_GAP };
			Part::new(l.cell, l.natural, l.floor, gap)
		})
		.collect()
}

/// Every cell a row can have as a part, those without text at width 0.
fn all_parts(present: Vec<Part<LogCell>>) -> Vec<Part<LogCell>> {
	LogCell::ALL
		.into_iter()
		.map(|cell| {
			present
				.iter()
				.find(|p| p.id == cell)
				.copied()
				.unwrap_or(Part::new(cell, 0., 0., 0.))
		})
		.collect()
}

/// The graph gutter, author and date widths of every row of a list, decided
/// once from the worst row of each kind. One order for every shortfall (ADR
/// 0005): the date is whole before anything else (or the column is gone, on
/// a list under about 250px); the subject gets its readable floor; the ref
/// labels' floors, then the collapsed chip, come before the author column,
/// so the author goes first and then, when even the labels' floors do not
/// fit, the labels are dropped as a group. The collapsed chip only explains
/// the `+` in the gutter, so it never costs a row its labels: it is the
/// first of them to go. Then the labels grow, the subject to its comfortable
/// width, the author, the date and the gutter, and the subject takes the
/// rest.
pub(super) fn fit_list(i: &ListFitInput) -> ListCols {
	let avail = i.list_w - ROW_PAD_R;
	let date = Part::new(
		LogCell::Date,
		i.date_need.max(DATE_W),
		i.date_need,
		CELL_GAP,
	);
	let mut present = vec![
		Part::new(LogCell::Gutter, i.gutter, 0., 0.),
		Part::new(
			LogCell::Subject,
			i.list_w.max(i.subject_floor_bound),
			i.subject_floor_bound,
			CELL_GAP,
		),
		date,
	];
	if i.hash {
		present.push(Part::new(LogCell::Hash, HASH_W, HASH_W, CELL_GAP));
	}
	if let Some(w) = i.collapsed {
		present.push(Part::new(LogCell::Collapsed, w, w, SUBJECT_LABEL_GAP));
	}
	present.extend(label_parts(&i.labels));
	if let Some(floor) = i.author_floor {
		present.push(Part::new(
			LogCell::Author,
			AUTHOR_W.max(floor),
			floor,
			CELL_GAP,
		));
	}
	let label_cells: Vec<LogCell> = i.labels.iter().map(|l| l.cell).collect();
	let kinds: Vec<LogCell> = label_cells
		.iter()
		.copied()
		.filter(|c| *c != LogCell::RefMore)
		.collect();
	let mut claims = Vec::new();
	if i.hash {
		claims.push(Claim::Atomic(vec![LogCell::Hash]));
	}
	claims.push(Claim::Atomic(vec![LogCell::Date]));
	claims.push(Claim::Grow(LogCell::Gutter, i.gutter.min(MIN_GUTTER_W)));
	claims.push(Claim::Grow(LogCell::Subject, i.subject_floor_bound));
	if i.collapsed.is_some() {
		claims.push(Claim::Atomic(vec![LogCell::Collapsed]));
	}
	if !label_cells.is_empty() {
		claims.push(Claim::Atomic(label_cells));
	}
	if i.author_floor.is_some() {
		claims.push(Claim::Atomic(vec![LogCell::Author]));
	}
	if !kinds.is_empty() {
		claims.push(Claim::ShortestFirst(kinds));
	}
	claims.push(Claim::Grow(LogCell::Subject, SUBJECT_COMFORT_W));
	if i.author_floor.is_some() {
		claims.push(Claim::Grow(LogCell::Author, AUTHOR_W));
	}
	claims.push(Claim::Grow(LogCell::Date, DATE_W));
	claims.push(Claim::Grow(LogCell::Gutter, i.gutter));
	claims.push(Claim::Rest(LogCell::Subject));
	let fit = fit_row(avail, &all_parts(present), &claims);
	ListCols {
		gutter: fit.width(LogCell::Gutter),
		author: (i.author_floor.is_some() && fit.shown(LogCell::Author))
			.then(|| fit.width(LogCell::Author)),
		date: fit.shown(LogCell::Date).then(|| fit.width(LogCell::Date)),
	}
}

/// How one row of a `list_w` wide list divides its width, in the list's
/// columns. The columns come first, the same in every row; then the subject's
/// floor, the ref labels (all or none), the collapsed chip (it gives way
/// before the labels do), the shorter label whole before the longer, and the
/// subject takes what is left. A list that
/// had room for its worst row's floors has it for every row's.
pub(super) fn fit_log_row(
	list_w: f32,
	cols: &ListCols,
	row: &RowFitInput,
) -> RowFit<LogCell> {
	let avail = list_w - ROW_PAD_R;
	let mut present = vec![
		Part::new(LogCell::Gutter, cols.gutter, cols.gutter, 0.),
		Part::new(
			LogCell::Subject,
			row.subject_natural,
			row.subject_floor,
			CELL_GAP,
		),
	];
	if let Some(w) = cols.date {
		present.push(Part::new(LogCell::Date, w, w, CELL_GAP));
	}
	if let Some(w) = cols.author {
		present.push(Part::new(LogCell::Author, w, w, CELL_GAP));
	}
	if row.hash {
		present.push(Part::new(LogCell::Hash, HASH_W, HASH_W, CELL_GAP));
	}
	if let Some(w) = row.collapsed {
		present.push(Part::new(LogCell::Collapsed, w, w, SUBJECT_LABEL_GAP));
	}
	present.extend(label_parts(&row.labels));
	let label_cells: Vec<LogCell> = row.labels.iter().map(|l| l.cell).collect();
	let kinds: Vec<LogCell> = label_cells
		.iter()
		.copied()
		.filter(|c| *c != LogCell::RefMore)
		.collect();
	let mut claims = Vec::new();
	if row.hash {
		claims.push(Claim::Atomic(vec![LogCell::Hash]));
	}
	if cols.date.is_some() {
		claims.push(Claim::Atomic(vec![LogCell::Date]));
	}
	claims.push(Claim::Atomic(vec![LogCell::Gutter]));
	if cols.author.is_some() {
		claims.push(Claim::Atomic(vec![LogCell::Author]));
	}
	claims.push(Claim::Grow(LogCell::Subject, row.subject_floor));
	if row.collapsed.is_some() {
		claims.push(Claim::Atomic(vec![LogCell::Collapsed]));
	}
	if !label_cells.is_empty() {
		claims.push(Claim::Atomic(label_cells));
	}
	if !kinds.is_empty() {
		claims.push(Claim::ShortestFirst(kinds));
	}
	claims.push(Claim::Rest(LogCell::Subject));
	fit_row(avail, &all_parts(present), &claims)
}

/// The width the date cell needs for the longest date string of `loc`
/// (a double-digit month and day, the latest time, and the today and
/// yesterday forms), measured with `measure`.
pub(super) fn date_need_w(loc: Locale, measure: &dyn Fn(&str) -> f32) -> f32 {
	let time = "23:59";
	[
		log_date_in(
			"2026-12-28T23:59:00Z",
			loc,
			chrono::DateTime::parse_from_rfc3339("2027-06-01T00:00:00Z")
				.unwrap()
				.with_timezone(&chrono::Utc),
			&chrono::Utc,
		),
		tf("log_today", loc, &[&time]),
		tf("log_yesterday", loc, &[&time]),
	]
	.iter()
	.map(|s| measure(s))
	.fold(0., f32::max)
}

/// A ref row of the Branches menu: its name and repo, the indent of its
/// depth, whether it is the active filter and the panel's width.
pub(super) struct MenuRow {
	pub name: String,
	pub label: String,
	pub depth: usize,
	pub repo: Option<String>,
	pub checked: bool,
	pub panel_w: f32,
}

/// A menu panel is between these wide; its border (1+1) and padding (4+4)
/// are around the rows, an item's own padding (8+8), check box (14) and the
/// gap after it (6) around its text, and a nested row is indented 14px a
/// level.
const MENU_MIN_W: f32 = 200.;
const MENU_MAX_W: f32 = 360.;
const MENU_PANEL_CHROME: f32 = 10.;
const MENU_ITEM_CHROME: f32 = 16. + 14. + 6.;
const MENU_INDENT_W: f32 = 14.;

/// One ref row of the branches pane.
pub(super) struct PaneRow {
	pub id: String,
	pub label: String,
	pub glyph: AnyElement,
	pub indent: f32,
	pub target: Option<String>,
	pub active: bool,
	pub repo: Option<String>,
}

/// The two texts of a pane or menu row that share its width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RefRowCell {
	Name,
	Suffix,
}

/// Around a pane row's text: the pane's right border, a row's margin on both
/// sides and its right padding, the glyph, the gap before the name and
/// before the repo.
const PANE_BORDER_W: f32 = 1.;
const PANE_ROW_MARGIN: f32 = 4.;
const PANE_ROW_PAD_R: f32 = 6.;
const PANE_GLYPH_W: f32 = 14.;
const REF_ROW_GAP: f32 = 6.;

/// `refs/heads/main` → `main`, `refs/remotes/origin/x` → `origin/x`.
pub(super) fn short_ref(name: &str) -> &str {
	["refs/heads/", "refs/remotes/", "refs/tags/"]
		.iter()
		.find_map(|p| name.strip_prefix(p))
		.unwrap_or(name)
}

/// Width of `s` laid out in the UI font at `size`, from the text system
/// (shaped lines are cached per frame, so a redraw does not reshape).
pub(crate) fn text_width(window: &Window, s: &str, size: f32) -> f32 {
	text_width_in(window, UI_FONT, s, size)
}

/// [`log_date_in`] in the viewer's time zone, now.
pub(crate) fn log_date(iso: &str, loc: Locale) -> String {
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
		.debug_selector(move || id.to_string())
		.relative()
		.flex_shrink_0()
		.size(px(26.))
		.flex()
		.items_center()
		.justify_center()
		.rounded(px(4.))
		.border_1()
		.border_color(transparent_black())
		.tip(tooltip)
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

/// The glyph of a ref label: HEAD / the checked-out branch, a tag, or a branch.
pub(super) fn label_icon_kind(l: &graph_view::RefLabel) -> Icon {
	if l.current {
		Icon::Head
	} else if l.tag {
		Icon::Tag
	} else {
		Icon::Branch
	}
}

/// The coloured label glyph of a ref.
pub(super) fn label_icon(l: &graph_view::RefLabel) -> gpui::Svg {
	icon_tinted(label_icon_kind(l), 14., l.color)
}

/// One ref label (or the `+N` badge) of a row, as data: the solver gives it
/// a width and the row draws it.
#[derive(Clone, Debug)]
pub(super) struct RefDatum {
	pub cell: LogCell,
	pub text: String,
	/// The label's icon and colour; `None` for the `+N`.
	pub label: Option<graph_view::RefLabel>,
	pub tooltip: String,
}

/// A row's ref labels, after the subject: one label for its branches and one
/// for its tags, each with its icon, and the names of at most two badges
/// per label, the rest folded into `+N`.
pub(super) fn ref_label_data(
	refs: &[snip_core::graph::RefInfo],
	current_branch: Option<&str>,
) -> Vec<RefDatum> {
	let (shown, hidden) = graph_view::visible_refs(refs, current_branch);
	let mut out = Vec::new();
	for l in graph_view::kind_labels(&shown, current_branch) {
		let tooltip = shown
			.iter()
			.filter(|b| {
				matches!(b.primary.kind, snip_core::graph::RefKind::Tag)
					== l.tag
			})
			.flat_map(|b| {
				std::iter::once(b.primary).chain(b.merged.iter().copied())
			})
			.map(|i| graph_view::format_ref_badge(i).0)
			.collect::<Vec<_>>()
			.join("\n");
		out.push(RefDatum {
			cell: if l.tag {
				LogCell::TagLabel
			} else {
				LogCell::BranchLabel
			},
			text: l.text.clone(),
			label: Some(l),
			tooltip,
		});
	}
	if hidden > 0 {
		let all = refs
			.iter()
			.map(|i| graph_view::format_ref_badge(i).0)
			.collect::<Vec<_>>()
			.join("\n");
		out.push(RefDatum {
			cell: LogCell::RefMore,
			text: format!("+{hidden}"),
			label: None,
			tooltip: all,
		});
	}
	out
}

/// What the solver needs of a ref label: it is never wider than
/// [`LABEL_MAX_W`] (the text beyond is cut with an ellipsis).
pub(super) fn label_part(window: &Window, d: &RefDatum) -> LabelPart {
	let role = d.cell.role().expect("a label cell has a role");
	let measure = role_measure(window, role);
	let natural = natural_px(role, &d.text, &measure).min(LABEL_MAX_W);
	LabelPart {
		cell: d.cell,
		natural,
		floor: floor_px(role, &d.text, 0, &measure).min(natural),
	}
}

impl WorkbenchModel {
	// ───────────────────────── git log ─────────────────────────

	// ───────────────────────── log (IJ-2a) ─────────────────────────

	/// The chip's text, and where its key ends.
	fn log_chip_text(label: &str, value: &Option<String>) -> (String, usize) {
		match value {
			Some(v) => (format!("{label}: {v}"), label.len() + 2),
			None => (label.to_string(), 0),
		}
	}

	/// Which chips stay on the bar, at what width: [`split_chips`] for the
	/// width the bar's container had last frame. Prints the chips'
	/// `LOG_CHIPS` line when the split changed.
	pub(super) fn log_chip_fit(&self, window: &Window) -> ChipFit {
		let loc = self.locale;
		let layout = &self.log_filter_layout;
		let inputs: [ChipInput; 4] = std::array::from_fn(|i| {
			let menu = ALL_LOG_FILTER_MENUS[i];
			let (label, value) = self.log_menu_value(menu, loc);
			let role = chip_role(menu);
			let m = role_measure(window, role);
			let (text, key_end) = Self::log_chip_text(&label, &value);
			let natural =
				CHIP_CHROME_W + m(&text).min(CHIP_TEXT_MAX_W) + FIT_MARGIN;
			let valued = value.is_some();
			ChipInput {
				natural,
				floor: if valued {
					floor_px(role, &text, key_end, &m).min(natural)
				} else {
					natural
				},
				valued,
			}
		});
		let more = MORE_PAD_W
			+ LABEL_ICON_GAP
			+ role_measure(window, FitRole::ChipMore)(t("log_chip_more", loc))
			+ MORE_ICON_W
			+ FIT_MARGIN;
		let more_dot = more + MORE_DOT_W + MORE_BORDER_W;
		let avail = self.log_chips_available_width();
		layout.used_w.set(avail);
		let (mask, fit) = split_chips(&inputs, more, more_dot, avail);
		if layout.last_mask.replace(mask) != mask {
			let w: Vec<String> = ALL_LOG_FILTER_MENUS
				.iter()
				.map(|m| format!("{:.0}", fit.width(ChipPart::Chip(*m))))
				.collect();
			app_log!(
				"[APP:LOG_CHIPS: avail={avail:.0} widths={} more={:.0} shown={mask:04b}]",
				w.join(","),
				fit.width(ChipPart::More)
			);
		}
		ChipFit {
			mask,
			fit,
			more,
			more_dot,
		}
	}

	/// Filter chip of the log's filter bar: `Name▾`, or `Name: value ✕`.
	pub(super) fn log_chip(
		&self,
		menu: LogMenu,
		label: &str,
		value: Option<String>,
		chips: &ChipFit,
		window: &Window,
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
		let open = self.overlays.log_menu() == Some(menu);
		let active = value.is_some();
		let (text, key_end) = Self::log_chip_text(label, &value);
		let trailing = if active {
			div()
				.id(SharedString::from(clear_id.clone()))
				.relative()
				.flex()
				.rounded(px(3.))
				.hover(|s| s.bg(rgb(pal().divider)))
				.tip(t("log_clear_filter", self.locale))
				.on_click(cx.listener(move |this, _, _, cx| {
					cx.stop_propagation();
					this.clear_log_chip(menu, cx);
				}))
				.child(icon(Icon::Close, 12.))
				.children(probe(log, clear_id))
				.into_any_element()
		} else {
			icon(Icon::ChevronDown, 12.).into_any_element()
		};
		// FIT-ROW-BEGIN log_filter_chips
		FitCell::new(FitId::new(id.clone(), chip_role(menu)), text)
			.key_end(key_end)
			.selector(id.clone())
			.pad(6.)
			.trailing(trailing)
			.solved(&chips.fit, ChipPart::Chip(menu))
			.render(&self.probes, window)
			.id(SharedString::from(id.clone()))
			.h(px(24.))
			.rounded(px(4.))
			.cursor_pointer()
			.text_color(rgb(pal().text))
			.when(open, |d| d.bg(rgb(pal().hover_bg)))
			.hover(|s| s.bg(rgb(pal().hover_bg)))
			.on_click(cx.listener(move |this, _, window, cx| {
				this.toggle_log_menu(menu, TriggerId::LogChip(menu), window, cx)
			}))
			.child(self.overlay_trigger_slot(TriggerId::LogChip(menu)))
			.when(open, |d| d.child(self.log_menu_panel(menu, window, cx)))
			.children(probe(log, id))
			.into_any_element()
		// FIT-ROW-END
	}

	/// Overflow chip collecting filters that do not fit in a narrow window.
	pub(super) fn log_chip_more(
		&self,
		hidden_menus: Vec<LogMenu>,
		chips: &ChipFit,
		window: &Window,
		cx: &mut Context<Self>,
	) -> AnyElement {
		let loc = self.locale;
		let log = &self.probes;
		let id = "log-filter-more".to_string();
		let active = hidden_menus
			.iter()
			.any(|m| self.log_menu_value(*m, loc).1.is_some());
		let open_menu = match self.overlays.log_menu() {
			Some(LogMenu::FilterMore) => Some(LogMenu::FilterMore),
			Some(m) if hidden_menus.contains(&m) => Some(m),
			_ => None,
		};
		let open = open_menu.is_some();
		// It is drawn whole even where the split found no room for it.
		let width = if chips.fit.shown(ChipPart::More) {
			chips.fit.width(ChipPart::More)
		} else if active {
			chips.more_dot
		} else {
			chips.more
		};
		let trailing = div()
			.flex()
			.items_center()
			.gap(px(2.))
			.when(active, |d| {
				d.child(
					div().size(px(5.)).rounded_full().bg(rgb(pal().focus_ring)),
				)
			})
			.child(icon(Icon::ChevronDown, MORE_ICON_W));
		FitCell::new(
			FitId::new("log-filter-more", FitRole::ChipMore),
			t("log_chip_more", loc),
		)
		.selector(id.clone())
		.pad(4.)
		.trailing(trailing)
		.fixed_width(width)
		.render(&self.probes, window)
		.id(SharedString::from(id.clone()))
		.h(px(24.))
		.rounded(px(4.))
		.cursor_pointer()
		.text_color(rgb(pal().text))
		.when(open, |d| d.bg(rgb(pal().hover_bg)))
		.when(active, |d| d.border_1().border_color(rgb(pal().focus_ring)))
		.hover(|s| s.bg(rgb(pal().hover_bg)))
		.on_click(cx.listener(move |this, _, window, cx| {
			// The sub-menu it opened last is closed by it too.
			this.toggle_log_menu(
				LogMenu::FilterMore,
				TriggerId::LogFilterMore,
				window,
				cx,
			)
		}))
		.child(self.overlay_trigger_slot(TriggerId::LogFilterMore))
		.when_some(open_menu, |d, m| {
			d.child(self.log_menu_panel(m, window, cx))
		})
		.children(probe(log, id))
		.into_any_element()
	}

	pub(super) fn log_branch_value(&self) -> Option<String> {
		self.active_ref_filter
			.as_deref()
			.map(|r| short_ref(r).to_string())
	}

	pub(super) fn log_user_value(&self, loc: Locale) -> Option<String> {
		self.log_filter.author.clone().map(|a| {
			if self.git_user_email.as_deref() == Some(a.as_str()) {
				t("log_user_me", loc).to_string()
			} else {
				a
			}
		})
	}

	pub(super) fn log_date_value(&self, loc: Locale) -> Option<String> {
		let day = |s: &Option<String>| {
			s.as_deref()
				.map(|s| s.split(' ').next().unwrap_or(s).to_string())
				.unwrap_or_else(|| "…".into())
		};
		match (&self.log_filter.since, &self.log_filter.until) {
			(None, None) => None,
			(Some(s), None)
				if DATE_PRESETS.iter().any(|(_, since, _)| since == s) =>
			{
				DATE_PRESETS
					.iter()
					.find(|(_, since, _)| since == s)
					.map(|(_, _, label)| t(label, loc).to_string())
			}
			(since, until) => Some(format!("{} – {}", day(since), day(until))),
		}
	}

	pub(crate) fn log_scope_value(&self) -> Option<String> {
		let repo_value = match self.log_repo_filter.as_slice() {
			[] => None,
			_ => {
				let scope = self.log_scope();
				scope.first().map(|(root, _)| {
					let name = self.log_repo_chip_name(root);
					match scope.len() {
						1 => name,
						n => format!("{name} +{}", n - 1),
					}
				})
			}
		};
		let paths_value = self.log_filter.paths.first().map(|p| {
			match self.log_filter.paths.len() {
				1 => p.clone(),
				n => format!("{p} +{}", n - 1),
			}
		});
		match (repo_value, paths_value) {
			(Some(r), Some(p)) => Some(format!("{r} · {p}")),
			(r, p) => r.or(p),
		}
	}

	pub(super) fn log_menu_value(
		&self,
		menu: LogMenu,
		loc: Locale,
	) -> (String, Option<String>) {
		match menu {
			LogMenu::Repo => (
				t(
					if self.repos.len() > 1 {
						"log_chip_repo"
					} else {
						"log_chip_paths"
					},
					loc,
				)
				.to_string(),
				self.log_scope_value(),
			),
			LogMenu::Branch => (
				t("log_chip_branch", loc).to_string(),
				self.log_branch_value(),
			),
			LogMenu::User => (
				t("log_chip_user", loc).to_string(),
				self.log_user_value(loc),
			),
			LogMenu::Date => (
				t("log_chip_date", loc).to_string(),
				self.log_date_value(loc),
			),
			_ => (t("log_chip_more", loc).to_string(), None),
		}
	}

	pub(crate) fn log_chips_available_width(&self) -> f32 {
		let measured = self.log_filter_layout.container_w.get();
		if measured > 0.0 {
			measured
		} else if self.log_width.get() > 0.0 {
			let branches_w = if self.log_branches_visible {
				self.log_branches_w()
			} else {
				0.0
			};
			let details_w = if self.log_details_visible {
				self.log_details_width()
			} else {
				0.0
			};
			let search_w = if self.log_width.get() < 1200.0 {
				160.0
			} else {
				240.0
			};
			let middle_w =
				(self.log_width.get() - 28.0 - branches_w - details_w).max(0.0);
			(middle_w - search_w - 136.0).max(0.0)
		} else {
			0.0
		}
	}

	#[cfg(test)]
	pub(crate) fn log_filter_split_has_values(&self, loc: Locale) -> [bool; 4] {
		[
			self.log_scope_value().is_some(),
			self.log_branch_value().is_some(),
			self.log_user_value(loc).is_some(),
			self.log_date_value(loc).is_some(),
		]
	}

	/// The filter chips on the bar and those in the overflow chip, in their
	/// usual order, as the last frame split them.
	pub(crate) fn log_filter_split(&self) -> (Vec<LogMenu>, Vec<LogMenu>) {
		split_by_mask(self.log_filter_layout.last_mask.get())
	}

	pub(super) fn clear_log_chip(
		&mut self,
		menu: LogMenu,
		cx: &mut Context<Self>,
	) {
		self.close_log_menus(CloseWhy::Picked, cx);
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
			LogMenu::More | LogMenu::FilterMore => cx.notify(),
		}
	}

	/// The dropdown under a filter chip or the More button.
	pub(super) fn log_menu_panel(
		&self,
		menu: LogMenu,
		window: &Window,
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
				// FIT-EXEMPT: a filter menu's item label shares its row with a 14px check box only
				.child(fill_text(label))
				.children(probe(log, id))
				.into_any_element()
		};
		let mut items: Vec<AnyElement> = Vec::new();
		// The Branches menu is as wide as its rows need.
		let mut branch_w: Option<f32> = None;
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
						let selector = id.clone();
						let check_id = format!("log-repo-check:{}", repo.name);
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
								// FIT-EXEMPT: a repository row of the filter menu shares its row with a check box only
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
					&self.log_feed_names(),
					&needle,
					&self.log_branch_menu_open,
					loc,
				);
				let refs: Vec<BranchRow> = head
					.then(|| BranchRow::Ref {
						name: "HEAD".into(),
						label: "HEAD".into(),
						depth: 0,
						repo: None,
					})
					.into_iter()
					.chain(rows)
					.take(MAX_LOG_MENU_ITEMS)
					.collect();
				// The panel is as wide as its widest row, between 200 and
				// 360px: the width the rows then share is known.
				let widest = refs
					.iter()
					.map(|r| self.branch_menu_natural(window, r))
					.fold(0., f32::max);
				branch_w = Some(
					(widest + MENU_PANEL_CHROME).clamp(MENU_MIN_W, MENU_MAX_W),
				);
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
									// FIT-EXEMPT: a branch menu group's label shares its row with fixed icons only
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
						BranchRow::Ref {
							name,
							label,
							depth,
							repo,
						} => {
							let checked = self.active_ref_filter.as_deref()
								== Some(&name);
							items.push(self.branch_menu_row(
								window,
								MenuRow {
									name,
									label,
									depth,
									repo,
									checked,
									panel_w: branch_w.unwrap_or(MENU_MIN_W),
								},
								cx,
							));
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
							this.close_log_menus(CloseWhy::Picked, cx);
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
			LogMenu::FilterMore => {
				let (_, hidden) = self.log_filter_split();
				for m in hidden {
					let key = match m {
						LogMenu::Repo if self.repos.len() <= 1 => "paths",
						_ => m.key(),
					};
					let id = format!("log-filter-more:{key}");
					let (label, val) = self.log_menu_value(m, loc);
					let is_active = val.is_some();
					let display_label = label.clone();
					let display = match val {
						Some(v) => format!("{label}: {v}"),
						None => label,
					};
					items.push(item(
						id,
						display,
						is_active,
						Box::new(move |this, cx| {
							// A sub-menu replaces the overflow menu; the
							// overflow chip still closes it.
							let ret = ReturnFocus::To(this.log_focus.clone());
							this.open_overlay(
								OverlayKind::LogMenu(m),
								ret,
								Some(TriggerId::LogFilterMore),
								None,
								cx,
							);
						}),
						cx,
					));
					// Same id as the chip's own clear button: a filter that
					// went to the overflow can still be cleared.
					if is_active {
						items.push(item(
							format!("log-filter-{key}-clear"),
							format!(
								"{} · {}",
								t("log_clear_filter", loc),
								display_label
							),
							false,
							Box::new(move |this, cx| {
								this.clear_log_chip(m, cx)
							}),
							cx,
						));
					}
				}
			}
		}
		let panel = self
			.overlay_panel(OverlayKind::LogMenu(menu), cx)
			.min_w(px(MENU_MIN_W))
			.max_w(px(MENU_MAX_W))
			.when_some(branch_w, |d, w| d.w(px(w)))
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
			// FIT-EXEMPT: a path picker row's name shares its row with fixed icons only
			.child(fill_text(pick.name))
			.children(probe(log, id))
			.into_any_element()
	}

	/// One ref row of the branches pane: the glyph, the name, and the repo
	/// the ref belongs to when it is not the same in all of them. The name
	/// gets its floor first and then its whole text; the repo only comes
	/// whole, after the name is whole, or not at all (#194).
	pub(super) fn branch_pane_row(
		&self,
		window: &Window,
		row: PaneRow,
		cx: &mut Context<Self>,
	) -> AnyElement {
		let PaneRow {
			id,
			label,
			glyph,
			indent,
			target,
			active,
			repo,
		} = row;
		let probes = &self.probes;
		let name_key = id.trim_start_matches("ref:").to_string();
		let name_m = role_measure(window, FitRole::PaneRefName);
		let repo_m = role_measure(window, FitRole::PaneRepo);
		// The row's width less its margins, padding, glyph and the gap
		// before the name.
		let avail = self.log_branches_w()
			- PANE_BORDER_W
			- 2. * PANE_ROW_MARGIN
			- indent - PANE_ROW_PAD_R
			- PANE_GLYPH_W
			- REF_ROW_GAP;
		let name_natural = natural_px(FitRole::PaneRefName, &label, &name_m);
		let name_floor = floor_px(FitRole::PaneRefName, &label, 0, &name_m);
		let repo_natural = repo
			.as_deref()
			.map_or(0., |r| natural_px(FitRole::PaneRepo, r, &repo_m));
		let fit = fit_row(
			avail,
			&[
				Part::new(RefRowCell::Name, name_natural, name_floor, 0.),
				Part::new(
					RefRowCell::Suffix,
					repo_natural,
					repo_natural,
					REF_ROW_GAP,
				),
			],
			&[
				Claim::Grow(RefRowCell::Name, name_floor),
				Claim::Grow(RefRowCell::Name, name_natural),
				Claim::Atomic(if repo.is_some() {
					vec![RefRowCell::Suffix]
				} else {
					vec![]
				}),
			],
		);
		let scope = format!("ref:{name_key}");
		let tip = match &repo {
			Some(r) => format!("{label} ({r})"),
			None => label.clone(),
		};
		// FIT-ROW-BEGIN branch_pane_row
		let name = FitCell::new(
			FitId::new(scope.clone(), FitRole::PaneRefName),
			label,
		)
		.selector(format!("ref-name:{name_key}"))
		.solved(&fit, RefRowCell::Name)
		.render(probes, window)
		.ml(px(REF_ROW_GAP))
		.text_color(rgb(pal().text));
		let suffix = repo.as_deref().map(|r| {
			if fit.shown(RefRowCell::Suffix) {
				Some(
					FitCell::new(
						FitId::new(scope.clone(), FitRole::PaneRepo),
						r.to_string(),
					)
					.selector(format!("ref-repo:{name_key}"))
					.solved(&fit, RefRowCell::Suffix)
					.render(probes, window)
					.ml(px(REF_ROW_GAP))
					.text_color(rgb(pal().text_muted)),
				)
			} else {
				if probes.is_some() {
					record_dropped(
						probes,
						FitId::new(scope.clone(), FitRole::PaneRepo),
						r,
						repo_natural,
						repo_natural,
					);
				}
				None
			}
		});
		div()
			.id(SharedString::from(id.clone()))
			.debug_selector(|| id.clone())
			.relative()
			.flex_shrink_0()
			.h(px(24.))
			.mx(px(PANE_ROW_MARGIN))
			.pl(px(indent))
			.pr(px(PANE_ROW_PAD_R))
			.flex()
			.items_center()
			.rounded(px(4.))
			.cursor_pointer()
			.when(active, |d| d.bg(rgb(pal().selection_inactive_bg)))
			.when(!active, |d| d.hover(|s| s.bg(rgb(pal().hover_bg))))
			.tip(tip)
			.on_click(cx.listener(move |this, _, _, cx| {
				this.filter_by_ref(target.clone(), cx)
			}))
			.child(div().flex_shrink_0().w(px(PANE_GLYPH_W)).child(glyph))
			.child(name)
			.children(suffix.flatten())
			.children(probe(&self.probes, id))
			.into_any_element()
		// FIT-ROW-END
	}

	/// What a row of the Branches menu needs to be whole, with its padding,
	/// check box and indent (the panel's own padding and border not counted).
	pub(super) fn branch_menu_natural(
		&self,
		window: &Window,
		row: &BranchRow,
	) -> f32 {
		match row {
			BranchRow::Group { label, .. } => {
				// Padding, the 14px before the label, the gap, the chevron.
				MENU_ITEM_CHROME
					+ text_width(window, label, UI_TEXT)
					+ REF_ROW_GAP + 12.
			}
			BranchRow::Ref {
				label, depth, repo, ..
			} => {
				let name = natural_px(
					FitRole::MenuRefName,
					label,
					&role_measure(window, FitRole::MenuRefName),
				);
				let suffix = repo.as_deref().map_or(0., |r| {
					REF_ROW_GAP
						+ natural_px(
							FitRole::MenuRepo,
							r,
							&role_measure(window, FitRole::MenuRepo),
						)
				});
				MENU_ITEM_CHROME + *depth as f32 * MENU_INDENT_W + name + suffix
			}
		}
	}

	/// One ref row of the Branches menu: a check box, the name and the repo
	/// of the ref, in the panel's width. The name gets its floor first and
	/// then its whole text; the repo comes whole after it or not at all.
	pub(super) fn branch_menu_row(
		&self,
		window: &Window,
		row: MenuRow,
		cx: &mut Context<Self>,
	) -> AnyElement {
		let MenuRow {
			name,
			label,
			depth,
			repo,
			checked,
			panel_w,
		} = row;
		let probes = &self.probes;
		let indent = depth as f32 * MENU_INDENT_W;
		let name_m = role_measure(window, FitRole::MenuRefName);
		let repo_m = role_measure(window, FitRole::MenuRepo);
		// What the panel's content leaves after the indent, the item's
		// padding, the check box and its gap.
		let avail = panel_w - MENU_PANEL_CHROME - indent - MENU_ITEM_CHROME;
		let name_natural = natural_px(FitRole::MenuRefName, &label, &name_m);
		let name_floor = floor_px(FitRole::MenuRefName, &label, 0, &name_m);
		let repo_natural = repo
			.as_deref()
			.map_or(0., |r| natural_px(FitRole::MenuRepo, r, &repo_m));
		let fit = fit_row(
			avail,
			&[
				Part::new(RefRowCell::Name, name_natural, name_floor, 0.),
				Part::new(
					RefRowCell::Suffix,
					repo_natural,
					repo_natural,
					REF_ROW_GAP,
				),
			],
			&[
				Claim::Grow(RefRowCell::Name, name_floor),
				Claim::Grow(RefRowCell::Name, name_natural),
				Claim::Atomic(if repo.is_some() {
					vec![RefRowCell::Suffix]
				} else {
					vec![]
				}),
			],
		);
		let scope = format!("log-branch:{name}");
		let id = format!("log-branch:{name}");
		let selector = id.clone();
		let target = name.clone();
		// FIT-ROW-BEGIN branch_menu_row
		let text = FitCell::new(
			FitId::new(scope.clone(), FitRole::MenuRefName),
			label,
		)
		.selector(format!("menu-name:{name}"))
		.solved(&fit, RefRowCell::Name)
		.render(probes, window)
		.ml(px(REF_ROW_GAP));
		let suffix = repo.as_deref().and_then(|r| {
			if fit.shown(RefRowCell::Suffix) {
				Some(
					FitCell::new(
						FitId::new(scope.clone(), FitRole::MenuRepo),
						r.to_string(),
					)
					.selector(format!("menu-repo:{name}"))
					.solved(&fit, RefRowCell::Suffix)
					.render(probes, window)
					.ml(px(REF_ROW_GAP))
					.text_color(rgb(pal().text_muted)),
				)
			} else {
				if probes.is_some() {
					record_dropped(
						probes,
						FitId::new(scope.clone(), FitRole::MenuRepo),
						r,
						repo_natural,
						repo_natural,
					);
				}
				None
			}
		});
		div()
			.pl(px(indent))
			.flex()
			.items_center()
			.child(
				div()
					.id(SharedString::from(id.clone()))
					.debug_selector(move || selector)
					.relative()
					.w(px(panel_w - MENU_PANEL_CHROME - indent))
					.h(px(24.))
					.px(px(8.))
					.flex()
					.items_center()
					.rounded(px(4.))
					.cursor_pointer()
					.hover(|s| s.bg(rgb(pal().hover_bg)))
					.on_click(cx.listener(move |this, _, _, cx| {
						cx.stop_propagation();
						this.close_log_menus(CloseWhy::Picked, cx);
						this.filter_by_ref(Some(target.clone()), cx)
					}))
					.child(
						div().flex_shrink_0().w(px(14.)).when(checked, |d| {
							d.child(icon(Icon::Checked, 14.))
						}),
					)
					.child(text)
					.children(suffix)
					.children(probe(&self.probes, id)),
			)
			.into_any_element()
		// FIT-ROW-END
	}

	/// IntelliJ's branches pane: search, HEAD, collapsible Local / Remote /
	/// Tags groups (no counts).
	pub(super) fn render_branches(
		&self,
		window: &Window,
		cx: &mut Context<Self>,
	) -> AnyElement {
		let loc = self.locale;
		let log = &self.probes;
		let needle = self
			.branch_filter_input
			.read(cx)
			.text()
			.trim()
			.to_lowercase();
		let current = self.log_branch_list_current();
		let mut rows: Vec<AnyElement> = Vec::new();
		let entry = |id: String,
		             label: String,
		             glyph: AnyElement,
		             indent: f32,
		             target: Option<String>,
		             active: bool,
		             repo: Option<String>,
		             cx: &mut Context<Self>| {
			self.branch_pane_row(
				window,
				PaneRow {
					id,
					label,
					glyph,
					indent,
					target,
					active,
					repo,
				},
				cx,
			)
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
				None,
				cx,
			));
		}
		for row in branch_rows(
			&self.refs,
			&self.log_feed_names(),
			&needle,
			&self.branch_groups_collapsed,
			loc,
		) {
			match row {
				BranchRow::Group {
					key,
					label,
					depth,
					collapsed,
				} => {
					let gid = format!("branch-group:{key}");
					let selector = gid.clone();
					rows.push(
						div()
							.id(SharedString::from(gid.clone()))
							.debug_selector(move || selector)
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
							.child(
								FitCell::new(
									FitId::new(gid.clone(), FitRole::PaneGroup),
									label,
								)
								.flex()
								.render(&self.probes, window)
								.text_color(rgb(pal().text)),
							)
							.children(probe(log, gid))
							.into_any_element(),
					);
				}
				BranchRow::Ref {
					name,
					label,
					depth,
					repo,
				} => {
					let is_current = name.strip_prefix("refs/heads/").is_some()
						&& current.as_deref()
							== name.strip_prefix("refs/heads/");
					let glyph = if name.starts_with("refs/tags/") {
						icon_tinted(Icon::Tag, 14., pal().ref_tag)
							.into_any_element()
					} else if is_current {
						div()
							.id(SharedString::from(format!("ref-head:{name}")))
							.debug_selector(|| format!("ref-head:{name}"))
							.relative()
							.flex_shrink_0()
							.size(px(14.))
							.child(icon_tinted(Icon::Head, 14., pal().ref_head))
							.children(probe(log, format!("ref-head:{name}")))
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
						repo,
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
mod filter_split_tests {
	use super::*;

	fn chip(natural: f32, floor: f32, valued: bool) -> ChipInput {
		ChipInput {
			natural,
			floor,
			valued,
		}
	}

	const MORE: f32 = 46.;
	const DOT: f32 = 9.;

	fn split(chips: [ChipInput; 4], avail: f32) -> u8 {
		split_chips(&chips, MORE, MORE + DOT, avail).0
	}

	/// All chips stay when they fit whole; chips without a value give way
	/// before one with a value, from the end; the overflow chip's dot counts
	/// only for a hidden value.
	#[test]
	fn chips_without_a_value_give_way_first() {
		let chips = [
			chip(100., 60., true),
			chip(50., 50., false),
			chip(110., 60., true),
			chip(55., 55., false),
		];
		let g = LOG_FILTER_GAP;
		let all = 100. + 50. + 110. + 55. + 3. * g;
		assert_eq!(split(chips, all), 0b1111);
		// Repo and User have a value: Date, then Branch go first.
		let two = 100. + 110. + MORE + 2. * g;
		assert_eq!(split(chips, two), 0b0101);
		assert_eq!(split(chips, two + 30.), 0b0101);
		let three = 100. + 50. + 110. + MORE + 3. * g;
		assert_eq!(split(chips, three), 0b0111);
		// Too narrow for both values whole: the two floors and the overflow
		// chip, with its dot, still fit, then a value is cut before one goes.
		let floors = 60. + 60. + MORE + DOT + 2. * g;
		let (mask, fit) = split_chips(&chips, MORE, MORE + DOT, floors);
		assert_eq!(mask, 0b0101);
		assert!(fit.width(ChipPart::Chip(LogMenu::Repo)) >= 60.);
		// Without room for the second floor it goes to the overflow chip.
		assert_eq!(split(chips, floors - 1.), 0b0001);
		// Nothing has a value: the front chips stay.
		let plain = [
			chip(100., 100., false),
			chip(50., 50., false),
			chip(110., 110., false),
			chip(55., 55., false),
		];
		assert_eq!(split(plain, 100. + 50. + MORE + 2. * g), 0b0011);
	}

	/// The overflow chip needs no dot when nothing with a value is hidden:
	/// the split is made again without it, and keeps the room.
	#[test]
	fn the_dot_is_only_for_a_hidden_value() {
		let chips = [
			chip(100., 60., true),
			chip(50., 50., false),
			chip(110., 110., false),
			chip(55., 55., false),
		];
		let g = LOG_FILTER_GAP;
		// Repo plus the overflow chip, no dot: fits in 100 + 46 + g, not with
		// the dot's 9 more: the split without the dot still shows the value.
		let (mask, fit) =
			split_chips(&chips, MORE, MORE + DOT, 100. + MORE + g);
		assert_eq!(mask, 0b0001);
		assert_eq!(fit.width(ChipPart::More), MORE);
	}

	/// A short value is whole before a longer one is given more (#205): the
	/// shorter active chip stays whole while the longer one is cut.
	#[test]
	fn the_shorter_value_is_whole_before_the_longer_gets_more() {
		let chips = [
			chip(300., 90., true),
			chip(120., 70., true),
			chip(40., 40., false),
			chip(40., 40., false),
		];
		let g = LOG_FILTER_GAP;
		// Room for the floors, the overflow chip and 40px more.
		let avail = 90. + 70. + MORE + DOT + 2. * g + 40.;
		let (mask, fit) = split_chips(&chips, MORE, MORE + DOT, avail);
		assert_eq!(mask & 0b11, 0b11);
		let (repo, branch) = (
			fit.width(ChipPart::Chip(LogMenu::Repo)),
			fit.width(ChipPart::Chip(LogMenu::Branch)),
		);
		// Nothing with a value is hidden, so the overflow chip has no dot and
		// there are 49px to give: the shorter value (120) takes them first,
		// 70 + 49, and the longer stays at its floor.
		assert_eq!((repo, branch), (90., 119.));
		// With 10px more the shorter one is whole (120) and the longer one
		// gets the 9px that are left.
		let (_, fit) = split_chips(&chips, MORE, MORE + DOT, avail + 10.);
		assert_eq!(fit.width(ChipPart::Chip(LogMenu::Branch)), 120.);
		assert_eq!(fit.width(ChipPart::Chip(LogMenu::Repo)), 99.);
	}

	#[test]
	fn no_room_for_the_overflow_chip_puts_nothing_on_the_bar() {
		let chips = [
			chip(100., 60., true),
			chip(50., 50., false),
			chip(110., 60., true),
			chip(55., 55., false),
		];
		assert_eq!(split(chips, MORE + DOT - 1.), 0);
		assert_eq!(split(chips, 0.), 0);
	}
}

#[cfg(test)]
mod repo_label_tests {
	use super::*;
	use snip_core::browser::GitReference;

	fn reference(name: &str, sha: &str) -> GitReference {
		GitReference {
			name: name.into(),
			sha: sha.into(),
		}
	}

	fn repo_of(rows: &[BranchRow], name: &str) -> Option<Option<String>> {
		rows.iter().find_map(|r| match r {
			BranchRow::Ref { name: n, repo, .. } if n == name => {
				Some(repo.clone())
			}
			_ => None,
		})
	}

	#[test]
	fn branch_rows_name_the_repo_or_count_in_a_merged_log() {
		let refs = vec![
			reference("refs/heads/main", "aa@0"),
			reference("refs/heads/main", "bb@1"),
			reference("refs/heads/feature/x", "cc@1"),
			reference("refs/tags/v1", "aa@0"),
		];
		let feeds = ["alpha".to_string(), "branchy".to_string()];
		let rows = branch_rows(&refs, &feeds, "", &[], Locale::ZhTw);
		assert_eq!(
			repo_of(&rows, "refs/heads/main"),
			Some(Some("×2".to_string()))
		);
		assert_eq!(
			repo_of(&rows, "refs/heads/feature/x"),
			Some(Some("branchy".to_string()))
		);
		assert_eq!(
			repo_of(&rows, "refs/tags/v1"),
			Some(Some("alpha".to_string()))
		);
		// A single repository names none.
		let rows = branch_rows(&refs, &[], "", &[], Locale::ZhTw);
		assert!(rows.iter().all(|r| match r {
			BranchRow::Ref { repo, .. } => repo.is_none(),
			BranchRow::Group { .. } => true,
		}));
	}

	#[test]
	fn label_icon_kind_differs_for_branch_and_tag() {
		let label = |current: bool, tag: bool| graph_view::RefLabel {
			text: "x".into(),
			color: 0,
			current,
			tag,
		};
		assert_eq!(label_icon_kind(&label(true, false)), Icon::Head);
		assert_eq!(label_icon_kind(&label(false, true)), Icon::Tag);
		assert_eq!(label_icon_kind(&label(false, false)), Icon::Branch);
	}
}

#[cfg(test)]
mod row_fit_tests {
	use super::*;
	use crate::ui::fit::{ELLIPSIS, FIT_MARGIN};

	/// 0.6em per character, as the test text system measures; `wide` text
	/// (a CJK font) 1.0em, the ellipsis always 0.6.
	fn measure(role: FitRole, wide: bool) -> impl Fn(&str) -> f32 {
		let size = role.font().1;
		move |s: &str| {
			s.chars()
				.map(|c| {
					if wide && !c.is_ascii() && c != '\u{2026}' {
						1.0
					} else {
						0.6
					}
				})
				.sum::<f32>()
				* size
		}
	}

	fn part(cell: LogCell, text: &str, wide: bool) -> LabelPart {
		let role = cell.role().unwrap();
		let m = measure(role, wide);
		let natural = natural_px(role, text, &m).min(LABEL_MAX_W);
		LabelPart {
			cell,
			natural,
			floor: floor_px(role, text, 0, &m).min(natural),
		}
	}

	const CJK_TAG: &str =
		"\u{767c}\u{884c}\u{7248}\u{672c}-\u{4e2d}\u{6587}\u{6a19}\u{7c64}";
	const CJK_SUBJECT: &str = "\u{4fee}\u{6b63}\u{9060}\u{7aef}\u{5de5}\u{4f5c}\u{5340}\u{5728}\u{6162}\u{901f}\u{9023}\u{7dda}\u{4e0b}\u{8b80}\u{53d6}";
	const CJK_AUTHOR: &str = "\u{9673}\u{5927}\u{6587}\u{6e2c}\u{8a66}\u{4f5c}\u{8005}\u{540d}\u{7a31}";

	/// The label groups a row can have: none, one branch, a branch and a tag
	/// (the most: a row shows two badges, or one and `+N`), a CJK tag, a label
	/// over the cap, one label and `+12`.
	fn label_shapes() -> Vec<(&'static str, Vec<LabelPart>)> {
		let long = "feature/".to_string() + &"x".repeat(40);
		vec![
			("none", vec![]),
			("main", vec![part(LogCell::BranchLabel, "main", false)]),
			(
				"main+tag",
				vec![
					part(LogCell::BranchLabel, "main, origin/main", false),
					part(LogCell::TagLabel, "v0.3.0", false),
				],
			),
			(
				"cjk tag",
				vec![
					part(LogCell::BranchLabel, "main, origin/main", false),
					part(LogCell::TagLabel, CJK_TAG, true),
				],
			),
			(
				"over the cap",
				vec![part(LogCell::BranchLabel, &long, false)],
			),
			// A row with three refs or more shows one badge and `+N`.
			(
				"one and more",
				vec![
					part(LogCell::TagLabel, CJK_TAG, true),
					part(LogCell::RefMore, "+12", false),
				],
			),
		]
	}

	/// The collapsed chip of a merge row: none, English, and CJK at 1em (the
	/// widest per character), so the chip's width differs by font.
	fn collapsed_shapes() -> Vec<(&'static str, Option<f32>)> {
		let w = |text: &str, wide: bool| {
			natural_px(
				FitRole::Collapsed,
				text,
				&measure(FitRole::Collapsed, wide),
			)
		};
		vec![
			("no chip", None),
			("collapsed en", Some(w("+3 collapsed nodes", false))),
			(
				"collapsed cjk",
				Some(w(
					"+3 \u{500b}\u{5df2}\u{6536}\u{5408}\u{7bc0}\u{9ede}",
					true,
				)),
			),
		]
	}

	#[derive(Clone)]
	struct Shape {
		collapsed_label: &'static str,
		collapsed: Option<f32>,
		list_w: f32,
		hash: bool,
		label: &'static str,
		labels: Vec<LabelPart>,
		subject: &'static str,
		wide_subject: bool,
		gutter: f32,
		author: Option<&'static str>,
		date_need: f32,
	}

	struct Solved {
		cols: ListCols,
		row: RowFit<LogCell>,
		input: ListFitInput,
	}

	fn solve(s: &Shape) -> Solved {
		let subject_m = measure(FitRole::Subject, s.wide_subject);
		let bound_m = measure(FitRole::Subject, true);
		let author_m = measure(FitRole::Author, true);
		let input = ListFitInput {
			list_w: s.list_w,
			hash: s.hash,
			gutter: s.gutter,
			date_need: s.date_need,
			author_floor: s
				.author
				.map(|a| floor_px(FitRole::Author, a, 0, &author_m)),
			subject_floor_bound: bound_m(&format!(
				"{}{ELLIPSIS}",
				"\u{6f22}".repeat(6)
			))
			.max(bound_m(&format!("{}{ELLIPSIS}", "W".repeat(6))))
				+ FIT_MARGIN,
			collapsed: s.collapsed,
			labels: s.labels.clone(),
		};
		let cols = fit_list(&input);
		let row = fit_log_row(
			s.list_w,
			&cols,
			&RowFitInput {
				hash: s.hash,
				subject_natural: natural_px(
					FitRole::Subject,
					s.subject,
					&subject_m,
				),
				subject_floor: floor_px(
					FitRole::Subject,
					s.subject,
					0,
					&subject_m,
				),
				collapsed: s.collapsed,
				labels: s.labels.clone(),
			},
		);
		Solved { cols, row, input }
	}

	fn shapes(list_w: impl Iterator<Item = f32> + Clone) -> Vec<Shape> {
		let mut out = Vec::new();
		for list_w in list_w {
			for hash in [false, true] {
				for (label, labels) in label_shapes() {
					for (subject, wide_subject) in [
						("short", false),
						("a subject long enough to want the whole row for itself and then some more words", false),
						(CJK_SUBJECT, true),
					] {
						for gutter in [24., 40., 72., 392.] {
							for author in [None, Some("t"), Some(CJK_AUTHOR)] {
								for date_need in [110., 124.8] {
									for (collapsed_label, collapsed) in
										collapsed_shapes()
									{
									out.push(Shape {
										collapsed_label,
										collapsed,
										list_w,
										hash,
										label,
										labels: labels.clone(),
										subject,
										wide_subject,
										gutter,
										author,
										date_need,
									});
									}
								}
							}
						}
					}
				}
			}
		}
		out
	}

	fn tag(s: &Shape) -> String {
		format!(
			"list {} hash={} labels={} chip={} subject={:?} gutter={} author={:?} need={}",
			s.list_w,
			s.hash,
			s.label,
			s.collapsed_label,
			s.subject,
			s.gutter,
			s.author,
			s.date_need
		)
	}

	/// The width a shown cell takes with its gap before it.
	fn used(r: &RowFit<LogCell>, labels: &[LabelPart]) -> f32 {
		let mut total = 0.;
		for cell in LogCell::ALL {
			if !r.shown(cell) {
				continue;
			}
			let gap = match cell {
				LogCell::Gutter => 0.,
				LogCell::Subject
				| LogCell::Author
				| LogCell::Date
				| LogCell::Hash => CELL_GAP,
				LogCell::Collapsed => SUBJECT_LABEL_GAP,
				LogCell::BranchLabel | LogCell::TagLabel | LogCell::RefMore => {
					if labels.first().map(|l| l.cell) == Some(cell) {
						SUBJECT_LABEL_GAP
					} else {
						LABEL_GAP
					}
				}
			};
			total += r.width(cell) + gap;
		}
		total
	}

	/// P-1: the one policy, over every shape (#228). The date is whole or
	/// the column is gone (only on a list under 250px); the subject keeps its
	/// readable floor from 433px; the label group is all shown or all
	/// dropped, each at its floor at least, the shorter whole before the
	/// longer; the author column is absent or at its floor at least; nothing
	/// is wider than the row.
	#[test]
	fn log_row_policy_holds_for_every_shape() {
		let mut bad = Vec::new();
		let mut checked = 0;
		for s in shapes((300..=1400).step_by(20).map(|w| w as f32)) {
			checked += 1;
			let Solved { cols, row, input } = solve(&s);
			let t = tag(&s);
			match cols.date {
				Some(w) => {
					if w < s.date_need - 0.01
						|| w > s.date_need.max(DATE_W) + 0.01
					{
						bad.push(format!(
							"{t}: date {w} for need {}",
							s.date_need
						));
					}
				}
				None => {
					if s.list_w >= 250. {
						bad.push(format!("{t}: no date column"));
					}
				}
			}
			if s.list_w >= 433.
				&& row.width(LogCell::Subject)
					< row.floor(LogCell::Subject) - 0.01
			{
				bad.push(format!(
					"{t}: subject {} < floor {}",
					row.width(LogCell::Subject),
					row.floor(LogCell::Subject)
				));
			}
			let cells: Vec<LogCell> = s.labels.iter().map(|l| l.cell).collect();
			let shown: Vec<bool> =
				cells.iter().map(|c| row.shown(*c)).collect();
			if shown.iter().any(|x| *x != shown[0]) {
				bad.push(format!("{t}: the label group is split: {shown:?}"));
			}
			for l in &s.labels {
				if row.shown(l.cell) && row.width(l.cell) < l.floor - 0.01 {
					bad.push(format!(
						"{t}: {:?} {} < floor {}",
						l.cell,
						row.width(l.cell),
						l.floor
					));
				}
			}
			let kinds: Vec<&LabelPart> = s
				.labels
				.iter()
				.filter(|l| l.cell != LogCell::RefMore)
				.collect();
			if let [a, b] = kinds[..] {
				if row.shown(a.cell) {
					let (short, long) = if a.natural <= b.natural {
						(a, b)
					} else {
						(b, a)
					};
					if row.width(short.cell)
						< short.natural.min(row.width(long.cell)) - 0.01
					{
						bad.push(format!(
							"{t}: the shorter label is cut before the longer"
						));
					}
				}
			}
			if let Some(w) = cols.author {
				if w < input.author_floor.unwrap() - 0.01 {
					bad.push(format!("{t}: author {w} under its floor"));
				}
			}
			if let Some(natural) = s.collapsed {
				if row.shown(LogCell::Collapsed)
					&& (row.width(LogCell::Collapsed) - natural).abs() > 0.01
				{
					bad.push(format!(
						"{t}: the collapsed chip is cut to {}",
						row.width(LogCell::Collapsed)
					));
				}
			}
			let u = used(&row, &s.labels);
			if u > s.list_w - ROW_PAD_R + 0.01 {
				bad.push(format!("{t}: {u} wide in a {} row", s.list_w));
			}
		}
		assert!(checked > 5000, "{checked}");
		assert!(
			bad.is_empty(),
			"{} violations in {checked} shapes:\n{}",
			bad.len(),
			bad.iter().take(20).cloned().collect::<Vec<_>>().join("\n")
		);
	}

	/// P-2: every row of a list shares its columns, and a label group no
	/// bigger than the worst row's fits wherever the worst group fits.
	#[test]
	fn every_row_shares_the_list_columns() {
		let cost = |g: &[LabelPart]| -> f32 {
			g.iter().map(|l| l.floor + CELL_GAP).sum()
		};
		let mut checked = 0;
		for s in shapes([433., 574., 760., 1100.].into_iter()) {
			let Solved { cols, row, .. } = solve(&s);
			let t = tag(&s);
			assert_eq!(row.width(LogCell::Gutter), cols.gutter, "{t}");
			assert_eq!(
				cols.author.is_some(),
				s.author.is_some() && cols.author.is_some()
			);
			if let Some(w) = cols.author {
				assert_eq!(row.width(LogCell::Author), w, "{t}");
			} else {
				assert!(!row.shown(LogCell::Author), "{t}");
			}
			if let Some(w) = cols.date {
				assert_eq!(row.width(LogCell::Date), w, "{t}");
			}
			let Some(first) = s.labels.first() else {
				continue;
			};
			if !row.shown(first.cell) {
				continue;
			}
			for (name, other) in label_shapes() {
				if other.is_empty() || cost(&other) > cost(&s.labels) {
					continue;
				}
				let subject_m = measure(FitRole::Subject, s.wide_subject);
				let other_row = fit_log_row(
					s.list_w,
					&cols,
					&RowFitInput {
						hash: s.hash,
						subject_natural: natural_px(
							FitRole::Subject,
							s.subject,
							&subject_m,
						),
						subject_floor: floor_px(
							FitRole::Subject,
							s.subject,
							0,
							&subject_m,
						),
						collapsed: s.collapsed,
						labels: other.clone(),
					},
				);
				for l in &other {
					assert!(
						other_row.shown(l.cell),
						"{t}: the smaller group {name} lost {:?} in a list that had room for the worst",
						l.cell
					);
				}
			}
			checked += 1;
		}
		assert!(checked > 100, "{checked}");
	}

	/// P-3: from a 1080 window on (a 574px list), the labels are there,
	/// hash column on or off, with or without a collapsed chip (the chip
	/// gives way first).
	#[test]
	fn labels_stay_from_the_1080_list() {
		let mut bad = Vec::new();
		for s in shapes([574., 600., 700., 900., 1200.].into_iter()) {
			if s.labels.is_empty() {
				continue;
			}
			let Solved { row, .. } = solve(&s);
			for l in &s.labels {
				if !row.shown(l.cell) {
					bad.push(format!("{}: {:?} dropped", tag(&s), l.cell));
				}
			}
		}
		assert!(
			bad.is_empty(),
			"{} violations:\n{}",
			bad.len(),
			bad.iter().take(20).cloned().collect::<Vec<_>>().join("\n")
		);
	}

	#[test]
	fn the_date_need_is_the_widest_string_of_the_locale() {
		let m = |s: &str| s.chars().count() as f32 * 7.;
		let d = date_need_w(Locale::ZhTw, &m);
		assert_eq!(d, "2026/12/28 23:59".chars().count() as f32 * 7.);
	}
}
