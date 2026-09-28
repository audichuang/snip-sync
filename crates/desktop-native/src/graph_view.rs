//! Git commit graph layout view using snip_core::graph.
//!
//! Provides topological DAG graph calculation, vector canvas rails and merge curves,
//! passing lanes, ref badges (HEAD, branches, remotes, tags), bounded history paging,
//! and historical commit file inspection.

use std::collections::HashSet;

use gpui::{
	point, px, quad, rgb, size, Bounds, ContentMask, PathBuilder, Pixels, Rgba,
	Window,
};
use snip_core::browser::{CommitSummary, GitReference};
use snip_core::graph::{
	compute_graph_layout, fallback_linear_layout, ContinuationKind,
	GraphCheckpoint, GraphConfig, GraphError, GraphLayout, GraphRow, NodeType,
	RefInfo, RefKind,
};

use crate::theme::pal;

pub const ROW_HEIGHT: f32 = crate::theme::ROW_H;
pub const LANE_WIDTH: f32 = 16.0;
pub const OFFSET_X: f32 = 14.0;
/// Commit dot radius (IntelliJ's log dot at a 24px row).
pub const NODE_RADIUS: f32 = 3.5;

/// Owned backing storage only; the enclosing struct/vector accounts for headers.
pub(crate) fn vec_bytes<T>(values: &Vec<T>) -> usize {
	values.capacity().saturating_mul(std::mem::size_of::<T>())
}

fn optional_string_bytes(value: &Option<String>) -> usize {
	value.as_ref().map_or(0, String::capacity)
}

pub(crate) fn checkpoint_heap_bytes(checkpoint: &GraphCheckpoint) -> usize {
	let mut bytes = vec_bytes(&checkpoint.frontier)
		.saturating_add(optional_string_bytes(&checkpoint.last_seen_sha));
	for rail in &checkpoint.frontier {
		bytes = bytes
			.saturating_add(rail.next_sha.capacity())
			.saturating_add(optional_string_bytes(&rail.color_override));
	}
	bytes
}

/// Capacity of every nested buffer in the retained renderer-neutral layout.
/// This is application-owned storage, not allocator overhead or process RSS.
pub(crate) fn layout_heap_bytes(layout: &GraphLayout) -> usize {
	let mut bytes = vec_bytes(&layout.rows)
		.saturating_add(vec_bytes(&layout.paths))
		.saturating_add(vec_bytes(&layout.links))
		.saturating_add(vec_bytes(&layout.commit_left_margin));
	for row in &layout.rows {
		bytes = bytes
			.saturating_add(row.sha.capacity())
			.saturating_add(optional_string_bytes(&row.node.color_override))
			.saturating_add(vec_bytes(&row.passing_lanes))
			.saturating_add(vec_bytes(&row.parent_edges))
			.saturating_add(vec_bytes(&row.refs));
		for lane in &row.passing_lanes {
			bytes = bytes
				.saturating_add(optional_string_bytes(&lane.color_override));
		}
		for edge in &row.parent_edges {
			bytes = bytes
				.saturating_add(edge.parent_sha.capacity())
				.saturating_add(optional_string_bytes(&edge.color_override));
		}
		for reference in &row.refs {
			bytes = bytes
				.saturating_add(reference.raw_name.capacity())
				.saturating_add(reference.display_name.capacity());
			if let RefKind::RemoteBranch { remote, name } = &reference.kind {
				bytes = bytes
					.saturating_add(remote.capacity())
					.saturating_add(name.capacity());
			}
		}
	}
	for path in &layout.paths {
		bytes = bytes
			.saturating_add(vec_bytes(&path.points))
			.saturating_add(optional_string_bytes(&path.color_override));
	}
	for link in &layout.links {
		bytes =
			bytes.saturating_add(optional_string_bytes(&link.color_override));
	}
	if let Some(checkpoint) = &layout.checkpoint {
		bytes = bytes.saturating_add(checkpoint_heap_bytes(checkpoint));
	}
	bytes
}

pub fn palette_rgb(index: usize) -> Rgba {
	let lanes = &pal().graph_lanes;
	rgb(lanes[index % lanes.len()])
}

pub fn lane_x(lane: usize) -> f32 {
	OFFSET_X + (lane as f32) * LANE_WIDTH
}

