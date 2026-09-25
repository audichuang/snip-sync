// Graph layout algorithm ported from SourceGit (https://github.com/sourcegit-scm/sourcegit)
// and ClipCodeVSCode / snip-sync git-graph-builder.ts.
//
// MIT License
//
// Copyright (c) SourceGit contributors, licensed under MIT License.
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

//! Pure Rust Git commit graph layout engine.
//!
//! Provides deterministic topological layout, stable branch lane and color
//! assignment, bounded-frontier sequential page processing, explicit boundary
//! representations for unresolved/shallow/filtered parents, and
//! renderer-neutral data structures suitable for native GPUI virtual list
//! rendering and vector Canvas drawing.
//!
//! # Architecture & UI Integration
//!
//! The layout algorithm consumes [`CommitSummary`] items and optional
//! [`GitReference`] items, emitting a [`GraphLayout`]. The layout provides:
//! - Per-row discrete lanes, commit dot classification, and active passing
//!   lanes in [`GraphRow`] for row-by-row virtual lists.
//! - Direct vector paths ([`GraphPath`]) and cubic/quadratic bezier merge curves
//!   ([`GraphLink`]) for canvas rendering.
//! - Consistent renderer-facing `rail_id` assignments linking nodes, passing
//!   lanes, merge links, and parent continuation edges.
//! - Explicit continuation semantics ([`ContinuationKind`]) ensuring parents
//!   that cross page or shallow boundaries are never misrepresented as root
//!   commits or false connections.
//! - A strictly bounded [`GraphCheckpoint`] capturing active frontier rails
//!   without retaining whole-history commit caches, guaranteeing O(frontier)
//!   memory across arbitrarily large repositories.
//!
//! # Example
//!
//! ```rust
//! use snip_core::browser::{CommitSummary, GitReference};
//! use snip_core::graph::{compute_graph_layout, GraphConfig};
//!
//! let commits = vec![
//!     CommitSummary {
//!         sha: "c2".into(),
//!         parents: vec!["c1".into()],
//!         author_name: "Developer".into(),
//!         author_email: "dev@example.com".into(),
//!         author_date: "2026-09-25T00:00:00Z".into(),
//!         subject: "feat: branch commit".into(),
//!     },
//!     CommitSummary {
//!         sha: "c1".into(),
//!         parents: vec![],
//!         author_name: "Developer".into(),
//!         author_email: "dev@example.com".into(),
//!         author_date: "2026-09-24T00:00:00Z".into(),
//!         subject: "initial commit".into(),
//!     },
//! ];
//! let refs = vec![GitReference {
//!     name: "refs/heads/main".into(),
//!     sha: "c2".into(),
//! }];
//! let layout = compute_graph_layout(
//!     &commits,
//!     &refs,
//!     Some("c2"),
//!     &GraphConfig::default(),
//!     None,
//! )
//! .unwrap();
//! assert_eq!(layout.rows.len(), 2);
//! assert_eq!(layout.rows[0].node.lane, 0);
//! ```

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::browser::{CommitSummary, GitReference};

/// Standard 12-color graph palette ported from SourceGit / ClipCodeVSCode.
pub const COLOR_PALETTE: [&str; 12] = [
	"#63b0f4", "#73d13d", "#ff7a45", "#b37feb", "#f759ab", "#36cfc9",
	"#ffc53d", "#ff4d4f", "#597ef7", "#9254de", "#43e8d8", "#faad14",
];

/// Maximum allowed byte length for a Git SHA (SHA-1 is 40 hex chars, SHA-256 is 64).
pub const MAX_SHA_LEN: usize = 128;

/// Maximum allowed byte length for a Git reference name.
pub const MAX_REF_NAME_LEN: usize = 1024;

/// Deterministic djb2-like string hash mapping branch ref names to a palette
/// index.
///
/// Uses UTF-16 code units (`encode_utf16`) to maintain exact parity with the
/// TypeScript reference implementation (`name.charCodeAt(i)`) across Chinese,
/// multi-byte CJK, and non-BMP surrogate pair ref names (G1).
pub fn hash_string_to_index(name: &str) -> usize {
	let mut h: u32 = 0;
	for code_unit in name.encode_utf16() {
		h = h.wrapping_mul(31).wrapping_add(code_unit as u32);
	}
	(h as usize) % COLOR_PALETTE.len()
}

/// Chooses the first available palette color index not present in `used_mask`.
/// If `preferred` is given and free, it is selected. If all colors are in use,
/// falls back deterministically to 0.
pub fn pick_color(used_mask: u32, preferred: Option<usize>) -> usize {
	if let Some(pref) = preferred {
		if pref < COLOR_PALETTE.len() && (used_mask & (1 << pref)) == 0 {
			return pref;
		}
	}
	for i in 0..COLOR_PALETTE.len() {
		if (used_mask & (1 << i)) == 0 {
			return i;
		}
	}
	0
}

/// 2D geometric point in layout coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Point {
	pub x: f64,
	pub y: f64,
}

/// Continuous rail path consisting of line and curve points.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphPath {
	pub points: Vec<Point>,
	pub color: usize,
	pub color_override: Option<String>,
	pub highlighted: bool,
	/// Unique rail identifier matching [`GraphNode::rail_id`] and
	/// [`ParentEdge::rail_id`].
	pub rail_id: usize,
}

/// Curved merge link connecting a commit to a merge parent's rail.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphLink {
	pub start: Point,
	pub control: Point,
	pub end: Point,
	pub color: usize,
	pub color_override: Option<String>,
	pub highlighted: bool,
	/// Unique identifier of the target rail this merge curve connects into.
	pub target_rail_id: usize,
}

/// Visual classification of a commit node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NodeType {
	/// Normal commit with a single parent.
	Normal,
	/// Commit carrying a HEAD reference (when not a merge).
	Head,
	/// Merge commit with two or more parents.
	Merge,
	/// Boundary commit in a shallow clone where parents are not fetched.
	ShallowRoot,
	/// Synthetic uncommitted working copy changes.
	Uncommitted,
}

/// Semantic node layout placed at a specific commit row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphNode {
	/// 0-indexed column lane.
	pub lane: usize,
	/// 0-indexed palette color index.
	pub color_index: usize,
	/// Optional hex color override (e.g. from user branch config).
	pub color_override: Option<String>,
	/// Visual style classification.
	pub node_type: NodeType,
	/// True if this commit carries HEAD.
	pub is_head: bool,
	/// True if this commit is reachable from HEAD (or if HEAD is unknown).
	pub highlighted: bool,
	/// Unique rail identifier this node sits on, if any.
	pub rail_id: Option<usize>,
}

/// Continuation semantics of an edge connecting a commit to a parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ContinuationKind {
	/// Parent is resolved within the loaded commits (connecting to another
	/// row).
	Resolved,
	/// Parent is unresolved because it crosses the current page boundary.
	/// The active rail continues into the checkpoint frontier.
	UnresolvedPageBoundary,
	/// Parent is at a Git shallow clone / graft boundary (not present in
	/// repo).
	ShallowBoundary,
	/// Intermediate history was filtered out (explicit gap representation).
	FilteredGap,
	/// Root commit or terminated rail (no parent).
	Terminated,
}

/// Outgoing edge from a commit row to one of its parents.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParentEdge {
	/// Commit SHA of the parent.
	pub parent_sha: String,
	/// 0-indexed parent index (0 = first parent, 1 = merge parent, etc.).
	pub parent_index: usize,
	/// Originating lane of the child commit.
	pub from_lane: usize,
	/// Target lane for this parent connection.
	pub to_lane: usize,
	/// Target row index if resolved within the current layout page.
	pub to_row: Option<usize>,
	/// Palette color index.
	pub color_index: usize,
	/// Optional color override.
	pub color_override: Option<String>,
	/// Continuation semantics.
	pub continuation: ContinuationKind,
	/// Unique rail identifier for this connection matching the target path or
	/// frontier rail.
	pub rail_id: usize,
}

/// An active rail passing vertically through a row without a commit node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PassingLane {
	pub lane: usize,
	pub color_index: usize,
	pub color_override: Option<String>,
	pub rail_id: usize,
}

/// Classified Git reference attached to a commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RefKind {
	Head,
	Branch,
	RemoteBranch { remote: String, name: String },
	Tag,
	Other,
}

/// Preserved reference metadata attached to a layout row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefInfo {
	pub raw_name: String,
	pub display_name: String,
	pub kind: RefKind,
}

/// Complete layout information for a single commit row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphRow {
	/// Commit SHA.
	pub sha: String,
	/// 0-indexed row within this page.
	pub row: usize,
	/// Global row index across paged iterations.
	pub global_row: usize,
	/// Commit dot node placed on this row.
	pub node: GraphNode,
	/// Active rails passing through this row.
	pub passing_lanes: Vec<PassingLane>,
	/// Connections to parent commits or boundaries.
	pub parent_edges: Vec<ParentEdge>,
	/// References pointing to this commit.
	pub refs: Vec<RefInfo>,
	/// Max lane index used in this row, useful for indenting commit messages.
	pub max_lane: usize,
}

/// Active rail in the frontier checkpoint between sequential layout pages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrontierRail {
	/// SHA of the parent commit this rail is waiting for.
	pub next_sha: String,
	/// Current lane index.
	pub lane: usize,
	/// Last X coordinate.
	pub last_x: f64,
	/// Last Y coordinate.
	pub last_y: f64,
	/// Color palette index.
	pub color_index: usize,
	/// Optional color override.
	pub color_override: Option<String>,
	/// Highlighted status propagated from HEAD ancestors.
	pub highlighted: bool,
	/// Unique rail ID.
	pub rail_id: usize,
}

/// Checkpoint preserving bounded frontier state across sequential pages.
///
/// Memory footprint is strictly O(frontier_size) and never scales with total
/// history depth. Historical duplicate checking across pages is delegated to
/// the Git stream/cursor layer to preserve strict O(1) page-step overhead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphCheckpoint {
	/// Active rails awaiting parent commits across the boundary.
	pub frontier: Vec<FrontierRail>,
	/// Next global row index to assign.
	pub next_global_row: usize,
	/// Next rail ID counter.
	pub next_rail_id: usize,
	/// Mask of palette colors in use in the frontier.
	pub used_colors_mask: u32,
	/// SHA of the last commit processed in the prior page, for continuity
	/// checks.
	pub last_seen_sha: Option<String>,
}

