//! Git commit graph layout view using snip_core::graph.
//!
//! Provides topological DAG graph calculation, vector canvas rails and merge curves,
//! passing lanes, ref badges (HEAD, branches, remotes, tags), bounded history paging,
//! and historical commit file inspection.

use gpui::{
	point, px, quad, rgb, size, Bounds, PathBuilder, Pixels, Rgba, Window,
};
use snip_core::browser::{CommitSummary, GitReference};
use snip_core::graph::{
	compute_graph_layout, ContinuationKind, GraphCheckpoint, GraphConfig,
	GraphLayout, GraphRow, NodeType, RefInfo, RefKind, COLOR_PALETTE,
};

pub const ROW_HEIGHT: f32 = crate::theme::ROW_H;
pub const LANE_WIDTH: f32 = 16.0;
pub const OFFSET_X: f32 = 14.0;

pub fn palette_rgb(index: usize) -> Rgba {
	let hex = COLOR_PALETTE[index % COLOR_PALETTE.len()];
	let hex = hex.trim_start_matches('#');
	let r = u8::from_str_radix(&hex[0..2], 16).unwrap_or(137);
	let g = u8::from_str_radix(&hex[2..4], 16).unwrap_or(180);
	let b = u8::from_str_radix(&hex[4..6], 16).unwrap_or(250);
	rgb(u32::from_be_bytes([0, r, g, b]))
}

pub fn lane_x(lane: usize) -> f32 {
	OFFSET_X + (lane as f32) * LANE_WIDTH
}

pub fn gutter_width(layout: &GraphLayout) -> f32 {
	let max_lane = layout.rows.iter().map(|r| r.max_lane).max().unwrap_or(0);
	((max_lane + 1) as f32) * LANE_WIDTH + 24.0
}