/// Lanes the gutter grows to; lanes past it are clipped at its edge.
pub const MAX_GUTTER_LANES: usize = 24;

pub fn gutter_width(layout: &GraphLayout) -> f32 {
	let max_lane = layout.rows.iter().map(|r| r.max_lane).max().unwrap_or(0);
	((max_lane + 1).min(MAX_GUTTER_LANES) as f32) * LANE_WIDTH + 8.0
}

/// Label text and text color for a ref; the background is the shared `ref_bg`.
pub fn format_ref_badge(info: &RefInfo) -> (String, Rgba) {
	match &info.kind {
		RefKind::Head => ("HEAD".to_string(), rgb(pal().ref_head)),
		RefKind::Branch => (info.display_name.clone(), rgb(pal().ref_local)),
		RefKind::RemoteBranch { remote, name } => {
			(format!("{remote}/{name}"), rgb(pal().ref_remote))
		}
		RefKind::Tag => (info.display_name.clone(), rgb(pal().ref_tag)),
		RefKind::Other => (info.display_name.clone(), rgb(pal().text_muted)),
	}
}

/// How IntelliJ draws one ref label: a coloured label icon (the "current"
/// shape for HEAD / the checked-out branch) and plain text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefLabel {
	pub text: String,
	pub color: u32,
	pub current: bool,
}

/// Label for a badge. A local branch with same-named remote-tracking refs
/// reads "origin & main", like IntelliJ.
pub fn ref_label(badge: &RefBadge, current_branch: Option<&str>) -> RefLabel {
	let info = badge.primary;
	let is_current = match &info.kind {
		RefKind::Head => true,
		RefKind::Branch => current_branch == Some(info.display_name.as_str()),
		_ => false,
	};
	let color = match &info.kind {
		_ if is_current => pal().ref_head,
		RefKind::Branch => pal().ref_local,
		RefKind::RemoteBranch { .. } => pal().ref_remote,
		RefKind::Tag => pal().ref_tag,
		_ => pal().text_muted,
	};
	let mut remotes: Vec<&str> = Vec::new();
	for r in &badge.merged {
		if let RefKind::RemoteBranch { remote, .. } = &r.kind {
			if !remotes.contains(&remote.as_str()) {
				remotes.push(remote);
			}
		}
	}
	let name = format_ref_badge(info).0;
	let text = if remotes.is_empty() {
		name
	} else {
		format!("{} & {name}", remotes.join(", "))
	};
	RefLabel {
		text,
		color,
		current: is_current,
	}
}

/// Display priority: current branch, HEAD, local, tag, remote, other.
fn ref_rank(info: &RefInfo, current_branch: Option<&str>) -> u8 {
	match &info.kind {
		RefKind::Branch
			if current_branch == Some(info.display_name.as_str()) =>
		{
			0
		}
		RefKind::Head => 1,
		RefKind::Branch => 2,
		RefKind::Tag => 3,
		RefKind::RemoteBranch { .. } => 4,
		RefKind::Other => 5,
	}
}

/// One drawn badge: `primary` plus the remote-tracking refs folded into it.
pub struct RefBadge<'a> {
	pub primary: &'a RefInfo,
	pub merged: Vec<&'a RefInfo>,
}

/// Display only, IntelliJ style: a remote-tracking ref whose short name equals
/// a local branch on the same commit folds into that branch's badge (any
/// remote). `<remote>/HEAD` follows its remote's sibling ref on the commit.
pub fn merge_tracking_refs(refs: &[RefInfo]) -> Vec<RefBadge<'_>> {
	let local = |name: &str| {
		refs.iter()
			.position(|r| r.kind == RefKind::Branch && r.display_name == name)
	};
	// Index of the ref whose badge absorbs `refs[i]`, or `i` itself.
	let owner = |i: usize| -> usize {
		let RefKind::RemoteBranch { remote, name } = &refs[i].kind else {
			return i;
		};
		if name != "HEAD" {
			return local(name).unwrap_or(i);
		}
		refs.iter()
			.enumerate()
			.find_map(|(j, r)| match &r.kind {
				RefKind::RemoteBranch {
					remote: rm,
					name: n,
				} if rm == remote && n != "HEAD" => Some(local(n).unwrap_or(j)),
				_ => None,
			})
			.unwrap_or(i)
	};
	let owners: Vec<usize> = (0..refs.len()).map(owner).collect();
	let mut out: Vec<(usize, RefBadge)> = refs
		.iter()
		.enumerate()
		.filter(|(i, _)| owners[*i] == *i)
		.map(|(i, r)| {
			(
				i,
				RefBadge {
					primary: r,
					merged: Vec::new(),
				},
			)
		})
		.collect();
	for (r, &o) in refs.iter().zip(&owners) {
		if let Some((_, b)) = out.iter_mut().find(|(k, _)| *k == o) {
			if !std::ptr::eq(b.primary, r) {
				b.merged.push(r);
			}
		}
	}
	out.into_iter().map(|(_, b)| b).collect()
}