/// Configuration options for the graph layout engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphConfig {
	/// Maximum number of concurrent active rails in the frontier.
	/// Default: 64. Prevents unbounded memory growth in wide octopus merges.
	pub max_frontier_size: usize,
	/// Maximum number of commit rows processed in a single layout invocation.
	/// Default: 10_000.
	pub max_rows: usize,
	/// Maximum number of Git references processed in a single layout invocation.
	/// Default: 10_000. Prevents unbounded memory consumption when callers pass
	/// large reference sets.
	pub max_refs: usize,
	/// Maximum total bytes across all input strings in a single layout invocation.
	/// Default: 10 MiB (10 * 1024 * 1024).
	pub max_total_bytes: usize,
	/// Horizontal column spacing in coordinates. Default: 12.0. Must be
	/// positive and finite.
	pub unit_x: f64,
	/// Vertical row spacing in coordinates. Default: 1.0. Must be positive and
	/// finite.
	pub unit_y: f64,
	/// Horizontal offset of column 0. Default: 10.0 (SourceGit standard: 4 - 6
	/// + 12 = 10). Must be non-negative and finite.
	pub offset_x: f64,
	/// Set of commit SHAs known to be shallow clone boundaries.
	pub shallow_roots: HashSet<String>,
	/// Set of commit SHAs explicitly known to be missing due to active path or
	/// revision filtering. Missing parents not in this set are treated as
	/// [`ContinuationKind::UnresolvedPageBoundary`].
	pub filtered_commits: HashSet<String>,
}

impl Default for GraphConfig {
	fn default() -> Self {
		Self {
			max_frontier_size: 64,
			max_rows: 10_000,
			max_refs: 10_000,
			max_total_bytes: 10 * 1024 * 1024,
			unit_x: 12.0,
			unit_y: 1.0,
			offset_x: 10.0,
			shallow_roots: HashSet::new(),
			filtered_commits: HashSet::new(),
		}
	}
}

/// Errors returned by the layout engine.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GraphError {
	#[error("invalid configuration: {message}")]
	InvalidConfig { message: String },

	#[error(
		"frontier limit exceeded: active rails ({current}) exceeded limit ({limit})"
	)]
	FrontierLimitExceeded { current: usize, limit: usize },

	#[error(
		"input rows limit exceeded: requested {requested} rows, limit is {limit}"
	)]
	RowLimitExceeded { requested: usize, limit: usize },

	#[error(
		"input refs limit exceeded: requested {requested} refs, limit is {limit}"
	)]
	RefLimitExceeded { requested: usize, limit: usize },

	#[error(
		"input bytes limit exceeded: total {total} bytes, limit is {limit}"
	)]
	ByteLimitExceeded { total: usize, limit: usize },

	#[error("invalid input: {message}")]
	InvalidInput { message: String },

	#[error("duplicate commit detected within page: sha {sha} at row {row}")]
	DuplicateCommit { sha: String, row: usize },

	#[error("page continuity error: {message}")]
	ContinuityError { message: String },
}

/// Output of the graph layout engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphLayout {
	/// Row-by-row layout entries for virtual list rendering.
	pub rows: Vec<GraphRow>,
	/// Continuous rail paths with bend coordinates.
	pub paths: Vec<GraphPath>,
	/// Merge link curves.
	pub links: Vec<GraphLink>,
	/// Left margin for each commit message in coordinates.
	pub commit_left_margin: Vec<f64>,
	/// Frontier checkpoint if sequential paging continues.
	pub checkpoint: Option<GraphCheckpoint>,
	/// True if layout was generated using the linear fallback mode.
	pub is_fallback: bool,
}

/// Parses raw Git reference names into structured [`RefInfo`].
pub fn parse_ref_name(name: &str) -> RefInfo {
	if name == "HEAD" || name == "refs/heads/HEAD" {
		RefInfo {
			raw_name: name.to_string(),
			display_name: "HEAD".to_string(),
			kind: RefKind::Head,
		}
	} else if let Some(branch) = name.strip_prefix("refs/heads/") {
		RefInfo {
			raw_name: name.to_string(),
			display_name: branch.to_string(),
			kind: RefKind::Branch,
		}
	} else if let Some(remote_ref) = name.strip_prefix("refs/remotes/") {
		let parts: Vec<&str> = remote_ref.splitn(2, '/').collect();
		if parts.len() == 2 {
			RefInfo {
				raw_name: name.to_string(),
				display_name: remote_ref.to_string(),
				kind: RefKind::RemoteBranch {
					remote: parts[0].to_string(),
					name: parts[1].to_string(),
				},
			}
		} else {
			RefInfo {
				raw_name: name.to_string(),
				display_name: remote_ref.to_string(),
				kind: RefKind::Other,
			}
		}
	} else if let Some(tag) = name.strip_prefix("refs/tags/") {
		RefInfo {
			raw_name: name.to_string(),
			display_name: tag.to_string(),
			kind: RefKind::Tag,
		}
	} else {
		RefInfo {
			raw_name: name.to_string(),
			display_name: name.to_string(),
			kind: RefKind::Other,
		}
	}
}

// ── Internal PathHelper (SourceGit faithful) ──

struct PathHelper {
	next: String,
	color: usize,
	color_override: Option<String>,
	highlighted: bool,
	rail_id: usize,
	points: Vec<Point>,
	last_x: f64,
	last_y: f64,
	end_y: f64,
}

impl PathHelper {
	fn new_start(
		next: String,
		color: usize,
		start: Point,
		rail_id: usize,
	) -> Self {
		Self {
			next,
			color,
			color_override: None,
			highlighted: false,
			rail_id,
			points: vec![start],
			last_x: start.x,
			last_y: start.y,
			end_y: start.y,
		}
	}

	fn new_merge(
		next: String,
		color: usize,
		start: Point,
		to: Point,
		rail_id: usize,
	) -> Self {
		Self {
			next,
			color,
			color_override: None,
			highlighted: false,
			rail_id,
			points: vec![start, to],
			last_x: to.x,
			last_y: to.y,
			end_y: to.y,
		}
	}

	fn from_frontier(rail: &FrontierRail) -> Self {
		let pt = Point {
			x: rail.last_x,
			y: rail.last_y,
		};
		Self {
			next: rail.next_sha.clone(),
			color: rail.color_index,
			color_override: rail.color_override.clone(),
			highlighted: rail.highlighted,
			rail_id: rail.rail_id,
			points: vec![pt],
			last_x: rail.last_x,
			last_y: rail.last_y,
			end_y: rail.last_y,
		}
	}

	fn add_point(&mut self, x: f64, y: f64) {
		if self.end_y < y {
			self.points.push(Point { x, y });
			self.end_y = y;
		}
	}

	fn pass(&mut self, x: f64, y: f64, half_h: f64) {
		if x > self.last_x {
			self.add_point(self.last_x, self.last_y);
			self.add_point(x, y - half_h);
		} else if x < self.last_x {
			self.add_point(self.last_x, y - half_h);
			let next_y = y + half_h;
			self.add_point(x, next_y);
		}
		self.last_x = x;
		self.last_y = y;
	}

	fn goto(&mut self, x: f64, y: f64, half_h: f64) {
		if x > self.last_x {
			self.add_point(self.last_x, self.last_y);
			self.add_point(x, y - half_h);
		} else if x < self.last_x {
			let mut min_y = y - half_h;
			if min_y > self.last_y {
				min_y -= half_h;
			}
			self.add_point(self.last_x, min_y);
			self.add_point(x, y);
		}
		self.last_x = x;
		self.last_y = y;
	}

	fn end(&mut self, x: f64, y: f64, half_h: f64) {
		if x > self.last_x {
			self.add_point(self.last_x, self.last_y);
			self.add_point(x, y - half_h);
		} else if x < self.last_x {
			self.add_point(self.last_x, y - half_h);
		}
		self.add_point(x, y);
		self.last_x = x;
		self.last_y = y;
	}

	fn to_graph_path(&self) -> GraphPath {
		GraphPath {
			points: self.points.clone(),
			color: self.color,
			color_override: self.color_override.clone(),
			highlighted: self.highlighted,
			rail_id: self.rail_id,
		}
	}
}

// ── Naming Ref & Reachability Analysis ──

fn find_naming_ref<'a>(
	commit_sha: &str,
	refs_by_commit: &HashMap<&str, Vec<&'a RefInfo>>,
) -> Option<&'a RefInfo> {
	let refs = refs_by_commit.get(commit_sha)?;
	refs.iter()
		.find(|r| r.kind == RefKind::Head)
		.or_else(|| refs.iter().find(|r| r.kind == RefKind::Branch))
		.or_else(|| {
			refs.iter()
				.find(|r| matches!(r.kind, RefKind::RemoteBranch { .. }))
		})
		.copied()
}

fn preferred_index_for_commit(
	commit_sha: &str,
	refs_by_commit: &HashMap<&str, Vec<&RefInfo>>,
) -> Option<usize> {
	let r = find_naming_ref(commit_sha, refs_by_commit)?;
	Some(hash_string_to_index(&r.display_name))
}

fn validate_config(config: &GraphConfig) -> Result<(), GraphError> {
	if config.max_frontier_size == 0 {
		return Err(GraphError::InvalidConfig {
			message: "max_frontier_size must be greater than 0".to_string(),
		});
	}
	if config.max_rows == 0 {
		return Err(GraphError::InvalidConfig {
			message: "max_rows must be greater than 0".to_string(),
		});
	}
	if config.max_refs == 0 {
		return Err(GraphError::InvalidConfig {
			message: "max_refs must be greater than 0".to_string(),
		});
	}
	if config.max_total_bytes == 0 {
		return Err(GraphError::InvalidConfig {
			message: "max_total_bytes must be greater than 0".to_string(),
		});
	}
	if !config.unit_x.is_finite() || config.unit_x <= 0.0 {
		return Err(GraphError::InvalidConfig {
			message: "unit_x must be positive and finite".to_string(),
		});
	}
	if !config.unit_y.is_finite() || config.unit_y <= 0.0 {
		return Err(GraphError::InvalidConfig {
			message: "unit_y must be positive and finite".to_string(),
		});
	}
	if !config.offset_x.is_finite() || config.offset_x < 0.0 {
		return Err(GraphError::InvalidConfig {
			message: "offset_x must be non-negative and finite".to_string(),
		});
	}
	Ok(())
}