/// Label text and text color for a ref; the background is the shared `REF_BG`.
pub fn format_ref_badge(info: &RefInfo) -> (String, Rgba) {
	use crate::theme::{REF_HEAD, REF_LOCAL, REF_REMOTE, REF_TAG, TEXT_MUTED};
	match &info.kind {
		RefKind::Head => ("HEAD".to_string(), rgb(REF_HEAD)),
		RefKind::Branch => (info.display_name.clone(), rgb(REF_LOCAL)),
		RefKind::RemoteBranch { remote, name } => {
			(format!("{remote}/{name}"), rgb(REF_REMOTE))
		}
		RefKind::Tag => (format!("tag: {}", info.display_name), rgb(REF_TAG)),
		RefKind::Other => (info.display_name.clone(), rgb(TEXT_MUTED)),
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
	let config = GraphConfig {
		max_rows: 500,
		max_frontier_size: 64,
		max_refs: 1000,
		unit_x: LANE_WIDTH as f64,
		unit_y: ROW_HEIGHT as f64,
		offset_x: OFFSET_X as f64,
		..Default::default()
	};
	compute_graph_layout(commits, refs, head_sha, &config, checkpoint)
		.map_err(|e| format!("Graph layout error: {e}"))
}

/// Layout where `filtered` commits are known to be hidden: edges into them
/// become `FilteredGap` instead of looking like page boundaries.
pub fn layout_commits_filtered(
	commits: &[CommitSummary],
	refs: &[GitReference],
	head_sha: Option<&str>,
	checkpoint: Option<&GraphCheckpoint>,
	filtered: std::collections::HashSet<String>,
) -> Result<GraphLayout, String> {
	let config = GraphConfig {
		max_rows: 500,
		max_frontier_size: 64,
		max_refs: 1000,
		unit_x: LANE_WIDTH as f64,
		unit_y: ROW_HEIGHT as f64,
		offset_x: OFFSET_X as f64,
		filtered_commits: filtered,
		..Default::default()
	};
	compute_graph_layout(commits, refs, head_sha, &config, checkpoint)
		.map_err(|e| format!("Graph layout error: {e}"))
}

/// Paints the commit row's vector graph rails, passing lanes, curves, and node glyph onto the canvas.
pub fn paint_row_graph(
	window: &mut Window,
	row: &GraphRow,
	incoming_from_above: bool,
	bounds: Bounds<Pixels>,
) {
	let mid_y = ROW_HEIGHT * 0.5;
	let ox = bounds.origin.x;
	let oy = bounds.origin.y;

	// 1. Passing lanes: straight vertical line passing uninterrupted through this row
	for p in &row.passing_lanes {
		let x = lane_x(p.lane);
		let color = palette_rgb(p.color_index);
		let mut builder = PathBuilder::stroke(px(2.0));
		builder.move_to(point(ox + px(x), oy + px(0.0)));
		builder.line_to(point(ox + px(x), oy + px(ROW_HEIGHT)));
		if let Ok(path) = builder.build() {
			window.paint_path(path, color);
		}
	}

	// 2. Incoming rail from row above to this commit node
	if incoming_from_above {
		let x = lane_x(row.node.lane);
		let color = palette_rgb(row.node.color_index);
		let mut builder = PathBuilder::stroke(px(2.0));
		builder.move_to(point(ox + px(x), oy + px(0.0)));
		builder.line_to(point(ox + px(x), oy + px(mid_y)));
		if let Ok(path) = builder.build() {
			window.paint_path(path, color);
		}
	}

	// 3. Outgoing parent edges (connecting downwards to parents or page boundary)
	for edge in &row.parent_edges {
		let from_x = lane_x(edge.from_lane);
		let to_x = lane_x(edge.to_lane);
		let color = palette_rgb(edge.color_index);

		if edge.continuation == ContinuationKind::FilteredGap {
			// Collapsed / filtered history: a dotted stub that stops short
			// with a gap mark, never a line into another commit.
			let end_x = from_x + (to_x - from_x) * 0.5;
			let end_y = mid_y + (ROW_HEIGHT - mid_y) * 0.7;
			let mut builder =
				PathBuilder::stroke(px(1.5)).dash_array(&[px(2.0), px(2.0)]);
			builder.move_to(point(ox + px(from_x), oy + px(mid_y)));
			builder.line_to(point(ox + px(end_x), oy + px(end_y)));
			if let Ok(path) = builder.build() {
				window.paint_path(path, color);
			}
			let mut tick = PathBuilder::stroke(px(1.5));
			tick.move_to(point(ox + px(end_x - 3.0), oy + px(end_y + 1.0)));
			tick.line_to(point(ox + px(end_x + 3.0), oy + px(end_y - 1.0)));
			if let Ok(path) = tick.build() {
				window.paint_path(path, color);
			}
			continue;
		}

		if edge.from_lane == edge.to_lane {
			if edge.continuation == ContinuationKind::UnresolvedPageBoundary {
				// Page boundary continuation: dashed line
				let mut builder = PathBuilder::stroke(px(2.0))
					.dash_array(&[px(3.0), px(2.0)]);
				builder.move_to(point(ox + px(from_x), oy + px(mid_y)));
				builder.line_to(point(ox + px(from_x), oy + px(ROW_HEIGHT)));
				if let Ok(path) = builder.build() {
					window.paint_path(path, color);
				}
			} else if edge.continuation != ContinuationKind::Terminated {
				let mut builder = PathBuilder::stroke(px(2.0));
				builder.move_to(point(ox + px(from_x), oy + px(mid_y)));
				builder.line_to(point(ox + px(from_x), oy + px(ROW_HEIGHT)));
				if let Ok(path) = builder.build() {
					window.paint_path(path, color);
				}
			} else {
				// Root commit: short stub with stop crossbar
				let mut builder = PathBuilder::stroke(px(2.0));
				builder.move_to(point(ox + px(from_x), oy + px(mid_y)));
				builder.line_to(point(ox + px(from_x), oy + px(mid_y + 6.0)));
				builder.move_to(point(
					ox + px(from_x - 3.0),
					oy + px(mid_y + 6.0),
				));
				builder.line_to(point(
					ox + px(from_x + 3.0),
					oy + px(mid_y + 6.0),
				));
				if let Ok(path) = builder.build() {
					window.paint_path(path, color);
				}
			}
		} else {
			// Branch fork or merge curve: smooth cubic Bézier S-curve connecting from_lane to to_lane
			let mut builder = PathBuilder::stroke(px(2.0));
			let p_start = point(ox + px(from_x), oy + px(mid_y));
			let p_end = point(ox + px(to_x), oy + px(ROW_HEIGHT));
			let p_c1 = point(
				ox + px(from_x),
				oy + px(mid_y + (ROW_HEIGHT - mid_y) * 0.5),
			);
			let p_c2 = point(
				ox + px(to_x),
				oy + px(mid_y + (ROW_HEIGHT - mid_y) * 0.5),
			);
			builder.move_to(p_start);
			builder.cubic_bezier_to(p_end, p_c1, p_c2);
			if let Ok(path) = builder.build() {
				window.paint_path(path, color);
			}
		}
	}

	// 4. Commit node glyph (Merge diamond, Head double circle, Normal circle, Hollow boundary)
	let nx = ox + px(lane_x(row.node.lane));
	let ny = oy + px(mid_y);
	let color = palette_rgb(row.node.color_index);

	match row.node.node_type {
		NodeType::Merge => {
			let r = px(5.5);
			let mut builder = PathBuilder::fill();
			builder.move_to(point(nx, ny - r));
			builder.line_to(point(nx + r, ny));
			builder.line_to(point(nx, ny + r));
			builder.line_to(point(nx - r, ny));
			builder.close();
			if let Ok(path) = builder.build() {
				window.paint_path(path, color);
			}
		}
		NodeType::Head => {
			let r_out = px(6.0);
			let r_in = px(2.5);
			let out_bounds = Bounds {
				origin: point(nx - r_out, ny - r_out),
				size: size(r_out * 2.0, r_out * 2.0),
			};
			window.paint_quad(quad(
				out_bounds,
				r_out,
				color,
				px(0.0),
				gpui::transparent_black(),
				Default::default(),
			));
			let in_bounds = Bounds {
				origin: point(nx - r_in, ny - r_in),
				size: size(r_in * 2.0, r_in * 2.0),
			};
			window.paint_quad(quad(
				in_bounds,
				r_in,
				gpui::white(),
				px(0.0),
				gpui::transparent_black(),
				Default::default(),
			));
		}
		NodeType::Normal => {
			let r = px(4.5);
			let circle_bounds = Bounds {
				origin: point(nx - r, ny - r),
				size: size(r * 2.0, r * 2.0),
			};
			window.paint_quad(quad(
				circle_bounds,
				r,
				color,
				px(0.0),
				gpui::transparent_black(),
				Default::default(),
			));
		}
		NodeType::ShallowRoot | NodeType::Uncommitted => {
			let r = px(4.5);
			let circle_bounds = Bounds {
				origin: point(nx - r, ny - r),
				size: size(r * 2.0, r * 2.0),
			};
			window.paint_quad(quad(
				circle_bounds,
				r,
				gpui::transparent_black(),
				px(1.5),
				color,
				Default::default(),
			));
		}
	}
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
		let layout =
			layout_commits_filtered(&shown, &[], Some("m"), None, hidden)
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