/// Display only: the badges drawn on a row plus how many fold into a `+N`
/// badge. Tracking refs merge first; two badges are shown as-is, more
/// collapse to the best one + `+N`.
pub fn visible_refs<'a>(
	refs: &'a [RefInfo],
	current_branch: Option<&str>,
) -> (Vec<RefBadge<'a>>, usize) {
	let mut sorted = merge_tracking_refs(refs);
	sorted.sort_by_key(|b| ref_rank(b.primary, current_branch));
	let shown = if sorted.len() <= 2 { sorted.len() } else { 1 };
	let hidden = sorted.len() - shown;
	sorted.truncate(shown);
	(sorted, hidden)
}

/// Rails one page may keep open. Beyond it the page is a plain list (with a
/// warning) rather than an error; the lanes themselves are clipped to the
/// gutter, so this only bounds layout memory.
pub const MAX_FRONTIER: usize = 256;
/// Refs one page hands to the layout (labels and rail colors only).
pub const MAX_LAYOUT_REFS: usize = 1000;
/// Rows one layout takes: the log's whole window.
pub const MAX_LAYOUT_ROWS: usize = 500;

fn graph_config(
	filtered: HashSet<String>,
	shallow: HashSet<String>,
) -> GraphConfig {
	GraphConfig {
		max_rows: MAX_LAYOUT_ROWS,
		max_frontier_size: MAX_FRONTIER,
		max_refs: MAX_LAYOUT_REFS,
		unit_x: LANE_WIDTH as f64,
		unit_y: ROW_HEIGHT as f64,
		offset_x: OFFSET_X as f64,
		filtered_commits: filtered,
		shallow_roots: shallow,
		..Default::default()
	}
}

/// The refs a page's layout needs: those on its commits (labels) or on
/// their parents (merge-rail colors), page commits first, at most
/// [`MAX_LAYOUT_REFS`]. A detached HEAD gets its own `HEAD` badge. The full
/// ref list stays with the caller for the ref selector.
pub fn page_refs(
	commits: &[CommitSummary],
	refs: &[GitReference],
	head: Option<&str>,
	detached: bool,
) -> Vec<GitReference> {
	let on_page: HashSet<&str> =
		commits.iter().map(|c| c.sha.as_str()).collect();
	let parents: HashSet<&str> = commits
		.iter()
		.flat_map(|c| c.parents.iter().map(String::as_str))
		.collect();
	let mut out: Vec<GitReference> = Vec::new();
	if let Some(head) = head.filter(|h| detached && on_page.contains(h)) {
		out.push(GitReference {
			name: "HEAD".into(),
			sha: head.to_string(),
		});
	}
	// ponytail: refs past the cap lose their badge on this page only;
	// a +N for them needs the layout to accept more refs.
	out.extend(
		refs.iter()
			.filter(|r| on_page.contains(r.sha.as_str()))
			.chain(refs.iter().filter(|r| {
				!on_page.contains(r.sha.as_str())
					&& parents.contains(r.sha.as_str())
			}))
			.take(MAX_LAYOUT_REFS.saturating_sub(out.len()))
			.cloned(),
	);
	out
}