fn validate_input_bounds(
	commits: &[CommitSummary],
	refs: &[GitReference],
	config: &GraphConfig,
	checkpoint: Option<&GraphCheckpoint>,
) -> Result<(), GraphError> {
	if commits.len() > config.max_rows {
		return Err(GraphError::RowLimitExceeded {
			requested: commits.len(),
			limit: config.max_rows,
		});
	}
	if refs.len() > config.max_refs {
		return Err(GraphError::RefLimitExceeded {
			requested: refs.len(),
			limit: config.max_refs,
		});
	}

	let mut total_bytes: usize = 0;

	for (row, c) in commits.iter().enumerate() {
		if c.sha.is_empty() || c.sha.len() > MAX_SHA_LEN {
			return Err(GraphError::InvalidInput {
				message: format!(
					"commit SHA at row {row} is empty or exceeds {MAX_SHA_LEN} bytes"
				),
			});
		}
		total_bytes = total_bytes.saturating_add(c.sha.len());
		total_bytes = total_bytes.saturating_add(c.author_name.len());
		total_bytes = total_bytes.saturating_add(c.author_email.len());
		total_bytes = total_bytes.saturating_add(c.author_date.len());
		total_bytes = total_bytes.saturating_add(c.subject.len());

		for (pi, p) in c.parents.iter().enumerate() {
			if p.is_empty() || p.len() > MAX_SHA_LEN {
				return Err(GraphError::InvalidInput {
					message: format!(
						"parent SHA {pi} for commit {} is empty or exceeds {MAX_SHA_LEN} bytes",
						c.sha
					),
				});
			}
			total_bytes = total_bytes.saturating_add(p.len());
		}
	}

	for (i, r) in refs.iter().enumerate() {
		if r.sha.is_empty() || r.sha.len() > MAX_SHA_LEN {
			return Err(GraphError::InvalidInput {
				message: format!(
					"ref SHA at index {i} is empty or exceeds {MAX_SHA_LEN} bytes"
				),
			});
		}
		if r.name.is_empty() || r.name.len() > MAX_REF_NAME_LEN {
			return Err(GraphError::InvalidInput {
				message: format!(
					"ref name at index {i} is empty or exceeds {MAX_REF_NAME_LEN} bytes"
				),
			});
		}
		total_bytes = total_bytes.saturating_add(r.sha.len());
		total_bytes = total_bytes.saturating_add(r.name.len());
	}

	if let Some(cp) = checkpoint {
		for rail in &cp.frontier {
			if rail.next_sha.is_empty() || rail.next_sha.len() > MAX_SHA_LEN {
				return Err(GraphError::InvalidInput {
					message: format!(
						"frontier rail next_sha is empty or exceeds {MAX_SHA_LEN} bytes"
					),
				});
			}
			total_bytes = total_bytes.saturating_add(rail.next_sha.len());
			if let Some(ref co) = rail.color_override {
				total_bytes = total_bytes.saturating_add(co.len());
			}
		}
		if let Some(ref last_sha) = cp.last_seen_sha {
			if last_sha.is_empty() || last_sha.len() > MAX_SHA_LEN {
				return Err(GraphError::InvalidInput {
					message: format!(
						"checkpoint last_seen_sha is empty or exceeds {MAX_SHA_LEN} bytes"
					),
				});
			}
			total_bytes = total_bytes.saturating_add(last_sha.len());
		}
	}

	if total_bytes > config.max_total_bytes {
		return Err(GraphError::ByteLimitExceeded {
			total: total_bytes,
			limit: config.max_total_bytes,
		});
	}

	Ok(())
}

// ── Core Layout Implementation ──

/// Primary entry point: computes topological Git graph layout for a slice of
/// commits.
///
/// Supports sequential paging via `checkpoint`. Returns an explicit
/// [`GraphError`] if input limits or geometry parameters are violated.
pub fn compute_graph_layout(
	commits: &[CommitSummary],
	refs: &[GitReference],
	head: Option<&str>,
	config: &GraphConfig,
	checkpoint: Option<&GraphCheckpoint>,
) -> Result<GraphLayout, GraphError> {
	validate_config(config)?;
	validate_input_bounds(commits, refs, config, checkpoint)?;

	// Validate incoming checkpoint frontier size before allocating
	if let Some(cp) = checkpoint {
		if cp.frontier.len() > config.max_frontier_size {
			return Err(GraphError::FrontierLimitExceeded {
				current: cp.frontier.len(),
				limit: config.max_frontier_size,
			});
		}
		// Continuity sanity check: first commit cannot repeat the exact last commit
		// (even on single-row pages)
		if let Some(ref last_sha) = cp.last_seen_sha {
			if let Some(first_commit) = commits.first() {
				if &first_commit.sha == last_sha {
					return Err(GraphError::ContinuityError {
						message: format!(
							"first commit {last_sha} duplicates last commit of previous page"
						),
					});
				}
			}
		}
	}

	// Page-local duplicate detection (bounded O(page))
	let mut page_seen = HashSet::with_capacity(commits.len());
	for (idx, c) in commits.iter().enumerate() {
		if !page_seen.insert(c.sha.clone()) {
			return Err(GraphError::DuplicateCommit {
				sha: c.sha.clone(),
				row: idx,
			});
		}
		if c.parents.len() > config.max_frontier_size {
			return Err(GraphError::FrontierLimitExceeded {
				current: c.parents.len(),
				limit: config.max_frontier_size,
			});
		}
	}

	let global_row_offset = checkpoint.map_or(0, |cp| cp.next_global_row);
	let mut next_rail_id = checkpoint.map_or(0, |cp| cp.next_rail_id);

	// Parse refs and index references by commit SHA borrowing strings without re-cloning
	let parsed_refs: Vec<(&str, RefInfo)> = refs
		.iter()
		.map(|r| (r.sha.as_str(), parse_ref_name(&r.name)))
		.collect();

	let mut refs_by_commit: HashMap<&str, Vec<&RefInfo>> =
		HashMap::with_capacity(parsed_refs.len());
	for (sha, ref_info) in &parsed_refs {
		refs_by_commit.entry(sha).or_default().push(ref_info);
	}

	let mut hash_index: HashMap<&str, usize> =
		HashMap::with_capacity(commits.len());
	for (i, c) in commits.iter().enumerate() {
		hash_index.insert(c.sha.as_str(), i);
	}

	// Check if this page or incoming frontier contains/knows HEAD
	let head_in_refs = commits.iter().any(|c| {
		refs_by_commit
			.get(c.sha.as_str())
			.is_some_and(|rs| rs.iter().any(|r| r.kind == RefKind::Head))
	});
	let has_head_reference = head.is_some() || head_in_refs;

	let unit_w = config.unit_x;
	let half_w = unit_w / 2.0;
	let unit_h = config.unit_y;
	let half_h = unit_h / 2.0;

	let mut unsolved: Vec<PathHelper> = checkpoint
		.map(|cp| cp.frontier.iter().map(PathHelper::from_frontier).collect())
		.unwrap_or_default();

	let mut completed_paths: Vec<GraphPath> = Vec::new();
	let mut links: Vec<GraphLink> = Vec::new();
	let mut rows: Vec<GraphRow> = Vec::with_capacity(commits.len());
	let mut commit_left_margin: Vec<f64> = Vec::with_capacity(commits.len());
	let mut ended: Vec<PathHelper> = Vec::new();

	for (row_idx, commit) in commits.iter().enumerate() {
		let global_row = global_row_offset + row_idx;
		let offset_y = (global_row as f64) * unit_h + half_h;
		let mut offset_x = config.offset_x - unit_w;

		let max_offset_old = if let Some(last_rail) = unsolved.last() {
			last_rail.last_x
		} else {
			config.offset_x
		};

		let mut major_idx: Option<usize> = None;
		let mut major_x: Option<f64> = None;
		let mut major_rail_id: Option<usize> = None;
		let mut major_color: Option<usize> = None;
		let mut passing_lanes_for_row = Vec::new();
		let mut incoming_highlighted = false;

		let is_head = head.map_or_else(
			|| {
				refs_by_commit.get(commit.sha.as_str()).is_some_and(|rs| {
					rs.iter().any(|r| r.kind == RefKind::Head)
				})
			},
			|h| commit.sha == h,
		);

		for (i, rail) in unsolved.iter_mut().enumerate() {
			let is_target = rail.next == commit.sha;
			if is_target {
				if major_idx.is_none() {
					offset_x += unit_w;
					major_idx = Some(i);
					major_x = Some(offset_x);
					major_rail_id = Some(rail.rail_id);
					major_color = Some(rail.color);
					if rail.highlighted || is_head {
						rail.highlighted = true;
						incoming_highlighted = true;
					}
					if !commit.parents.is_empty() {
						rail.next = commit.parents[0].clone();
						rail.goto(offset_x, offset_y, half_h);
					} else {
						rail.end(offset_x, offset_y, half_h);
					}
				} else {
					let major_last_x = major_x.unwrap_or(offset_x);
					rail.end(major_last_x, offset_y, half_h);
				}
			} else {
				offset_x += unit_w;
				rail.pass(offset_x, offset_y, half_h);
				let lane = ((offset_x - config.offset_x) / unit_w)
					.round()
					.max(0.0) as usize;
				passing_lanes_for_row.push(PassingLane {
					lane,
					color_index: rail.color,
					color_override: rail.color_override.clone(),
					rail_id: rail.rail_id,
				});
			}
		}

		// Separate ended paths
		if major_idx.is_some() {
			let mut remaining = Vec::with_capacity(unsolved.len());
			for (i, rail) in unsolved.into_iter().enumerate() {
				let was_major = major_idx == Some(i);
				let matched_commit = rail.next == commit.sha;
				if was_major {
					if commit.parents.is_empty() {
						ended.push(rail);
					} else {
						remaining.push(rail);
					}
				} else if matched_commit {
					ended.push(rail);
				} else {
					remaining.push(rail);
				}
			}
			unsolved = remaining;
		}

		let is_node_highlighted = if has_head_reference {
			is_head
				|| incoming_highlighted
				|| (commit.sha == "UNCOMMITTED" && is_head)
		} else {
			// Without any loaded HEAD, everything remains highlighted (ClipCodeVSCode parity)
			true
		};

		// New branch head if commit had no incoming rail
		if major_idx.is_none() {
			offset_x += unit_w;
			major_x = Some(offset_x);
			if !commit.parents.is_empty() {
				if unsolved.len() >= config.max_frontier_size {
					return Err(GraphError::FrontierLimitExceeded {
						current: unsolved.len() + 1,
						limit: config.max_frontier_size,
					});
				}
				let preferred =
					preferred_index_for_commit(&commit.sha, &refs_by_commit);
				let mut mask = 0u32;
				for r in &unsolved {
					if r.color < 32 {
						mask |= 1 << r.color;
					}
				}
				let color = pick_color(mask, preferred);
				let mut rail = PathHelper::new_start(
					commit.parents[0].clone(),
					color,
					Point {
						x: offset_x,
						y: offset_y,
					},
					next_rail_id,
				);
				next_rail_id += 1;
				rail.highlighted = is_node_highlighted;
				let rid = rail.rail_id;
				major_rail_id = Some(rid);
				major_color = Some(color);
				unsolved.push(rail);
			}
		}

		// Compute node dot position and lane
		let node_x = major_x.unwrap_or(offset_x);
		let node_lane =
			((node_x - config.offset_x) / unit_w).round().max(0.0) as usize;
		let node_pos = Point {
			x: node_x,
			y: offset_y,
		};

		let dot_color = major_color.unwrap_or_else(|| {
			preferred_index_for_commit(&commit.sha, &refs_by_commit)
				.unwrap_or(0)
		});

		let is_shallow = config.shallow_roots.contains(&commit.sha);
		let node_type = if commit.sha == "UNCOMMITTED" {
			NodeType::Uncommitted
		} else if is_shallow {
			NodeType::ShallowRoot
		} else if commit.parents.len() > 1 {
			NodeType::Merge
		} else if is_head {
			NodeType::Head
		} else {
			NodeType::Normal
		};

		// Track merge parent rails explicitly: (rail_id, target_lane, color)
		let mut merge_parent_info: Vec<(usize, usize, usize)> = Vec::new();
		if commit.parents.len() > 1 {
			for parent_sha in commit.parents.iter().skip(1) {
				if let Some(target_rail) =
					unsolved.iter().find(|r| &r.next == parent_sha)
				{
					// Connect to existing path via merge link
					links.push(GraphLink {
						start: node_pos,
						end: Point {
							x: target_rail.last_x,
							y: offset_y + half_h,
						},
						control: Point {
							x: target_rail.last_x,
							y: node_pos.y,
						},
						color: target_rail.color,
						color_override: target_rail.color_override.clone(),
						highlighted: is_node_highlighted,
						target_rail_id: target_rail.rail_id,
					});
					let target_lane = ((target_rail.last_x - config.offset_x)
						/ unit_w)
						.round()
						.max(0.0) as usize;
					merge_parent_info.push((
						target_rail.rail_id,
						target_lane,
						target_rail.color,
					));
				} else {
					// New path for merge parent
					offset_x += unit_w;
					if unsolved.len() >= config.max_frontier_size {
						return Err(GraphError::FrontierLimitExceeded {
							current: unsolved.len() + 1,
							limit: config.max_frontier_size,
						});
					}
					let preferred = preferred_index_for_commit(
						parent_sha.as_str(),
						&refs_by_commit,
					);
					let mut mask = 0u32;
					for r in &unsolved {
						if r.color < 32 {
							mask |= 1 << r.color;
						}
					}
					let color = pick_color(mask, preferred);
					let mut new_rail = PathHelper::new_merge(
						parent_sha.clone(),
						color,
						node_pos,
						Point {
							x: offset_x,
							y: node_pos.y + half_h,
						},
						next_rail_id,
					);
					let rid = new_rail.rail_id;
					next_rail_id += 1;
					new_rail.highlighted = is_node_highlighted;
					unsolved.push(new_rail);
					let new_lane = ((offset_x - config.offset_x) / unit_w)
						.round()
						.max(0.0) as usize;
					merge_parent_info.push((rid, new_lane, color));
				}
			}
		}

		let left_margin = f64::max(offset_x, max_offset_old) + half_w + 2.0;
		commit_left_margin.push(left_margin);
		let max_lane =
			((left_margin - config.offset_x) / unit_w).ceil().max(0.0) as usize;

		let node = GraphNode {
			lane: node_lane,
			color_index: dot_color,
			color_override: None,
			node_type,
			is_head,
			highlighted: is_node_highlighted,
			rail_id: major_rail_id,
		};

		// Construct parent edges with consistent rail IDs and boundaries
		let mut parent_edges = Vec::new();
		if is_shallow && commit.parents.is_empty() {
			// Explicit shallow boundary representation even when Git rev-list reports 0 parents
			parent_edges.push(ParentEdge {
				parent_sha: String::new(),
				parent_index: 0,
				from_lane: node_lane,
				to_lane: node_lane,
				to_row: None,
				color_index: dot_color,
				color_override: None,
				continuation: ContinuationKind::ShallowBoundary,
				rail_id: major_rail_id.unwrap_or(0),
			});
		} else {
			for (p_idx, p_sha) in commit.parents.iter().enumerate() {
				let (edge_rail_id, edge_to_lane, p_color) = if p_idx == 0 {
					(major_rail_id.unwrap_or(0), node_lane, dot_color)
				} else {
					merge_parent_info.get(p_idx - 1).copied().unwrap_or((
						major_rail_id.unwrap_or(0),
						node_lane,
						dot_color,
					))
				};

				let (continuation, to_row) =
					if let Some(&p_row) = hash_index.get(p_sha.as_str()) {
						(ContinuationKind::Resolved, Some(p_row))
					} else if config.filtered_commits.contains(p_sha) {
						(ContinuationKind::FilteredGap, None)
					} else {
						// Missing from this page, unresolved page boundary
						(ContinuationKind::UnresolvedPageBoundary, None)
					};

				parent_edges.push(ParentEdge {
					parent_sha: p_sha.clone(),
					parent_index: p_idx,
					from_lane: node_lane,
					to_lane: edge_to_lane,
					to_row,
					color_index: p_color,
					color_override: None,
					continuation,
					rail_id: edge_rail_id,
				});
			}
		}

		let commit_refs: Vec<RefInfo> = refs_by_commit
			.get(commit.sha.as_str())
			.map(|rs| rs.iter().map(|&r| r.clone()).collect())
			.unwrap_or_default();

		rows.push(GraphRow {
			sha: commit.sha.clone(),
			row: row_idx,
			global_row,
			node,
			passing_lanes: passing_lanes_for_row,
			parent_edges,
			refs: commit_refs,
			max_lane,
		});
	}

	// Move ended paths to completed
	for p in ended {
		completed_paths.push(p.to_graph_path());
	}

	// Resolve target lanes for edges resolved within this page
	let row_lanes: Vec<usize> = rows.iter().map(|r| r.node.lane).collect();
	for row in &mut rows {
		for edge in &mut row.parent_edges {
			if let Some(target_row) = edge.to_row {
				if let Some(&lane) = row_lanes.get(target_row) {
					edge.to_lane = lane;
				}
			}
		}
	}

	// Checkpoint creation or termination at page bottom
	let end_y = ((global_row_offset + commits.len()) as f64 - 0.5) * unit_h;
	let checkpoint_out = if !unsolved.is_empty() {
		let frontier = unsolved
			.iter()
			.map(|r| {
				let lane = ((r.last_x - config.offset_x) / unit_w)
					.round()
					.max(0.0) as usize;
				FrontierRail {
					next_sha: r.next.clone(),
					lane,
					last_x: r.last_x,
					last_y: r.last_y,
					color_index: r.color,
					color_override: r.color_override.clone(),
					highlighted: r.highlighted,
					rail_id: r.rail_id,
				}
			})
			.collect();

		let mut mask = 0u32;
		for r in &unsolved {
			if r.color < 32 {
				mask |= 1 << r.color;
			}
		}

		Some(GraphCheckpoint {
			frontier,
			next_global_row: global_row_offset + commits.len(),
			next_rail_id,
			used_colors_mask: mask,
			last_seen_sha: commits.last().map(|c| c.sha.clone()),
		})
	} else {
		None
	};

	// Finalize remaining paths
	for mut rail in unsolved {
		if rail.points.len() > 1
			|| (rail.points.len() == 1
				&& (rail.points[0].y - end_y).abs() > 1e-4)
		{
			rail.end(rail.last_x, end_y + half_h, half_h);
		}
		completed_paths.push(rail.to_graph_path());
	}

	Ok(GraphLayout {
		rows,
		paths: completed_paths,
		links,
		commit_left_margin,
		checkpoint: checkpoint_out,
		is_fallback: false,
	})
}

/// Convenience layout entry point using standard configuration and no initial
/// checkpoint.
pub fn build_graph_layout(
	commits: &[CommitSummary],
	refs: &[GitReference],
	head: Option<&str>,
) -> Result<GraphLayout, GraphError> {
	compute_graph_layout(commits, refs, head, &GraphConfig::default(), None)
}