/// Lays out one page. Too many concurrent rails (or no checkpoint to
/// continue from, after such a page) gives a plain list with
/// `is_fallback`, never an error that would leave history unpageable.
pub fn layout_page(
	commits: &[CommitSummary],
	refs: &[GitReference],
	head_sha: Option<&str>,
	checkpoint: Option<&GraphCheckpoint>,
	resume: bool,
	filtered: HashSet<String>,
	shallow: HashSet<String>,
) -> Result<GraphLayout, String> {
	let config = GraphConfig {
		// Row ids namespaced per repository: several unrelated histories
		// share the lanes, so a root's lane stays empty for a row before
		// another repository's rail may bend into it.
		hold_root_lanes: commits
			.first()
			.is_some_and(|c| crate::multi_log::split_id(&c.sha).1.is_some()),
		..graph_config(filtered, shallow)
	};
	let fallback = |config: &GraphConfig| {
		fallback_linear_layout(commits, refs, head_sha, config)
			.map_err(|e| format!("Graph layout error: {e}"))
	};
	if resume && checkpoint.is_none() {
		return fallback(&config);
	}
	match compute_graph_layout(commits, refs, head_sha, &config, checkpoint) {
		Err(GraphError::FrontierLimitExceeded { .. }) => fallback(&config),
		other => other.map_err(|e| format!("Graph layout error: {e}")),
	}
}

pub fn layout_commits(
	commits: &[CommitSummary],
	refs: &[GitReference],
	head_sha: Option<&str>,
) -> Result<GraphLayout, String> {
	layout_commits_paged(commits, refs, head_sha, None)
}

pub fn layout_commits_paged(
	commits: &[CommitSummary],
	refs: &[GitReference],
	head_sha: Option<&str>,
	checkpoint: Option<&GraphCheckpoint>,
) -> Result<GraphLayout, String> {
	layout_page(
		commits,
		refs,
		head_sha,
		checkpoint,
		false,
		HashSet::new(),
		HashSet::new(),
	)
}

/// How a [`Stroke`] is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrokeKind {
	/// A rail: straight segments through `points`.
	Rail,
	/// A merge curve: `points` is start, control, end (quadratic).
	Merge,
	/// Collapsed history below the dot: a dotted stub, not a join.
	Gap,
	/// A shallow clone's cut: dashed, the parents exist but are not here.
	Shallow,
}

/// One line drawn in a row, in row-local pixels (y = 0 is the row top).
#[derive(Debug, Clone, PartialEq)]
pub struct Stroke {
	pub kind: StrokeKind,
	pub color: usize,
	pub points: Vec<(f32, f32)>,
}

/// Everything a row draws besides its dot: the part of every rail inside
/// the row's y-range, the merge curves that start on it, and gap / shallow
/// stubs. Rails are clipped from whole-page polylines, so a bend, a lane
/// shift or a converging branch is drawn exactly where the layout put it
/// and each row's lines meet the next row's at the shared edge.
pub fn row_strokes(layout: &GraphLayout, row: &GraphRow) -> Vec<Stroke> {
	let h = ROW_HEIGHT as f64;
	let top = row.global_row as f64 * h;
	let bottom = top + h;
	let local = |x: f64, y: f64| (x as f32, (y - top) as f32);
	let mut out = Vec::new();
	for path in &layout.paths {
		let points = path.clip_y(top, bottom);
		if points.len() >= 2 {
			out.push(Stroke {
				kind: StrokeKind::Rail,
				color: path.color,
				points: points.iter().map(|p| local(p.x, p.y)).collect(),
			});
		}
	}
	for link in &layout.links {
		if link.start.y >= top && link.start.y < bottom {
			out.push(Stroke {
				kind: StrokeKind::Merge,
				color: link.color,
				points: [link.start, link.control, link.end]
					.iter()
					.map(|p| local(p.x, p.y))
					.collect(),
			});
		}
	}
	let mid = ROW_HEIGHT * 0.5;
	for edge in &row.parent_edges {
		let x = lane_x(edge.from_lane);
		let (kind, end) = match edge.continuation {
			ContinuationKind::FilteredGap => {
				(StrokeKind::Gap, (x, mid + (ROW_HEIGHT - mid) * 0.7))
			}
			ContinuationKind::ShallowBoundary => {
				(StrokeKind::Shallow, (x, ROW_HEIGHT))
			}
			_ => continue,
		};
		out.push(Stroke {
			kind,
			color: edge.color_index,
			points: vec![(x, mid), end],
		});
	}
	out
}

/// Paints a row's strokes and dot, clipped to the gutter so a wide graph
/// never draws over the commit message.
pub fn paint_row_graph(
	window: &mut Window,
	row: &GraphRow,
	strokes: &[Stroke],
	bounds: Bounds<Pixels>,
) {
	window.with_content_mask(Some(ContentMask { bounds }), |window| {
		paint_strokes(window, row, strokes, bounds)
	});
}

fn paint_strokes(
	window: &mut Window,
	row: &GraphRow,
	strokes: &[Stroke],
	bounds: Bounds<Pixels>,
) {
	let mid_y = ROW_HEIGHT * 0.5;
	let ox = bounds.origin.x;
	let oy = bounds.origin.y;
	let at = |(x, y): (f32, f32)| point(ox + px(x), oy + px(y));

	for stroke in strokes {
		let color = palette_rgb(stroke.color);
		let mut builder = match stroke.kind {
			StrokeKind::Rail | StrokeKind::Merge => {
				PathBuilder::stroke(px(1.5))
			}
			StrokeKind::Gap => {
				PathBuilder::stroke(px(1.5)).dash_array(&[px(2.0), px(2.0)])
			}
			StrokeKind::Shallow => {
				PathBuilder::stroke(px(1.5)).dash_array(&[px(3.0), px(2.0)])
			}
		};
		let Some((&first, rest)) = stroke.points.split_first() else {
			continue;
		};
		builder.move_to(at(first));
		match (stroke.kind, rest) {
			(StrokeKind::Merge, [ctrl, end]) => {
				builder.curve_to(at(*end), at(*ctrl))
			}
			_ => rest.iter().for_each(|p| builder.line_to(at(*p))),
		}
		if let Ok(path) = builder.build() {
			window.paint_path(path, color);
		}
		if stroke.kind == StrokeKind::Gap {
			// Collapsed history ends in a gap mark, never at a commit.
			let (x, y) = stroke.points[stroke.points.len() - 1];
			let mut tick = PathBuilder::stroke(px(1.5));
			tick.move_to(at((x - 3.0, y + 1.0)));
			tick.line_to(at((x + 3.0, y - 1.0)));
			if let Ok(path) = tick.build() {
				window.paint_path(path, color);
			}
		}
	}

	// IntelliJ draws every commit as the same filled dot; only a shallow
	// boundary or uncommitted node is hollow.
	let nx = ox + px(lane_x(row.node.lane));
	let ny = oy + px(mid_y);
	let color = palette_rgb(row.node.color_index);
	let r = px(NODE_RADIUS);
	let hollow = matches!(
		row.node.node_type,
		NodeType::ShallowRoot | NodeType::Uncommitted
	);
	window.paint_quad(quad(
		Bounds {
			origin: point(nx - r, ny - r),
			size: size(r * 2.0, r * 2.0),
		},
		r,
		if hollow {
			gpui::transparent_black()
		} else {
			color.into()
		},
		px(if hollow { 1.5 } else { 0.0 }),
		color,
		Default::default(),
	));
}

#[cfg(test)]
mod tests {
	use super::*;

	fn make_summary(
		sha: &str,
		parents: &[&str],
		subject: &str,
	) -> CommitSummary {
		CommitSummary {
			sha: sha.to_string(),
			parents: parents.iter().map(|s| s.to_string()).collect(),
			author_name: "Tester".to_string(),
			author_email: "tester@example.com".to_string(),
			author_date: "2026-09-25T00:00:00Z".to_string(),
			subject: subject.to_string(),
		}
	}

	fn info(raw: &str, display: &str, kind: RefKind) -> RefInfo {
		RefInfo {
			raw_name: raw.into(),
			display_name: display.into(),
			kind,
		}
	}