/// Fallback plain linear row list layout when graph frontier limits are exceeded.
///
/// Returns a bounded list of rows with lane 0 positions and no vector paths or
/// links (`paths` and `links` are empty, and `node.rail_id` is `None`).
/// Preserves factual commit references, parent edges, and node style metadata
/// without inventing false continuous rails through unrelated branches or root
/// commits.
///
/// # Limits and Caller Responsibility
/// - Callers should present a visible warning to the user indicating a plain list
///   fallback when a frontier limit (`GraphError::FrontierLimitExceeded`) occurs.
/// - General input overflow (exceeding `max_rows`, `max_refs`, or `max_total_bytes`)
///   is an explicit error and MUST NOT be retried through fallback layout.
///   This function enforces the exact same row, byte, ref, and SHA bounds as
///   [`compute_graph_layout`], returning an error if bounds are exceeded rather
///   than silently truncating input.
pub fn fallback_linear_layout(
	commits: &[CommitSummary],
	refs: &[GitReference],
	head: Option<&str>,
	config: &GraphConfig,
) -> Result<GraphLayout, GraphError> {
	validate_config(config)?;
	validate_input_bounds(commits, refs, config, None)?;

	let parsed_refs: Vec<(&str, RefInfo)> = refs
		.iter()
		.map(|r| (r.sha.as_str(), parse_ref_name(&r.name)))
		.collect();

	let mut refs_by_commit: HashMap<&str, Vec<&RefInfo>> =
		HashMap::with_capacity(parsed_refs.len());
	for (sha, ref_info) in &parsed_refs {
		refs_by_commit.entry(sha).or_default().push(ref_info);
	}

	let hash_index: HashMap<&str, usize> = commits
		.iter()
		.enumerate()
		.map(|(i, c)| (c.sha.as_str(), i))
		.collect();

	let mut rows = Vec::with_capacity(commits.len());
	let mut commit_left_margin = Vec::with_capacity(commits.len());

	for (i, commit) in commits.iter().enumerate() {
		let is_head = head.map_or_else(
			|| {
				refs_by_commit.get(commit.sha.as_str()).is_some_and(|rs| {
					rs.iter().any(|r| r.kind == RefKind::Head)
				})
			},
			|h| commit.sha == h,
		);

		let is_shallow = config.shallow_roots.contains(&commit.sha);
		let node_type = if commit.sha == "UNCOMMITTED" {
			NodeType::Uncommitted
		} else if is_shallow {
			NodeType::ShallowRoot
		} else if commit.parents.len() > 1 {
			NodeType::Merge
		} else if is_head {
			NodeType::Head
		} else {
			NodeType::Normal
		};

		let node = GraphNode {
			lane: 0,
			color_index: 0,
			color_override: None,
			node_type,
			is_head,
			highlighted: true,
			rail_id: None,
		};

		let parent_edges = if is_shallow && commit.parents.is_empty() {
			vec![ParentEdge {
				parent_sha: String::new(),
				parent_index: 0,
				from_lane: 0,
				to_lane: 0,
				to_row: None,
				color_index: 0,
				color_override: None,
				continuation: ContinuationKind::ShallowBoundary,
				rail_id: 0,
			}]
		} else {
			commit
				.parents
				.iter()
				.enumerate()
				.map(|(p_idx, p_sha)| {
					let (continuation, to_row) =
						if let Some(&target) = hash_index.get(p_sha.as_str()) {
							(ContinuationKind::Resolved, Some(target))
						} else if config.filtered_commits.contains(p_sha) {
							(ContinuationKind::FilteredGap, None)
						} else {
							(ContinuationKind::UnresolvedPageBoundary, None)
						};
					ParentEdge {
						parent_sha: p_sha.clone(),
						parent_index: p_idx,
						from_lane: 0,
						to_lane: 0,
						to_row,
						color_index: 0,
						color_override: None,
						continuation,
						rail_id: 0,
					}
				})
				.collect()
		};

		let commit_refs: Vec<RefInfo> = refs_by_commit
			.get(commit.sha.as_str())
			.map(|rs| rs.iter().map(|&r| r.clone()).collect())
			.unwrap_or_default();

		commit_left_margin.push(config.offset_x + config.unit_x + 2.0);

		rows.push(GraphRow {
			sha: commit.sha.clone(),
			row: i,
			global_row: i,
			node,
			passing_lanes: Vec::new(),
			parent_edges,
			refs: commit_refs,
			max_lane: 0,
		});
	}

	Ok(GraphLayout {
		rows,
		paths: Vec::new(),
		links: Vec::new(),
		commit_left_margin,
		checkpoint: None,
		is_fallback: true,
	})
}

// ── Invariant Verifier ──

/// Error indicating a broken topological or structural layout invariant.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum InvariantError {
	#[error("row count mismatch: layout has {layout_rows} rows, commits have {commits_rows}")]
	RowCountMismatch {
		layout_rows: usize,
		commits_rows: usize,
	},

	#[error(
		"commit sha mismatch at row {row}: expected {expected}, got {got}"
	)]
	ShaMismatch {
		row: usize,
		expected: String,
		got: String,
	},

	#[error("duplicate commit node in layout: sha {sha}")]
	DuplicateCommitNode { sha: String },

	#[error("edge connects to incorrect hash at row {row}: expected {expected}, got {got}")]
	EdgeHashMismatch {
		row: usize,
		expected: String,
		got: String,
	},

	#[error("invalid edge row index: edge targets row {target_row}, total rows {total}")]
	InvalidEdgeRow { target_row: usize, total: usize },

	#[error("coordinate out of bounds or non-finite: point ({x}, {y})")]
	InvalidCoordinate { x: f64, y: f64 },

	#[error("dangling rail ID: node at row {row} references rail {rail_id} which does not exist")]
	DanglingRailId { row: usize, rail_id: usize },
}

/// Verifies that a generated [`GraphLayout`] satisfies all fundamental graph
/// invariants:
/// 1. Row count and commit SHAs correspond 1:1 in order.
/// 2. No duplicated commit nodes exist.
/// 3. All resolved parent edges point to the exact expected parent commit SHA
///    and row.
/// 4. All geometric coordinates are non-negative and finite.
/// 5. Node rail IDs resolve to a defined [`GraphPath`].
pub fn verify_graph_invariants(
	layout: &GraphLayout,
	commits: &[CommitSummary],
) -> Result<(), InvariantError> {
	if layout.rows.len() != commits.len() {
		return Err(InvariantError::RowCountMismatch {
			layout_rows: layout.rows.len(),
			commits_rows: commits.len(),
		});
	}

	let mut seen_shas = HashSet::with_capacity(commits.len());
	for (i, row) in layout.rows.iter().enumerate() {
		if row.sha != commits[i].sha {
			return Err(InvariantError::ShaMismatch {
				row: i,
				expected: commits[i].sha.clone(),
				got: row.sha.clone(),
			});
		}
		if !seen_shas.insert(row.sha.clone()) {
			return Err(InvariantError::DuplicateCommitNode {
				sha: row.sha.clone(),
			});
		}
	}

	let known_rail_ids: HashSet<usize> =
		layout.paths.iter().map(|p| p.rail_id).collect();

	for (i, row) in layout.rows.iter().enumerate() {
		if let Some(rid) = row.node.rail_id {
			if !known_rail_ids.contains(&rid) {
				return Err(InvariantError::DanglingRailId {
					row: i,
					rail_id: rid,
				});
			}
		}

		for edge in &row.parent_edges {
			if edge.continuation == ContinuationKind::Resolved {
				let target_row_idx =
					edge.to_row.ok_or(InvariantError::InvalidEdgeRow {
						target_row: usize::MAX,
						total: layout.rows.len(),
					})?;
				if target_row_idx >= layout.rows.len() {
					return Err(InvariantError::InvalidEdgeRow {
						target_row: target_row_idx,
						total: layout.rows.len(),
					});
				}
				let target_sha = &layout.rows[target_row_idx].sha;
				if target_sha != &edge.parent_sha {
					return Err(InvariantError::EdgeHashMismatch {
						row: i,
						expected: edge.parent_sha.clone(),
						got: target_sha.clone(),
					});
				}
			}
		}
	}

	for path in &layout.paths {
		for pt in &path.points {
			if !pt.x.is_finite()
				|| !pt.y.is_finite()
				|| pt.x < 0.0
				|| pt.y < 0.0
			{
				return Err(InvariantError::InvalidCoordinate {
					x: pt.x,
					y: pt.y,
				});
			}
		}
	}

	for link in &layout.links {
		for pt in [&link.start, &link.control, &link.end] {
			if !pt.x.is_finite()
				|| !pt.y.is_finite()
				|| pt.x < 0.0
				|| pt.y < 0.0
			{
				return Err(InvariantError::InvalidCoordinate {
					x: pt.x,
					y: pt.y,
				});
			}
		}
	}

	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	fn make_commit(sha: &str, parents: &[&str]) -> CommitSummary {
		CommitSummary {
			sha: sha.to_string(),
			parents: parents.iter().map(|s| s.to_string()).collect(),
			author_name: "Tester".to_string(),
			author_email: "test@example.com".to_string(),
			author_date: "2026-09-25T00:00:00Z".to_string(),
			subject: format!("Commit {sha}"),
		}
	}

	#[test]
	fn test_empty_commits() {
		let layout = build_graph_layout(&[], &[], None).unwrap();
		assert!(layout.rows.is_empty());
		assert!(layout.paths.is_empty());
		assert!(layout.links.is_empty());
		assert!(verify_graph_invariants(&layout, &[]).is_ok());
	}

	#[test]
	fn test_linear_history_same_lane() {
		let commits = vec![
			make_commit("c3", &["c2"]),
			make_commit("c2", &["c1"]),
			make_commit("c1", &[]),
		];
		let layout = build_graph_layout(&commits, &[], None).unwrap();
		assert_eq!(layout.rows.len(), 3);
		// All nodes reside on lane 0
		assert_eq!(layout.rows[0].node.lane, 0);
		assert_eq!(layout.rows[1].node.lane, 0);
		assert_eq!(layout.rows[2].node.lane, 0);
		assert!(layout.links.is_empty());
		assert!(verify_graph_invariants(&layout, &commits).is_ok());
	}

	#[test]
	fn test_branch_and_merge() {
		// c4 (merge) -> c3, c2
		// c3 -> c1
		// c2 -> c1
		// c1 -> root
		let commits = vec![
			make_commit("c4", &["c3", "c2"]),
			make_commit("c3", &["c1"]),
			make_commit("c2", &["c1"]),
			make_commit("c1", &[]),
		];
		let layout = build_graph_layout(&commits, &[], None).unwrap();
		assert_eq!(layout.rows.len(), 4);
		assert_eq!(layout.rows[0].node.node_type, NodeType::Merge);
		assert_eq!(layout.rows[0].parent_edges.len(), 2);
		assert_eq!(layout.rows[0].parent_edges[0].to_lane, 0);
		assert_eq!(layout.rows[0].parent_edges[1].to_lane, 1);
		assert_ne!(
			layout.rows[0].parent_edges[0].rail_id,
			layout.rows[0].parent_edges[1].rail_id
		);
		assert!(verify_graph_invariants(&layout, &commits).is_ok());
	}

	#[test]
	fn test_octopus_merge() {
		let commits = vec![
			make_commit("merge", &["p1", "p2", "p3"]),
			make_commit("p1", &["root"]),
			make_commit("p2", &["root"]),
			make_commit("p3", &["root"]),
			make_commit("root", &[]),
		];
		let layout = build_graph_layout(&commits, &[], None).unwrap();
		assert_eq!(layout.rows.len(), 5);
		assert_eq!(layout.rows[0].node.node_type, NodeType::Merge);
		assert_eq!(layout.rows[0].parent_edges.len(), 3);
		assert!(verify_graph_invariants(&layout, &commits).is_ok());
	}

	#[test]
	fn test_root_commit() {
		let commits = vec![make_commit("root", &[])];
		let layout = build_graph_layout(&commits, &[], None).unwrap();
		assert_eq!(layout.rows.len(), 1);
		assert_eq!(layout.rows[0].node.lane, 0);
		assert!(layout.rows[0].parent_edges.is_empty());
		assert!(verify_graph_invariants(&layout, &commits).is_ok());
	}

	#[test]
	fn test_unresolved_parent_at_page_boundary() {
		let commits = vec![make_commit("c2", &["c1"])];
		let layout = build_graph_layout(&commits, &[], None).unwrap();
		assert_eq!(layout.rows.len(), 1);
		assert_eq!(layout.rows[0].parent_edges.len(), 1);
		assert_eq!(
			layout.rows[0].parent_edges[0].continuation,
			ContinuationKind::UnresolvedPageBoundary
		);
		assert_eq!(layout.rows[0].parent_edges[0].to_row, None);
		assert!(layout.checkpoint.is_some());
		assert_eq!(layout.checkpoint.as_ref().unwrap().frontier.len(), 1);
		assert_eq!(
			layout.checkpoint.as_ref().unwrap().frontier[0].next_sha,
			"c1"
		);
	}

	#[test]
	fn test_shallow_and_filtered_boundary_indications() {
		let commits = vec![
			make_commit("shallow_commit", &[]),
			make_commit("child", &["filtered_parent"]),
		];
		let mut shallow_roots = HashSet::new();
		shallow_roots.insert("shallow_commit".to_string());

		let mut filtered_commits = HashSet::new();
		filtered_commits.insert("filtered_parent".to_string());

		let config = GraphConfig {
			shallow_roots,
			filtered_commits,
			..Default::default()
		};

		let layout =
			compute_graph_layout(&commits, &[], None, &config, None).unwrap();
		assert_eq!(layout.rows[0].node.node_type, NodeType::ShallowRoot);
		assert_eq!(
			layout.rows[0].parent_edges[0].continuation,
			ContinuationKind::ShallowBoundary
		);
		assert_eq!(
			layout.rows[1].parent_edges[0].continuation,
			ContinuationKind::FilteredGap
		);
	}

	#[test]
	fn test_page_crossing_parent_not_falsely_filtered() {
		// Parent c1 is not in page 1, but is NOT in filtered_commits.
		// It MUST be UnresolvedPageBoundary, NOT FilteredGap.
		let commits = vec![make_commit("c2", &["c1"])];
		let config = GraphConfig {
			filtered_commits: HashSet::new(),
			..Default::default()
		};
		let layout =
			compute_graph_layout(&commits, &[], None, &config, None).unwrap();
		assert_eq!(
			layout.rows[0].parent_edges[0].continuation,
			ContinuationKind::UnresolvedPageBoundary
		);
	}

	#[test]
	fn test_frontier_limit_exceeded_error() {
		let commits = vec![
			make_commit("merge", &["p1", "p2", "p3", "p4", "p5"]),
			make_commit("p1", &[]),
			make_commit("p2", &[]),
			make_commit("p3", &[]),
			make_commit("p4", &[]),
			make_commit("p5", &[]),
		];
		let config = GraphConfig {
			max_frontier_size: 3,
			..Default::default()
		};
		let err = compute_graph_layout(&commits, &[], None, &config, None)
			.unwrap_err();
		assert!(matches!(err, GraphError::FrontierLimitExceeded { .. }));
	}

	#[test]
	fn test_row_limit_exceeded_error() {
		let commits = vec![make_commit("c1", &[]), make_commit("c2", &[])];
		let config = GraphConfig {
			max_rows: 1,
			..Default::default()
		};
		let err = compute_graph_layout(&commits, &[], None, &config, None)
			.unwrap_err();
		assert!(matches!(
			err,
			GraphError::RowLimitExceeded {
				requested: 2,
				limit: 1
			}
		));
	}

	#[test]
	fn test_duplicate_commit_error_within_page() {
		let commits = vec![make_commit("c1", &[]), make_commit("c1", &[])];
		let err = build_graph_layout(&commits, &[], None).unwrap_err();
		assert!(matches!(err, GraphError::DuplicateCommit { .. }));
	}

	#[test]
	fn test_bounded_checkpoint_memory_over_many_pages() {
		// Walk 20,000 single-parent commits over 200 pages.
		// Assert that checkpoint serialized JSON length stays strictly bounded O(1),
		// independent of total pages walked (not growing with total history).
		let total_commits = 20_000;
		let page_size = 100;
		let num_pages = total_commits / page_size;

		let mut checkpoint: Option<GraphCheckpoint> = None;
		let mut initial_serialized_size = 0;

		for page in 0..num_pages {
			let start_idx = (num_pages - page) * page_size - 1;
			let end_idx = (num_pages - page - 1) * page_size;

			let mut page_commits = Vec::with_capacity(page_size);
			for idx in (end_idx..=start_idx).rev() {
				let sha = format!("commit_{idx}");
				let parent_sha = if idx > 0 {
					format!("commit_{}", idx - 1)
				} else {
					"root".to_string()
				};
				page_commits.push(make_commit(&sha, &[&parent_sha]));
			}

			let layout = compute_graph_layout(
				&page_commits,
				&[],
				None,
				&GraphConfig::default(),
				checkpoint.as_ref(),
			)
			.unwrap();

			checkpoint = layout.checkpoint;
			let cp_json =
				serde_json::to_string(checkpoint.as_ref().unwrap()).unwrap();

			if initial_serialized_size == 0 {
				initial_serialized_size = cp_json.len();
			} else {
				// Allow small variance only for digit count in next_global_row
				let diff = (cp_json.len() as isize
					- initial_serialized_size as isize)
					.abs();
				assert!(
					diff < 30,
					"checkpoint size grew unexpectedly: {cp_json}"
				);
			}
		}

		assert!(
			initial_serialized_size > 0 && initial_serialized_size < 300,
			"checkpoint should be compact O(frontier), was {initial_serialized_size} bytes"
		);
	}

	#[test]
	fn test_paged_vs_one_shot_consistency_across_multiple_split_points() {
		// Diamond history with branches and a merge:
		//   m3 (merge) -> [m2, f2]
		//   m2 -> [m1]
		//   f2 -> [f1]
		//   m1 -> [root]
		//   f1 -> [root]
		//   root -> []
		let full_history = vec![
			make_commit("m3", &["m2", "f2"]),
			make_commit("m2", &["m1"]),
			make_commit("f2", &["f1"]),
			make_commit("m1", &["root"]),
			make_commit("f1", &["root"]),
			make_commit("root", &[]),
		];
		let refs = vec![GitReference {
			name: "refs/heads/main".into(),
			sha: "m3".into(),
		}];

		let one_shot = compute_graph_layout(
			&full_history,
			&refs,
			Some("m3"),
			&GraphConfig::default(),
			None,
		)
		.unwrap();
		assert!(verify_graph_invariants(&one_shot, &full_history).is_ok());

		// Test multiple split positions: split at 1, 2, 3, 4, 5
		for split_at in 1..full_history.len() {
			let (p1_commits, p2_commits) = full_history.split_at(split_at);

			let p1 = compute_graph_layout(
				p1_commits,
				&refs,
				Some("m3"),
				&GraphConfig::default(),
				None,
			)
			.unwrap();
			let cp1 = p1.checkpoint.as_ref().unwrap();

			// Serialize and deserialize roundtrip
			let cp1_json = serde_json::to_string(cp1).unwrap();
			let cp1_deser: GraphCheckpoint =
				serde_json::from_str(&cp1_json).unwrap();

			let p2 = compute_graph_layout(
				p2_commits,
				&refs,
				Some("m3"),
				&GraphConfig::default(),
				Some(&cp1_deser),
			)
			.unwrap();

			// Verify topological lane, color, node_type, and highlighted matching across all rows
			for (i, row) in p1.rows.iter().enumerate() {
				assert_eq!(
					row.node.lane, one_shot.rows[i].node.lane,
					"lane mismatch at row {i} in split {split_at}"
				);
				assert_eq!(
					row.node.color_index, one_shot.rows[i].node.color_index,
					"color mismatch at row {i} in split {split_at}"
				);
				assert_eq!(
					row.node.node_type, one_shot.rows[i].node.node_type,
					"node_type mismatch at row {i} in split {split_at}"
				);
				assert_eq!(
					row.node.highlighted, one_shot.rows[i].node.highlighted,
					"highlighted mismatch at row {i} in split {split_at}"
				);
				assert_eq!(
					row.passing_lanes.len(),
					one_shot.rows[i].passing_lanes.len(),
					"passing_lanes count mismatch at row {i} in split {split_at}"
				);
				for (k, pl) in row.passing_lanes.iter().enumerate() {
					assert_eq!(pl.lane, one_shot.rows[i].passing_lanes[k].lane);
					assert_eq!(
						pl.color_index,
						one_shot.rows[i].passing_lanes[k].color_index
					);
				}
				assert_eq!(
					row.parent_edges.len(),
					one_shot.rows[i].parent_edges.len(),
					"parent_edges count mismatch at row {i} in split {split_at}"
				);
				for (k, edge) in row.parent_edges.iter().enumerate() {
					assert_eq!(
						edge.parent_sha,
						one_shot.rows[i].parent_edges[k].parent_sha
					);
					assert_eq!(
						edge.from_lane,
						one_shot.rows[i].parent_edges[k].from_lane
					);
					assert_eq!(
						edge.color_index,
						one_shot.rows[i].parent_edges[k].color_index
					);
					if edge.continuation == ContinuationKind::Resolved {
						assert_eq!(
							edge.to_lane,
							one_shot.rows[i].parent_edges[k].to_lane
						);
					}
				}
			}
			for (i, row) in p2.rows.iter().enumerate() {
				let one_shot_idx = split_at + i;
				assert_eq!(
					row.node.lane, one_shot.rows[one_shot_idx].node.lane,
					"lane mismatch at row {one_shot_idx} in split {split_at}"
				);
				assert_eq!(
					row.node.color_index,
					one_shot.rows[one_shot_idx].node.color_index,
					"color mismatch at row {one_shot_idx} in split {split_at}"
				);
				assert_eq!(
					row.node.node_type,
					one_shot.rows[one_shot_idx].node.node_type,
					"node_type mismatch at row {one_shot_idx} in split {split_at}"
				);
				assert_eq!(
					row.node.highlighted,
					one_shot.rows[one_shot_idx].node.highlighted,
					"highlighted mismatch at row {one_shot_idx} in split {split_at}"
				);
				assert_eq!(
					row.passing_lanes.len(),
					one_shot.rows[one_shot_idx].passing_lanes.len(),
					"passing_lanes count mismatch at row {one_shot_idx} in split {split_at}"
				);
				for (k, pl) in row.passing_lanes.iter().enumerate() {
					assert_eq!(
						pl.lane,
						one_shot.rows[one_shot_idx].passing_lanes[k].lane
					);
					assert_eq!(
						pl.color_index,
						one_shot.rows[one_shot_idx].passing_lanes[k]
							.color_index
					);
				}
				assert_eq!(
					row.parent_edges.len(),
					one_shot.rows[one_shot_idx].parent_edges.len(),
					"parent_edges count mismatch at row {one_shot_idx} in split {split_at}"
				);
				for (k, edge) in row.parent_edges.iter().enumerate() {
					assert_eq!(
						edge.parent_sha,
						one_shot.rows[one_shot_idx].parent_edges[k].parent_sha
					);
					assert_eq!(
						edge.from_lane,
						one_shot.rows[one_shot_idx].parent_edges[k].from_lane
					);
					assert_eq!(
						edge.to_lane,
						one_shot.rows[one_shot_idx].parent_edges[k].to_lane
					);
					assert_eq!(
						edge.color_index,
						one_shot.rows[one_shot_idx].parent_edges[k].color_index
					);
				}
			}

			// Verify rail ID continuity: if a parent edge in p1 points across the boundary,
			// its rail_id must match the frontier rail in the checkpoint
			for row in &p1.rows {
				for edge in &row.parent_edges {
					if edge.continuation
						== ContinuationKind::UnresolvedPageBoundary
					{
						let matching_frontier = cp1
							.frontier
							.iter()
							.find(|r| r.rail_id == edge.rail_id);
						assert!(
							matching_frontier.is_some(),
							"frontier must contain rail for edge rail_id {}",
							edge.rail_id
						);
						let mf = matching_frontier.unwrap();
						assert_eq!(
							mf.next_sha, edge.parent_sha,
							"frontier next_sha must match parent_sha"
						);
						assert_eq!(
							mf.lane, edge.to_lane,
							"frontier lane must match edge.to_lane"
						);
						assert_eq!(
							mf.color_index, edge.color_index,
							"frontier color must match edge.color_index"
						);
					}
				}
			}
		}
	}

	#[test]
	fn test_reject_exact_duplicate_first_commit_even_single_row_page() {
		let c1 = make_commit("c1", &[]);
		let cp = GraphCheckpoint {
			frontier: Vec::new(),
			next_global_row: 1,
			next_rail_id: 1,
			used_colors_mask: 0,
			last_seen_sha: Some("c1".to_string()),
		};
		// Single-row page with duplicated SHA from previous page must fail
		let err = compute_graph_layout(
			&[c1],
			&[],
			None,
			&GraphConfig::default(),
			Some(&cp),
		)
		.unwrap_err();
		assert!(matches!(err, GraphError::ContinuityError { .. }));
	}

	#[test]
	fn test_color_stability_across_page_split_without_parent_lookahead() {
		// Child commit c2 has NO ref; parent c1 has branch ref "refs/heads/feature"
		let c2 = make_commit("c2", &["c1"]);
		let c1 = make_commit("c1", &[]);
		let history = vec![c2.clone(), c1.clone()];
		let refs = vec![GitReference {
			name: "refs/heads/feature".into(),
			sha: "c1".into(),
		}];

		// One-shot layout
		let one_shot = compute_graph_layout(
			&history,
			&refs,
			None,
			&GraphConfig::default(),
			None,
		)
		.unwrap();

		// Paged layout: Page 1 has c2, Page 2 has c1
		let p1 = compute_graph_layout(
			&[c2],
			&refs,
			None,
			&GraphConfig::default(),
			None,
		)
		.unwrap();
		let cp1 = p1.checkpoint.as_ref().unwrap();

		let p2 = compute_graph_layout(
			&[c1],
			&refs,
			None,
			&GraphConfig::default(),
			Some(cp1),
		)
		.unwrap();

		// c2 in one-shot vs p1 must have exact same lane, color, and rail_id
		assert_eq!(p1.rows[0].node.lane, one_shot.rows[0].node.lane);
		assert_eq!(
			p1.rows[0].node.color_index,
			one_shot.rows[0].node.color_index
		);
		assert_eq!(p1.rows[0].node.rail_id, one_shot.rows[0].node.rail_id);

		// c1 in one-shot vs p2 must have exact same lane, color, and rail_id
		assert_eq!(p2.rows[0].node.lane, one_shot.rows[1].node.lane);
		assert_eq!(
			p2.rows[0].node.color_index,
			one_shot.rows[1].node.color_index
		);
		assert_eq!(p2.rows[0].node.rail_id, one_shot.rows[1].node.rail_id);

		// Parent edge from c2 to c1 must match in color and rail_id
		assert_eq!(
			p1.rows[0].parent_edges[0].color_index,
			one_shot.rows[0].parent_edges[0].color_index
		);
		assert_eq!(
			p1.rows[0].parent_edges[0].rail_id,
			one_shot.rows[0].parent_edges[0].rail_id
		);
	}

	#[test]
	fn test_merge_paging_topology_and_rail_continuity() {
		// Merge commit m with parents b1 and b2
		let commits = vec![
			make_commit("m", &["b1", "b2"]),
			make_commit("b1", &["base"]),
			make_commit("b2", &["base"]),
			make_commit("base", &[]),
		];
		let refs = vec![
			GitReference {
				name: "refs/heads/main".into(),
				sha: "m".into(),
			},
			GitReference {
				name: "refs/heads/feature".into(),
				sha: "b2".into(),
			},
		];

		let one_shot = compute_graph_layout(
			&commits,
			&refs,
			Some("m"),
			&GraphConfig::default(),
			None,
		)
		.unwrap();

		// Split between merge commit m (page 1) and parents b1, b2, base (page 2)
		let p1 = compute_graph_layout(
			&commits[..1],
			&refs,
			Some("m"),
			&GraphConfig::default(),
			None,
		)
		.unwrap();
		let cp1 = p1.checkpoint.as_ref().unwrap();

		// Page 1 frontier must have 2 rails waiting for b1 and b2
		assert_eq!(cp1.frontier.len(), 2);
		assert_eq!(cp1.frontier[0].next_sha, "b1");
		assert_eq!(cp1.frontier[1].next_sha, "b2");

		let p2 = compute_graph_layout(
			&commits[1..],
			&refs,
			Some("m"),
			&GraphConfig::default(),
			Some(cp1),
		)
		.unwrap();

		let combined_rows = [&p1.rows[..], &p2.rows[..]].concat();
		assert_eq!(combined_rows.len(), one_shot.rows.len());

		for (i, (paged_row, os_row)) in
			combined_rows.iter().zip(one_shot.rows.iter()).enumerate()
		{
			assert_eq!(paged_row.sha, os_row.sha, "SHA mismatch at row {i}");
			assert_eq!(
				paged_row.node.lane, os_row.node.lane,
				"lane mismatch at row {i}"
			);
			assert_eq!(
				paged_row.node.color_index, os_row.node.color_index,
				"color mismatch at row {i}"
			);
			assert_eq!(
				paged_row.node.rail_id, os_row.node.rail_id,
				"rail_id mismatch at row {i}"
			);
			assert_eq!(
				paged_row.node.highlighted, os_row.node.highlighted,
				"highlighted mismatch at row {i}"
			);
			assert_eq!(
				paged_row.passing_lanes.len(),
				os_row.passing_lanes.len(),
				"passing_lanes count mismatch at row {i}"
			);
			for (pl_p, pl_os) in paged_row
				.passing_lanes
				.iter()
				.zip(os_row.passing_lanes.iter())
			{
				assert_eq!(pl_p.lane, pl_os.lane);
				assert_eq!(pl_p.color_index, pl_os.color_index);
				assert_eq!(pl_p.rail_id, pl_os.rail_id);
			}
		}
	}

	#[test]
	fn test_sha_and_ref_bounds_rejected() {
		// Overly long SHA > MAX_SHA_LEN (128)
		let long_sha = "a".repeat(129);
		let c_bad_sha = make_commit(&long_sha, &[]);
		let err = build_graph_layout(&[c_bad_sha], &[], None).unwrap_err();
		assert!(matches!(err, GraphError::InvalidInput { .. }));

		// Overly long ref name > MAX_REF_NAME_LEN (1024)
		let c_valid = make_commit("c1", &[]);
		let bad_ref = GitReference {
			name: "r".repeat(1025),
			sha: "c1".into(),
		};
		let err = compute_graph_layout(
			&[c_valid.clone()],
			&[bad_ref],
			None,
			&GraphConfig::default(),
			None,
		)
		.unwrap_err();
		assert!(matches!(err, GraphError::InvalidInput { .. }));

		// Exceeding max_refs
		let config_refs = GraphConfig {
			max_refs: 2,
			..Default::default()
		};
		let refs = vec![
			GitReference {
				name: "r1".into(),
				sha: "c1".into(),
			},
			GitReference {
				name: "r2".into(),
				sha: "c1".into(),
			},
			GitReference {
				name: "r3".into(),
				sha: "c1".into(),
			},
		];
		let err = compute_graph_layout(
			&[c_valid.clone()],
			&refs,
			None,
			&config_refs,
			None,
		)
		.unwrap_err();
		assert!(matches!(
			err,
			GraphError::RefLimitExceeded {
				requested: 3,
				limit: 2
			}
		));

		// Exceeding max_total_bytes
		let config_bytes = GraphConfig {
			max_total_bytes: 20,
			..Default::default()
		};
		let err =
			compute_graph_layout(&[c_valid], &[], None, &config_bytes, None)
				.unwrap_err();
		assert!(matches!(err, GraphError::ByteLimitExceeded { .. }));
	}

	#[test]
	fn test_utf16_djb2_hashing_parity() {
		// Parity with TS name.charCodeAt(i)
		assert_eq!(hash_string_to_index("feature"), 10);
		assert_eq!(hash_string_to_index("main"), 1);
		assert_eq!(hash_string_to_index("develop"), 1);
		// Chinese ref names
		assert_eq!(hash_string_to_index("分支"), 5);
		assert_eq!(hash_string_to_index("feature/新功能"), 9);
		// Non-BMP surrogate pair ref names (emoji)
		assert_eq!(hash_string_to_index("tag/🚀"), 0);
	}

	#[test]
	fn test_invalid_config_parameters_rejected() {
		let config_zero_frontier = GraphConfig {
			max_frontier_size: 0,
			..Default::default()
		};
		assert!(matches!(
			compute_graph_layout(&[], &[], None, &config_zero_frontier, None),
			Err(GraphError::InvalidConfig { .. })
		));

		let config_zero_refs = GraphConfig {
			max_refs: 0,
			..Default::default()
		};
		assert!(matches!(
			compute_graph_layout(&[], &[], None, &config_zero_refs, None),
			Err(GraphError::InvalidConfig { .. })
		));

		let config_zero_bytes = GraphConfig {
			max_total_bytes: 0,
			..Default::default()
		};
		assert!(matches!(
			compute_graph_layout(&[], &[], None, &config_zero_bytes, None),
			Err(GraphError::InvalidConfig { .. })
		));

		let config_nan = GraphConfig {
			unit_x: f64::NAN,
			..Default::default()
		};
		assert!(matches!(
			compute_graph_layout(&[], &[], None, &config_nan, None),
			Err(GraphError::InvalidConfig { .. })
		));

		let config_neg = GraphConfig {
			offset_x: -1.0,
			..Default::default()
		};
		assert!(matches!(
			compute_graph_layout(&[], &[], None, &config_neg, None),
			Err(GraphError::InvalidConfig { .. })
		));
	}

	#[test]
	fn test_fallback_linear_layout() {
		let commits = vec![
			make_commit("c3", &["c2"]),
			make_commit("c2", &["c1"]),
			make_commit("c1", &[]),
		];
		let layout = fallback_linear_layout(
			&commits,
			&[],
			None,
			&GraphConfig::default(),
		)
		.unwrap();
		assert!(layout.is_fallback);
		assert_eq!(layout.rows.len(), 3);
		assert_eq!(layout.rows[0].node.lane, 0);
		assert_eq!(layout.rows[1].node.lane, 0);
		assert_eq!(layout.rows[2].node.lane, 0);
		assert!(layout.paths.is_empty());
		assert!(layout.links.is_empty());
		assert_eq!(layout.rows[0].node.rail_id, None);
		assert!(verify_graph_invariants(&layout, &commits).is_ok());
	}

	#[test]
	fn test_fallback_linear_layout_unrelated_roots_zero_paths() {
		let commits =
			vec![make_commit("root_a", &[]), make_commit("root_b", &[])];
		let layout = fallback_linear_layout(
			&commits,
			&[],
			None,
			&GraphConfig::default(),
		)
		.unwrap();

		assert!(layout.is_fallback);
		assert_eq!(layout.rows.len(), 2);
		// Zero connecting paths or links between unrelated roots
		assert!(
			layout.paths.is_empty(),
			"fallback must produce zero connecting paths"
		);
		assert!(layout.links.is_empty(), "fallback must produce zero links");
		assert_eq!(layout.rows[0].node.rail_id, None);
		assert_eq!(layout.rows[1].node.rail_id, None);
		assert!(verify_graph_invariants(&layout, &commits).is_ok());
	}

	#[test]
	fn test_fallback_linear_layout_oversized_input_returns_error() {
		let c1 = make_commit("c1", &[]);
		let c2 = make_commit("c2", &[]);

		// Exceeding max_rows
		let config_rows = GraphConfig {
			max_rows: 1,
			..Default::default()
		};
		let err =
			fallback_linear_layout(&[c1.clone(), c2], &[], None, &config_rows)
				.unwrap_err();
		assert!(matches!(err, GraphError::RowLimitExceeded { .. }));

		// Exceeding max_refs
		let config_refs = GraphConfig {
			max_refs: 1,
			..Default::default()
		};
		let refs = vec![
			GitReference {
				name: "r1".into(),
				sha: "c1".into(),
			},
			GitReference {
				name: "r2".into(),
				sha: "c1".into(),
			},
		];
		let err =
			fallback_linear_layout(&[c1.clone()], &refs, None, &config_refs)
				.unwrap_err();
		assert!(matches!(err, GraphError::RefLimitExceeded { .. }));

		// Exceeding max_total_bytes
		let config_bytes = GraphConfig {
			max_total_bytes: 10,
			..Default::default()
		};
		let err = fallback_linear_layout(&[c1], &[], None, &config_bytes)
			.unwrap_err();
		assert!(matches!(err, GraphError::ByteLimitExceeded { .. }));
	}

	#[test]
	fn test_child_and_actual_shallow_parent_split_across_pages() {
		// Child commit c2 has parent c1.
		// c1 is a shallow boundary root (in shallow_roots, 0 parents fetched in shallow clone).
		let c2 = make_commit("c2", &["c1"]);
		let c1 = make_commit("c1", &[]);
		let mut shallow_roots = HashSet::new();
		shallow_roots.insert("c1".to_string());

		let config = GraphConfig {
			shallow_roots,
			..Default::default()
		};

		// Page 1 contains only c2
		let p1 = compute_graph_layout(&[c2], &[], None, &config, None).unwrap();
		// In Page 1, c1 is absent from this page; it MUST NOT be marked as ShallowBoundary yet!
		// It must be an UnresolvedPageBoundary waiting across the page boundary.
		assert_eq!(
			p1.rows[0].parent_edges[0].continuation,
			ContinuationKind::UnresolvedPageBoundary
		);
		let cp1 = p1.checkpoint.as_ref().unwrap();
		assert_eq!(cp1.frontier.len(), 1);
		assert_eq!(cp1.frontier[0].next_sha, "c1");

		// Page 2 contains c1 (the actual shallow parent)
		let p2 =
			compute_graph_layout(&[c1], &[], None, &config, Some(cp1)).unwrap();
		// Now c1 is loaded: it MUST be identified as ShallowRoot!
		assert_eq!(p2.rows[0].node.node_type, NodeType::ShallowRoot);
		assert_eq!(
			p2.rows[0].parent_edges[0].continuation,
			ContinuationKind::ShallowBoundary
		);
		// Frontier must now be empty since c1 terminates the rail at the shallow boundary
		assert!(p2.checkpoint.is_none());
	}

	#[test]
	fn test_head_on_merge_flag() {
		let commits = vec![
			make_commit("merge", &["a", "b"]),
			make_commit("a", &["base"]),
			make_commit("b", &["base"]),
			make_commit("base", &[]),
		];
		let refs = vec![GitReference {
			name: "refs/heads/main".into(),
			sha: "merge".into(),
		}];
		let layout = compute_graph_layout(
			&commits,
			&refs,
			Some("merge"),
			&GraphConfig::default(),
			None,
		)
		.unwrap();
		assert_eq!(layout.rows[0].node.node_type, NodeType::Merge);
		assert!(layout.rows[0].node.is_head);
		assert!(verify_graph_invariants(&layout, &commits).is_ok());
	}

	#[test]
	fn test_merge_parent_link_reuse() {
		let commits = vec![
			make_commit("N", &["B"]),
			make_commit("M", &["A", "B"]),
			make_commit("A", &["X"]),
			make_commit("B", &["X"]),
			make_commit("X", &[]),
		];
		let layout = build_graph_layout(&commits, &[], None).unwrap();
		assert_eq!(layout.rows.len(), 5);
		assert!(!layout.links.is_empty());
		assert!(verify_graph_invariants(&layout, &commits).is_ok());
	}

	#[test]
	fn test_real_shallow_git_history_layout() {
		use std::fs;
		use std::path::Path;

		fn run_git(root: &Path, args: &[&str]) -> String {
			let out = std::process::Command::new("git")
				.args(args)
				.current_dir(root)
				.output()
				.unwrap();
			assert!(
				out.status.success(),
				"git {:?} failed: {:?}",
				args,
				String::from_utf8_lossy(&out.stderr)
			);
			String::from_utf8_lossy(&out.stdout).trim().to_string()
		}

		fn commit_file(root: &Path, msg: &str) -> String {
			run_git(
				root,
				&[
					"-c",
					"user.name=Tester",
					"-c",
					"user.email=tester@example.com",
					"commit",
					"--allow-empty",
					"-qm",
					msg,
				],
			);
			run_git(root, &["rev-parse", "HEAD"])
		}

		// Create source repo with 3 commits
		let origin_dir = tempfile::tempdir().unwrap();
		let origin = origin_dir.path();
		run_git(origin, &["init", "-q", "-b", "main"]);
		fs::write(origin.join("f.txt"), "1\n").unwrap();
		run_git(origin, &["add", "f.txt"]);
		let _c1 = commit_file(origin, "c1");
		fs::write(origin.join("f.txt"), "2\n").unwrap();
		run_git(origin, &["add", "f.txt"]);
		let _c2 = commit_file(origin, "c2");
		fs::write(origin.join("f.txt"), "3\n").unwrap();
		run_git(origin, &["add", "f.txt"]);
		let c3 = commit_file(origin, "c3");

		// Clone with depth 1
		let shallow_dir = tempfile::tempdir().unwrap();
		let shallow = shallow_dir.path();
		run_git(
			shallow,
			&[
				"clone",
				"--depth",
				"1",
				"--no-local",
				origin.to_str().unwrap(),
				shallow.to_str().unwrap(),
			],
		);
		let git = crate::gitsrc::Git::open(shallow).unwrap();

		let history = crate::browser::history(&git, None, "", 0, 10).unwrap();
		assert_eq!(history.commits.len(), 1);
		assert_eq!(history.commits[0].sha, c3);
		// In a shallow clone, git rev-list reports 0 parents
		assert!(history.commits[0].parents.is_empty());

		let mut shallow_roots = HashSet::new();
		shallow_roots.insert(c3.clone());

		let config = GraphConfig {
			shallow_roots,
			..Default::default()
		};

		let layout = compute_graph_layout(
			&history.commits,
			&history.refs,
			history.head.as_deref(),
			&config,
			None,
		)
		.unwrap();

		assert_eq!(layout.rows[0].node.node_type, NodeType::ShallowRoot);
		assert_eq!(
			layout.rows[0].parent_edges[0].continuation,
			ContinuationKind::ShallowBoundary
		);
		assert!(verify_graph_invariants(&layout, &history.commits).is_ok());
	}
}