	#[test]
	fn visible_refs_prefers_current_then_local_and_folds_rest() {
		let remote = info(
			"refs/remotes/origin/main",
			"origin/main",
			RefKind::RemoteBranch {
				remote: "origin".into(),
				name: "main".into(),
			},
		);
		let tag = info("refs/tags/v1", "v1", RefKind::Tag);
		let other = info("refs/heads/other", "other", RefKind::Branch);
		let cur = info("refs/heads/dev", "dev", RefKind::Branch);
		let refs = vec![remote.clone(), tag, other.clone(), cur];
		let (shown, hidden) = visible_refs(&refs, Some("dev"));
		assert_eq!(shown.len(), 1);
		assert_eq!(shown[0].primary.display_name, "dev");
		assert_eq!(hidden, 3);
		let (shown, hidden) = visible_refs(&refs, None);
		assert_eq!(shown[0].primary.display_name, "other");
		assert_eq!(hidden, 3);
		// Two refs fit as-is, local before remote.
		let two = vec![remote, other];
		let (shown, hidden) = visible_refs(&two, None);
		assert_eq!(hidden, 0);
		assert_eq!(shown[0].primary.display_name, "other");
		assert_eq!(shown[1].primary.display_name, "origin/main");
		assert!(visible_refs(&[], None).0.is_empty());
	}

	fn remote(remote: &str, name: &str) -> RefInfo {
		info(
			&format!("refs/remotes/{remote}/{name}"),
			&format!("{remote}/{name}"),
			RefKind::RemoteBranch {
				remote: remote.into(),
				name: name.into(),
			},
		)
	}

	fn local(name: &str) -> RefInfo {
		info(&format!("refs/heads/{name}"), name, RefKind::Branch)
	}

	fn names(b: &RefBadge) -> Vec<String> {
		std::iter::once(b.primary)
			.chain(b.merged.iter().copied())
			.map(|r| r.display_name.clone())
			.collect()
	}

	#[test]
	fn tracking_refs_merge_into_same_named_local_on_same_commit() {
		// Same commit: origin/main folds into main.
		let refs = vec![remote("origin", "main"), local("main")];
		let b = merge_tracking_refs(&refs);
		assert_eq!(b.len(), 1);
		assert_eq!(names(&b[0]), ["main", "origin/main"]);
		// A remote whose local counterpart is on another commit stays alone.
		let refs = vec![remote("origin", "develop"), local("main")];
		let b = merge_tracking_refs(&refs);
		assert_eq!(b.len(), 2);
		assert!(b.iter().all(|b| b.merged.is_empty()));
		// Multiple remotes and the symbolic origin/HEAD fold into one badge.
		let refs = vec![
			remote("origin", "HEAD"),
			remote("upstream", "main"),
			local("main"),
			remote("origin", "main"),
		];
		let b = merge_tracking_refs(&refs);
		assert_eq!(b.len(), 1);
		assert_eq!(
			names(&b[0]),
			["main", "origin/HEAD", "upstream/main", "origin/main"]
		);
		// Without a local, origin/HEAD rides on origin/main; alone it stays.
		let refs = vec![remote("origin", "HEAD"), remote("origin", "x")];
		let b = merge_tracking_refs(&refs);
		assert_eq!(b.len(), 1);
		assert_eq!(names(&b[0]), ["origin/x", "origin/HEAD"]);
		assert_eq!(merge_tracking_refs(&[remote("origin", "HEAD")]).len(), 1);
	}

	#[test]
	fn plus_n_counts_badges_after_merge() {
		// 5 refs -> 3 badges (main+origin/main, dev+origin/dev, v1): 1 + `+2`.
		let refs = vec![
			local("main"),
			remote("origin", "main"),
			local("dev"),
			remote("origin", "dev"),
			info("refs/tags/v1", "v1", RefKind::Tag),
		];
		let (shown, hidden) = visible_refs(&refs, Some("main"));
		assert_eq!(names(&shown[0]), ["main", "origin/main"]);
		assert_eq!(hidden, 2);
		// 4 refs merging to 2 badges fit without `+N`.
		let (shown, hidden) = visible_refs(&refs[..4], None);
		assert_eq!((shown.len(), hidden), (2, 0));
	}

	/// x of every rail end on the row's top (`at_top`) or bottom edge.
	fn edge_xs(strokes: &[Stroke], at_top: bool) -> Vec<i32> {
		let y = if at_top { 0.0 } else { ROW_HEIGHT };
		let mut xs: Vec<i32> = strokes
			.iter()
			.filter(|s| matches!(s.kind, StrokeKind::Rail | StrokeKind::Merge))
			.flat_map(|s| [s.points[0], s.points[s.points.len() - 1]])
			.filter(|p| (p.1 - y).abs() < 0.01)
			.map(|p| (p.0 * 100.0).round() as i32)
			.collect();
		xs.sort_unstable();
		xs.dedup();
		xs
	}

	#[test]
	fn rows_join_at_their_edges_and_bend_where_the_layout_does() {
		// base; branch side; m1; m2; side: s1; main: m3; m4;
		// merge --no-ff side; m5. The old renderer drew s1 as m4's child.
		let commits = [
			make_summary("m5", &["mg"], ""),
			make_summary("mg", &["m4", "s1"], ""),
			make_summary("s1", &["base"], ""),
			make_summary("m4", &["m3"], ""),
			make_summary("m3", &["m2"], ""),
			make_summary("m2", &["m1"], ""),
			make_summary("m1", &["base"], ""),
			make_summary("base", &[], ""),
		];
		for split in 1..commits.len() {
			let first = layout_commits(&commits[..split], &[], None).unwrap();
			let rest = layout_commits_paged(
				&commits[split..],
				&[],
				None,
				first.checkpoint.as_ref(),
			)
			.unwrap();
			let rows: Vec<(&GraphLayout, &GraphRow)> = first
				.rows
				.iter()
				.map(|r| (&first, r))
				.chain(rest.rows.iter().map(|r| (&rest, r)))
				.collect();
			let strokes: Vec<Vec<Stroke>> =
				rows.iter().map(|(l, r)| row_strokes(l, r)).collect();
			for s in strokes.iter().flatten() {
				assert!(s
					.points
					.iter()
					.all(|p| (0.0..=ROW_HEIGHT).contains(&p.1)));
			}
			for i in 1..rows.len() {
				assert_eq!(
					edge_xs(&strokes[i - 1], false),
					edge_xs(&strokes[i], true),
					"split {split}: rows {} and {i} do not meet",
					i - 1
				);
			}
			// A rail runs through s1's dot on to the row's bottom edge.
			let s1 = rows.iter().position(|(_, r)| r.sha == "s1").unwrap();
			let s1_x = lane_x(rows[s1].1.node.lane);
			assert!(strokes[s1].iter().any(|s| s.kind == StrokeKind::Rail
				&& s.points[0].0 == s1_x
				&& s.points[0].1 <= ROW_HEIGHT * 0.5
				&& s.points.last().unwrap().1 == ROW_HEIGHT));
			let base = rows.len() - 1;
			let base_dot = (lane_x(rows[base].1.node.lane), ROW_HEIGHT * 0.5);
			assert!(
				strokes[base]
					.iter()
					.filter(|s| s.points.last() == Some(&base_dot))
					.count() >= 2,
				"split {split}: m1 and s1 both end in base"
			);
		}
	}

	#[test]
	fn page_refs_keep_page_and_parent_refs_and_cap_the_rest() {
		let commits = vec![make_summary("a", &["p"], "")];
		let r = |name: &str, sha: &str| GitReference {
			name: name.into(),
			sha: sha.into(),
		};
		let refs = vec![
			r("refs/heads/far", "zz"),
			r("refs/heads/parent", "p"),
			r("refs/heads/here", "a"),
		];
		let names = |v: Vec<GitReference>| {
			v.into_iter().map(|r| r.name).collect::<Vec<_>>()
		};
		assert_eq!(
			names(page_refs(&commits, &refs, Some("a"), false)),
			["refs/heads/here", "refs/heads/parent"]
		);
		assert_eq!(
			names(page_refs(&commits, &refs, Some("a"), true)),
			["HEAD", "refs/heads/here", "refs/heads/parent"]
		);
		let many: Vec<_> = (0..2000)
			.map(|n| r(&format!("refs/tags/t{n}"), "a"))
			.collect();
		assert_eq!(
			page_refs(&commits, &many, None, false).len(),
			MAX_LAYOUT_REFS
		);
		// A wide graph never grows the gutter past the cap.
		let wide: Vec<_> = (0..60)
			.map(|n| make_summary(&format!("t{n}"), &[&format!("p{n}")], ""))
			.collect();
		let layout = layout_commits(&wide, &[], None).unwrap();
		assert_eq!(
			gutter_width(&layout),
			MAX_GUTTER_LANES as f32 * LANE_WIDTH + 8.0
		);
	}

	#[test]
	fn test_render_geometry_against_layout_parent_oracle() {
		// Realistic DAG fixture:
		// Base: C0
		// Feature branch commit: C1 (parent C0)
		// Unmerged tip commit: C2 (parent C0)
		// Merge commit: M (parents C1, C0)
		let commits = vec![
			make_summary("c2", &["c0"], "unmerged tip"),
			make_summary("m", &["c1", "c0"], "merge feature into main"),
			make_summary("c1", &["c0"], "feature commit"),
			make_summary("c0", &[], "base commit"),
		];

		let refs = vec![
			GitReference {
				name: "refs/heads/main".into(),
				sha: "m".into(),
			},
			GitReference {
				name: "refs/heads/feature".into(),
				sha: "c1".into(),
			},
			GitReference {
				name: "refs/heads/unmerged".into(),
				sha: "c2".into(),
			},
		];

		let layout = layout_commits(&commits, &refs, Some("m")).unwrap();
		assert_eq!(layout.rows.len(), 4);

		// Oracle verification:
		// Row 1 is merge commit 'm'
		let m_row = layout.rows.iter().find(|r| r.sha == "m").unwrap();
		assert_eq!(m_row.node.node_type, NodeType::Merge);
		// Must have 2 parent edges
		assert_eq!(m_row.parent_edges.len(), 2);
		let p0 = &m_row.parent_edges[0];
		let p1 = &m_row.parent_edges[1];
		assert_eq!(p0.parent_sha, "c1");
		assert_eq!(p1.parent_sha, "c0");

		// One parent edge must connect to a separate lane (divergent branch)
		assert!(p0.from_lane != p1.to_lane || p1.from_lane != p0.to_lane);

		// Geometry dimensions
		let gw = gutter_width(&layout);
		assert!(gw >= 40.0, "gutter width must accommodate lanes: {gw}");
		assert_eq!(lane_x(0), OFFSET_X);
		assert_eq!(lane_x(1), OFFSET_X + LANE_WIDTH);
	}

	#[test]
	fn test_bounded_history_paging_stable_checkpoint() {
		let commits = [
			make_summary("c3", &["c2"], "commit 3"),
			make_summary("c2", &["c1"], "commit 2"),
			make_summary("c1", &["c0"], "commit 1"),
			make_summary("c0", &[], "commit 0"),
		];

		// Page 1: first 2 commits
		let page1 =
			layout_commits_paged(&commits[..2], &[], None, None).unwrap();
		assert_eq!(page1.rows.len(), 2);
		assert!(
			page1.checkpoint.is_some(),
			"checkpoint must be created at page boundary"
		);
		let cp1 = page1.checkpoint.as_ref().unwrap();

		// Page 2: remaining 2 commits using checkpoint
		let page2 =
			layout_commits_paged(&commits[2..], &[], None, Some(cp1)).unwrap();
		assert_eq!(page2.rows.len(), 2);

		// Continuity verification: page 2 row 0 must connect from checkpoint rail
		assert_eq!(page2.rows[0].global_row, 2);
	}

	#[test]
	fn collapsed_merge_edge_is_a_filtered_gap_not_a_false_parent() {
		// m merges f (f -> base) into main (a -> base); collapse hides f.
		let shown = vec![
			make_summary("m", &["a", "f"], "merge"),
			make_summary("a", &["base"], "main"),
			make_summary("base", &[], "base"),
		];
		let hidden: std::collections::HashSet<String> =
			["f".to_string()].into();
		let layout = layout_page(
			&shown,
			&[],
			Some("m"),
			None,
			false,
			hidden,
			HashSet::new(),
		)
		.unwrap();
		let m = &layout.rows[0];
		let side = m.parent_edges.iter().find(|e| e.parent_sha == "f").unwrap();
		assert_eq!(side.continuation, ContinuationKind::FilteredGap);
		assert_eq!(
			side.to_row, None,
			"hidden parent must not resolve to a row"
		);
		let main = m.parent_edges.iter().find(|e| e.parent_sha == "a").unwrap();
		assert_eq!(main.continuation, ContinuationKind::Resolved);
		// No row claims a parent edge into a commit that is not its parent.
		for r in &layout.rows {
			for e in &r.parent_edges {
				if let Some(t) = e.to_row {
					assert_eq!(layout.rows[t].sha, e.parent_sha);
				}
			}
		}
	}
}
