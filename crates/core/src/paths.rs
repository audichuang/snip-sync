//! Path normalization and root resolution for restore targets.
//!
//! Port of ClipCodeVSCode `src/pathResolver.ts`. Clipboard paths are handled
//! as slash-normalized strings end to end (a `D:/x` path is "absolute" on
//! every platform); only the resolved target and containment checks touch
//! native paths.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

/// A restore target that passed every check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedRestoreTarget {
	pub relative_path: String,
	/// Native path the restore writes to and re-checks containment on.
	pub absolute_path: PathBuf,
	pub root_path: PathBuf,
	pub existed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum RejectReason {
	#[serde(rename = "unsafe path")]
	UnsafePath,
	#[serde(rename = "outside workspace")]
	OutsideWorkspace,
	#[serde(rename = "ambiguous path")]
	AmbiguousPath,
	#[serde(rename = "missing path")]
	MissingPath,
}

impl RejectReason {
	/// The exact TS reason string.
	pub fn as_str(self) -> &'static str {
		match self {
			Self::UnsafePath => "unsafe path",
			Self::OutsideWorkspace => "outside workspace",
			Self::AmbiguousPath => "ambiguous path",
			Self::MissingPath => "missing path",
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RejectedRestoreTarget {
	pub reason: RejectReason,
	pub relative_path: Option<String>,
	/// Slash-normalized candidate targets, only for `AmbiguousPath`.
	pub candidates: Option<Vec<String>>,
}

pub type RestoreTargetResolution =
	Result<ResolvedRestoreTarget, RejectedRestoreTarget>;

/// The `// clipcode-root:` value: basename of the single root, or `None`
/// when there are several roots (paths are then labelled per root).
pub fn source_root_name<P: AsRef<Path>>(roots: &[P]) -> Option<String> {
	if roots.len() != 1 {
		return None;
	}
	normalize_system_path(&lossy(roots[0].as_ref()))
		.split('/')
		.rfind(|s| !s.is_empty())
		.map(str::to_owned)
}

pub fn to_clipboard_path(
	workspace_root: impl AsRef<Path>,
	absolute_path: impl AsRef<Path>,
) -> String {
	to_clipboard_path_from_roots(&[workspace_root], absolute_path, None)
}

/// `primary_root` defaults to the first root, as in TS.
pub fn to_clipboard_path_from_roots<P: AsRef<Path>>(
	workspace_roots: &[P],
	absolute_path: impl AsRef<Path>,
	primary_root: Option<&Path>,
) -> String {
	PathResolver::new(workspace_roots, primary_root)
		.to_clipboard_path(&lossy(absolute_path.as_ref()))
}

pub fn resolve_restore_target(
	workspace_root: impl AsRef<Path>,
	clipboard_path: &str,
) -> RestoreTargetResolution {
	resolve_write_target(&[workspace_root], clipboard_path)
}

pub fn resolve_write_target<P: AsRef<Path>>(
	workspace_roots: &[P],
	clipboard_path: &str,
) -> RestoreTargetResolution {
	PathResolver::new(workspace_roots, None)
		.resolve_write_target(clipboard_path)
}

pub fn resolve_delete_target<P: AsRef<Path>>(
	workspace_roots: &[P],
	clipboard_path: &str,
) -> RestoreTargetResolution {
	PathResolver::new(workspace_roots, None)
		.resolve_delete_target(clipboard_path)
}

struct RootEntry {
	/// Slash-normalized.
	path: String,
	is_primary: bool,
	clipboard_label: Option<String>,
	has_ambiguous_label: bool,
}

struct TargetCandidate {
	/// Index into `ordered_roots`.
	root: usize,
	/// Slash-normalized.
	target: String,
	root_relative_path: String,
}

struct PathResolver {
	ordered_roots: Vec<RootEntry>,
	primary_root: Option<usize>,
	primary_reserved_labels: HashSet<String>,
}

fn unsafe_path(relative_path: Option<String>) -> RejectedRestoreTarget {
	reject(RejectReason::UnsafePath, relative_path)
}

fn reject(
	reason: RejectReason,
	relative_path: Option<String>,
) -> RejectedRestoreTarget {
	RejectedRestoreTarget {
		reason,
		relative_path,
		candidates: None,
	}
}

impl PathResolver {
	fn new<P: AsRef<Path>>(roots: &[P], primary: Option<&Path>) -> Self {
		let normalized_roots = distinct_by(
			roots
				.iter()
				.map(|r| normalize_system_path(&lossy(r.as_ref())))
				.filter(|r| !r.is_empty())
				.collect(),
			|r| path_key(r),
		);
		// TS: `primaryRootPath ? normalize(primaryRootPath) : normalizedRoots[0]`
		// with the default being the raw first root; an empty result behaves
		// exactly like no primary at all.
		let raw_primary = match primary {
			Some(p) => Some(lossy(p)),
			None => roots.first().map(|r| lossy(r.as_ref())),
		};
		let normalized_primary = match raw_primary {
			Some(p) if !p.is_empty() => Some(normalize_system_path(&p)),
			_ => normalized_roots.first().cloned(),
		}
		.filter(|p| !p.is_empty());
		let mut all = Vec::new();
		all.extend(normalized_primary.clone());
		all.extend(normalized_roots);
		let all_root_paths = distinct_by(all, |r| path_key(r));

		let external_labels: Vec<String> = match &normalized_primary {
			Some(np) => all_root_paths
				.iter()
				.filter(|r| !same_path(r, np) && !is_under_root(r, np))
				.map(|r| basename(r))
				.filter(|l| !l.is_empty())
				.collect(),
			None => Vec::new(),
		};
		let mut label_counts: HashMap<&str, usize> = HashMap::new();
		for label in &external_labels {
			*label_counts.entry(label).or_default() += 1;
		}

		let mut reserved = HashSet::new();
		if let Some(np) = &normalized_primary {
			for root in &all_root_paths {
				if !same_path(root, np) && is_under_root(root, np) {
					if let Some(first) = relativize_path(root, np)
						.and_then(|r| r.split('/').next().map(str::to_owned))
						.filter(|s| !s.is_empty())
					{
						reserved.insert(first);
					}
				}
			}
			for label in &external_labels {
				if native(&join_str(np, label)).exists() {
					reserved.insert(label.clone());
				}
			}
		}

		let mut ordered_roots: Vec<RootEntry> = all_root_paths
			.iter()
			.map(|root| {
				let is_primary = normalized_primary
					.as_ref()
					.is_some_and(|np| same_path(root, np));
				let is_external = !is_primary
					&& normalized_primary
						.as_ref()
						.is_none_or(|np| !is_under_root(root, np));
				let clipboard_label = if is_external {
					Some(basename(root)).filter(|l| !l.is_empty())
				} else {
					None
				};
				let has_ambiguous_label =
					clipboard_label.as_ref().is_some_and(|l| {
						label_counts.get(l.as_str()).copied().unwrap_or(0) != 1
							|| reserved.contains(l)
					});
				RootEntry {
					path: root.clone(),
					is_primary,
					clipboard_label,
					has_ambiguous_label,
				}
			})
			.collect();
		// Stable sort, longest first; TS compares UTF-16 lengths.
		ordered_roots.sort_by_key(|r| std::cmp::Reverse(utf16_len(&r.path)));
		let primary_root = ordered_roots
			.iter()
			.position(|r| r.is_primary)
			.or((!ordered_roots.is_empty()).then_some(0));

		Self {
			ordered_roots,
			primary_root,
			primary_reserved_labels: reserved,
		}
	}

	fn others(&self) -> impl Iterator<Item = (usize, &RootEntry)> {
		self.ordered_roots
			.iter()
			.enumerate()
			.filter(move |(i, _)| Some(*i) != self.primary_root)
	}

	fn primary(&self) -> Option<&RootEntry> {
		self.primary_root.map(|i| &self.ordered_roots[i])
	}

	fn candidate(&self, root: usize, rel: String) -> TargetCandidate {
		TargetCandidate {
			root,
			target: join_str(&self.ordered_roots[root].path, &rel),
			root_relative_path: rel,
		}
	}

	fn to_clipboard_path(&self, absolute_path: &str) -> String {
		let normalized = normalize_system_path(absolute_path);
		if let Some(primary) = self.primary() {
			if let Some(rel) = relativize_path(&normalized, &primary.path) {
				return rel;
			}
		}
		for (_, root) in self.others() {
			let Some(rel) = relativize_path(&normalized, &root.path) else {
				continue;
			};
			return match &root.clipboard_label {
				Some(label) if !root.has_ambiguous_label => {
					if rel.is_empty() {
						label.clone()
					} else {
						format!("{label}/{rel}")
					}
				}
				_ => normalized,
			};
		}
		normalized
	}

	fn resolve_write_target(&self, raw_path: &str) -> RestoreTargetResolution {
		let absolute = self
			.absolute_root_candidate(raw_path)
			.or_else(|| self.cross_machine_suffix_candidate(raw_path))
			.or_else(|| self.literal_absolute_candidate(raw_path));
		if let Some(c) = absolute {
			let rel = c.root_relative_path.clone();
			return self.resolve_write_candidate(c, rel, None);
		}

		let Some(relative_path) = self.to_relative_project_path(raw_path)
		else {
			return Err(unsafe_path(None));
		};

		let mut explicit = self.explicit_root_label_candidates(&relative_path);
		if explicit.len() > 1 {
			return Err(ambiguous(relative_path, &explicit));
		}
		if let Some(c) = explicit.pop() {
			let rel = c.root_relative_path.clone();
			return self.resolve_write_candidate(c, rel, None);
		}

		let targets = self.legacy_target_candidates(&relative_path);
		if targets.is_empty() {
			return Err(reject(
				RejectReason::OutsideWorkspace,
				Some(relative_path),
			));
		}

		let (mut primary_existing, mut other_existing) = (None, Vec::new());
		for c in targets {
			if !is_existing_file(&c.target) {
				continue;
			}
			if self.ordered_roots[c.root].is_primary {
				// TS `find`: first match only.
				if primary_existing.is_none() {
					primary_existing = Some(c);
				}
			} else {
				other_existing.push(c);
			}
		}

		if let Some(p) = primary_existing {
			if !other_existing.is_empty()
				&& !self.has_nested_root_prefix(&relative_path)
			{
				other_existing.insert(0, p);
				return Err(ambiguous(relative_path, &other_existing));
			}
			return self.resolve_write_candidate(p, relative_path, None);
		}
		if other_existing.len() > 1 {
			return Err(ambiguous(relative_path, &other_existing));
		}
		if let Some(c) = other_existing.pop() {
			return self.resolve_write_candidate(c, relative_path, None);
		}

		let Some(primary) = self.primary_root else {
			return Err(reject(
				RejectReason::OutsideWorkspace,
				Some(relative_path),
			));
		};
		let c = self.candidate(primary, relative_path.clone());
		self.resolve_write_candidate(c, relative_path, Some(false))
	}

	fn resolve_delete_candidate(
		&self,
		c: TargetCandidate,
		relative_path: String,
	) -> RestoreTargetResolution {
		if self.escapes(&c.target) {
			return Err(unsafe_path(Some(relative_path)));
		}
		Ok(self.resolved_target(&c, relative_path, true))
	}

	/// Delete an existing file, or report it missing.
	fn delete_existing(&self, c: TargetCandidate) -> RestoreTargetResolution {
		let rel = c.root_relative_path.clone();
		if is_existing_file(&c.target) {
			self.resolve_delete_candidate(c, rel)
		} else {
			Err(reject(RejectReason::MissingPath, Some(rel)))
		}
	}

	fn resolve_delete_target(&self, raw_path: &str) -> RestoreTargetResolution {
		if let Some(c) = self.absolute_root_candidate(raw_path) {
			return self.delete_existing(c);
		}
		if let Some(c) = self.cross_machine_suffix_candidate(raw_path) {
			return self.delete_existing(c);
		}

		let Some(relative_path) = self.to_relative_project_path(raw_path)
		else {
			return Err(unsafe_path(None));
		};

		let mut explicit = self.explicit_root_label_candidates(&relative_path);
		if explicit.len() > 1 {
			return Err(ambiguous(relative_path, &explicit));
		}
		if let Some(c) = explicit.pop() {
			return self.delete_existing(c);
		}

		let mut existing: Vec<TargetCandidate> = self
			.legacy_target_candidates(&relative_path)
			.into_iter()
			.filter(|c| is_existing_file(&c.target))
			.collect();
		if existing.len() > 1 {
			return Err(ambiguous(relative_path, &existing));
		}
		match existing.pop() {
			Some(c) => self.resolve_delete_candidate(c, relative_path),
			None => Err(reject(RejectReason::MissingPath, Some(relative_path))),
		}
	}

	fn to_relative_project_path(&self, raw_path: &str) -> Option<String> {
		let normalized = normalize_system_path(raw_path);
		if normalized.is_empty() {
			return None;
		}
		if !is_absolute_path(&normalized) {
			return sanitize_relative_path(&normalized);
		}
		if let Some(primary) = self.primary() {
			if let Some(rel) = relativize_path(&normalized, &primary.path)
				.filter(|r| !r.is_empty())
			{
				return Some(rel);
			}
		}
		for (_, root) in self.others() {
			let Some(rel) = relativize_path(&normalized, &root.path)
				.filter(|r| !r.is_empty())
			else {
				continue;
			};
			return Some(match &root.clipboard_label {
				Some(label) if !root.has_ambiguous_label => {
					format!("{label}/{rel}")
				}
				_ => rel,
			});
		}
		self.cross_machine_suffix_relative_path(&normalized)
	}

	/// Unmapped absolute paths retain every directory under the primary root.
	/// Only the drive colon/root separator is removed. Writes only: a foreign
	/// delete must not acquire a new target through this fallback.
	fn literal_absolute_candidate(
		&self,
		raw_path: &str,
	) -> Option<TargetCandidate> {
		let normalized = normalize_system_path(raw_path);
		let primary = self.primary_root?;
		if !is_absolute_path(&normalized) || is_drive_root(&normalized) {
			return None;
		}
		let stripped = if has_drive_slash(&normalized) {
			// `X:/rest` -> `X/rest`
			format!("{}{}", &normalized[..1], &normalized[2..])
		} else {
			normalized
		};
		let rel = sanitize_relative_path(&stripped)?;
		Some(self.candidate(primary, rel))
	}

	fn absolute_root_candidate(
		&self,
		raw_path: &str,
	) -> Option<TargetCandidate> {
		let normalized = normalize_system_path(raw_path);
		if !is_absolute_path(&normalized) {
			return None;
		}
		self.ordered_roots.iter().enumerate().find_map(|(i, root)| {
			relativize_path(&normalized, &root.path)
				.map(|rel| self.candidate(i, rel))
		})
	}

	fn explicit_root_label_candidates(
		&self,
		relative_path: &str,
	) -> Vec<TargetCandidate> {
		let Some((first, rest)) = relative_path.split_once('/') else {
			return Vec::new();
		};
		if first.is_empty() || rest.is_empty() {
			return Vec::new();
		}
		let mut candidates = Vec::new();
		if self.primary_reserved_labels.contains(first) {
			if let Some(p) = self.primary_root {
				candidates.push(self.candidate(p, relative_path.to_owned()));
			}
		}
		for (i, root) in self.ordered_roots.iter().enumerate() {
			if root.clipboard_label.as_deref() == Some(first) {
				candidates.push(self.candidate(i, rest.to_owned()));
			}
		}
		distinct_by(candidates, |c| path_key(&c.target))
	}

	fn legacy_target_candidates(
		&self,
		relative_path: &str,
	) -> Vec<TargetCandidate> {
		// Roots, primary first; roots are already distinct by key.
		let order = self
			.primary_root
			.into_iter()
			.chain(self.others().map(|(i, _)| i));
		distinct_by(
			order
				.map(|i| self.candidate(i, relative_path.to_owned()))
				.collect(),
			|c| path_key(&c.target),
		)
	}

	fn has_nested_root_prefix(&self, relative_path: &str) -> bool {
		let first = relative_path.split('/').next().unwrap_or("");
		if first.is_empty() {
			return false;
		}
		let windows = is_windows_style_path(relative_path);
		self.others()
			.any(|(_, r)| segments_match(&basename(&r.path), first, windows))
	}

	/// The suffix match already determines a UNIQUE target root; the candidate
	/// keeps it so an external root's file is not written over the primary's.
	fn cross_machine_suffix_candidate(
		&self,
		raw_path: &str,
	) -> Option<TargetCandidate> {
		// Absolute only: the "payload came from another machine" fallback.
		let absolute = normalize_system_path(raw_path);
		if absolute.is_empty() || !is_absolute_path(&absolute) {
			return None;
		}
		let without_drive = if has_drive_slash(&absolute) {
			&absolute[3..]
		} else {
			&absolute
		};
		let segments: Vec<&str> = without_drive
			.trim_start_matches('/')
			.split('/')
			.filter(|s| !s.is_empty())
			.collect();
		let windows = is_windows_style_path(&absolute)
			|| self
				.ordered_roots
				.iter()
				.any(|r| is_windows_style_path(&r.path));

		let mut candidates = Vec::new();
		for (i, root) in self.ordered_roots.iter().enumerate() {
			let root_name = basename(&root.path);
			if root_name.is_empty() {
				continue;
			}
			for index in 0..segments.len().saturating_sub(1) {
				if !segments_match(segments[index], &root_name, windows) {
					continue;
				}
				let Some(rel) =
					sanitize_relative_path(&segments[index + 1..].join("/"))
				else {
					continue;
				};
				candidates.push(self.candidate(i, rel));
			}
		}

		let keys: HashSet<String> =
			candidates.iter().map(|c| path_key(&c.target)).collect();
		if keys.len() != 1 {
			return None;
		}
		let pos = candidates
			.iter()
			.position(|c| self.ordered_roots[c.root].is_primary)
			.unwrap_or(0);
		Some(candidates.swap_remove(pos))
	}

	fn cross_machine_suffix_relative_path(
		&self,
		absolute_path: &str,
	) -> Option<String> {
		let winner = self.cross_machine_suffix_candidate(absolute_path)?;
		let root = &self.ordered_roots[winner.root];
		match &root.clipboard_label {
			Some(label) if !root.is_primary && !root.has_ambiguous_label => {
				Some(format!("{label}/{}", winner.root_relative_path))
			}
			_ => Some(winner.root_relative_path),
		}
	}

	fn escapes(&self, target: &str) -> bool {
		let roots: Vec<PathBuf> =
			self.ordered_roots.iter().map(|r| native(&r.path)).collect();
		escapes_all_roots(&roots, native(target))
	}

	fn resolve_write_candidate(
		&self,
		c: TargetCandidate,
		relative_path: String,
		existed: Option<bool>,
	) -> RestoreTargetResolution {
		if self.escapes(&c.target) {
			return Err(unsafe_path(Some(relative_path)));
		}
		let existed = existed.unwrap_or_else(|| is_existing_file(&c.target));
		Ok(self.resolved_target(&c, relative_path, existed))
	}

	fn resolved_target(
		&self,
		c: &TargetCandidate,
		relative_path: String,
		existed: bool,
	) -> ResolvedRestoreTarget {
		// NATIVE separators, never slash-normalised: this is the path restore
		// writes to and re-checks containment on.
		ResolvedRestoreTarget {
			relative_path,
			absolute_path: absolutize(&native(&c.target)),
			root_path: absolutize(&native(&self.ordered_roots[c.root].path)),
			existed,
		}
	}
}

fn ambiguous(
	relative_path: String,
	candidates: &[TargetCandidate],
) -> RejectedRestoreTarget {
	let paths = distinct_by(
		candidates
			.iter()
			.map(|c| normalize_system_path(&c.target))
			.collect(),
		|p| path_key(p),
	);
	RejectedRestoreTarget {
		reason: RejectReason::AmbiguousPath,
		relative_path: Some(relative_path),
		candidates: Some(paths),
	}
}

fn lossy(p: &Path) -> String {
	p.to_string_lossy().into_owned()
}

/// Trim only the six ASCII whitespace characters, never Unicode ones.
fn ascii_trim(s: &str) -> &str {
	s.trim_matches(|c| matches!(c, ' ' | '\t' | '\n' | '\x0B' | '\x0C' | '\r'))
}

/// Replace `\` with `/` and collapse runs of `/`.
fn collapse_slashes(s: &str) -> String {
	let mut out = String::with_capacity(s.len());
	for c in s.chars() {
		let c = if c == '\\' { '/' } else { c };
		if c == '/' && out.ends_with('/') {
			continue;
		}
		out.push(c);
	}
	out
}

fn sanitize_relative_path(value: &str) -> Option<String> {
	let collapsed = collapse_slashes(ascii_trim(value));
	let normalized = collapsed.trim_start_matches('/');
	if normalized.is_empty() || is_absolute_path(normalized) {
		return None;
	}
	let segments: Vec<&str> = normalized
		.split('/')
		.filter(|s| !s.is_empty() && *s != ".")
		.collect();
	if segments.is_empty() {
		return None;
	}
	// Control characters and Windows-illegal <>:"|?* are refused on every
	// platform so one payload restores identically everywhere.
	let bad = |s: &&str| {
		*s == ".."
			|| s.chars().any(|c| {
				matches!(
					c,
					'<' | '>' | ':' | '"' | '|' | '?' | '*' | '\0'..='\x1F'
				)
			})
	};
	if segments.iter().any(bad) {
		return None;
	}
	Some(segments.join("/"))
}

fn normalize_system_path(value: &str) -> String {
	let mut s = collapse_slashes(ascii_trim(value));
	if s != "/" && !is_drive_root(&s) {
		while s.ends_with('/') {
			s.pop();
		}
	}
	s
}

fn has_drive(s: &str) -> bool {
	let b = s.as_bytes();
	b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

/// `^[A-Za-z]:/`
fn has_drive_slash(s: &str) -> bool {
	has_drive(s) && s.as_bytes().get(2) == Some(&b'/')
}

/// `^[A-Za-z]:/$`
fn is_drive_root(s: &str) -> bool {
	has_drive_slash(s) && s.len() == 3
}

fn is_absolute_path(s: &str) -> bool {
	s.starts_with('/') || has_drive_slash(s)
}

/// `^[A-Za-z]:(/.*)?$`
fn is_windows_style_path(s: &str) -> bool {
	has_drive(s) && (s.len() == 2 || s.as_bytes()[2] == b'/')
}

fn path_key(value: &str) -> String {
	let normalized = normalize_system_path(value);
	// TS `toLowerCase()` folds Unicode, so `to_lowercase` matches it here.
	if is_windows_style_path(&normalized) {
		normalized.to_lowercase()
	} else {
		normalized
	}
}

fn segments_match(left: &str, right: &str, windows: bool) -> bool {
	if windows {
		left.to_lowercase() == right.to_lowercase()
	} else {
		left == right
	}
}

fn relativize_path(absolute_path: &str, root_path: &str) -> Option<String> {
	let abs = normalize_system_path(absolute_path);
	let root = normalize_system_path(root_path);
	let abs_key = path_key(&abs);
	let root_key = path_key(&root);
	if abs_key == root_key {
		return Some(String::new());
	}
	if !abs_key.starts_with(&format!("{root_key}/")) {
		return None;
	}
	sanitize_relative_path(abs.get(root.len() + 1..)?)
}

fn same_path(left: &str, right: &str) -> bool {
	path_key(left) == path_key(right)
}

fn is_under_root(value: &str, root: &str) -> bool {
	let value_key = path_key(value);
	let root_key = path_key(root);
	value_key != root_key && value_key.starts_with(&format!("{root_key}/"))
}

/// Last segment of a slash-normalized path (`path.basename`).
fn basename(p: &str) -> String {
	if is_drive_root(p) {
		return String::new();
	}
	p.trim_end_matches('/')
		.rsplit('/')
		.next()
		.unwrap_or("")
		.to_owned()
}

/// `path.resolve(root, rel)` for a slash-normalized root and a sanitized
/// relative path (no `.` or `..` segments).
fn join_str(root: &str, rel: &str) -> String {
	if rel.is_empty() {
		root.to_owned()
	} else if root.ends_with('/') {
		format!("{root}{rel}")
	} else {
		format!("{root}/{rel}")
	}
}

/// Convert a slash-normalized path to native separators.
fn native(p: &str) -> PathBuf {
	Path::new(p).components().collect()
}

fn is_existing_file(target: &str) -> bool {
	fs::metadata(native(target)).is_ok_and(|m| m.is_file())
}

fn utf16_len(s: &str) -> usize {
	s.encode_utf16().count()
}

fn distinct_by<T>(values: Vec<T>, key: impl Fn(&T) -> String) -> Vec<T> {
	let mut seen = HashSet::new();
	values.into_iter().filter(|v| seen.insert(key(v))).collect()
}

/// `path.resolve(p)`: absolute and lexically normalized.
fn absolutize(p: &Path) -> PathBuf {
	let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
	let mut out = PathBuf::new();
	for c in abs.components() {
		match c {
			std::path::Component::CurDir => {}
			std::path::Component::ParentDir => {
				out.pop();
			}
			other => out.push(other),
		}
	}
	out
}

/// True when the target's REAL location is no longer inside any root, i.e. a
/// symlink inside the workspace points out of it. Both the target and the
/// roots are resolved, so a workspace reached through a symlinked path is
/// not read as an escape from itself. Fails closed when containment cannot
/// be established.
pub fn escapes_all_roots<P: AsRef<Path>>(
	roots: &[P],
	target_path: impl AsRef<Path>,
) -> bool {
	let Some(real) = containment_target(target_path.as_ref(), 0) else {
		return true;
	};
	!roots
		.iter()
		.map(|r| real_or_self(r.as_ref()))
		.any(|root| is_contained(&real, &root))
}

/// Component-wise containment on native paths, never on strings.
fn is_contained(real_target: &Path, real_root: &Path) -> bool {
	#[cfg(windows)]
	{
		// Windows paths compare case-insensitively (as node's path.relative).
		let t = PathBuf::from(real_target.to_string_lossy().to_lowercase());
		let r = PathBuf::from(real_root.to_string_lossy().to_lowercase());
		t.starts_with(r)
	}
	#[cfg(not(windows))]
	real_target.starts_with(real_root)
}

/// Where this path REALLY lands with every symlink resolved, or `None` when
/// that cannot be established.
///
/// When the path does not exist, the deepest existing ancestor is resolved
/// and the remaining names appended; the first name below it may be a
/// dangling symlink and is read relative to that RESOLVED parent. The
/// ancestor walk is unbounded (it ends at the filesystem root); only symlink
/// hops are capped, because only they can cycle.
fn containment_target(target_path: &Path, hops: u32) -> Option<PathBuf> {
	let target = absolutize(target_path);
	if let Ok(real) = dunce::canonicalize(&target) {
		return Some(real);
	}
	if hops > 40 {
		// Pathological symlink nest: cannot establish, so refuse.
		return None;
	}
	for ancestor in target.ancestors().skip(1) {
		let Ok(real) = dunce::canonicalize(ancestor) else {
			continue;
		};
		let rest = target.strip_prefix(ancestor).ok()?;
		let mut names = rest.components();
		let first = real.join(names.next()?);
		if fs::symlink_metadata(&first).is_ok_and(|m| m.is_symlink()) {
			if let Ok(link) = fs::read_link(&first) {
				let linked = real.join(link).join(names.as_path());
				return containment_target(&linked, hops + 1);
			}
		}
		return Some(real.join(rest));
	}
	// Nothing on the path exists at all, so there is nothing to escape through.
	Some(target)
}

fn real_or_self(p: &Path) -> PathBuf {
	containment_target(p, 0).unwrap_or_else(|| absolutize(p))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn tmp() -> (tempfile::TempDir, PathBuf) {
		let dir = tempfile::tempdir().unwrap();
		let real = dunce::canonicalize(dir.path()).unwrap();
		(dir, real)
	}

	#[cfg(unix)]
	fn symlink(target: &Path, link: &Path) {
		std::os::unix::fs::symlink(target, link).unwrap();
	}

	#[cfg(windows)]
	fn symlink(target: &Path, link: &Path) {
		std::os::windows::fs::symlink_dir(target, link).unwrap();
	}

	fn ok(r: &RestoreTargetResolution) -> &ResolvedRestoreTarget {
		r.as_ref().expect("resolved")
	}

	#[test]
	fn converts_workspace_files_to_slash_separated_clipboard_paths() {
		let root = absolutize(Path::new("/tmp/project"));
		assert_eq!(
			to_clipboard_path(&root, root.join("src").join("main.ts")),
			"src/main.ts"
		);
	}

	#[test]
	fn keeps_windows_style_clipboard_paths_slash_separated() {
		assert_eq!(
			to_clipboard_path("C:/repo/app", "C:\\repo\\app\\src\\main.ts"),
			"src/main.ts"
		);
	}

	#[test]
	fn labels_files_from_sibling_workspace_roots() {
		let (_d, parent) = tmp();
		let primary = parent.join("app");
		let sibling = parent.join("shared-lib");
		fs::create_dir_all(primary.join("src")).unwrap();
		fs::create_dir_all(sibling.join("src")).unwrap();
		let roots = [&primary, &sibling];
		assert_eq!(
			to_clipboard_path_from_roots(
				&roots,
				sibling.join("src/util.ts"),
				None
			),
			"shared-lib/src/util.ts"
		);
		assert_eq!(
			to_clipboard_path_from_roots(
				&roots,
				primary.join("src/main.ts"),
				None
			),
			"src/main.ts"
		);
	}

	#[test]
	fn source_root_name_only_for_a_single_root() {
		assert_eq!(source_root_name(&["/a/repo/"]), Some("repo".into()));
		assert_eq!(source_root_name(&["/a", "/b"]), None);
	}

	#[test]
	fn resolves_safe_restore_target_under_missing_root() {
		let root = absolutize(Path::new("/tmp/project"));
		let r = resolve_restore_target(&root, "src/main.ts");
		assert_eq!(ok(&r).relative_path, "src/main.ts");
		assert_eq!(ok(&r).absolute_path, root.join("src").join("main.ts"));
	}

	#[test]
	fn rejects_traversal_and_invalid_segments() {
		let root = absolutize(Path::new("/tmp/project"));
		assert!(resolve_restore_target(&root, "../secret.txt").is_err());
		assert!(resolve_restore_target(&root, "src/bad:name.ts").is_err());
		let abs = resolve_restore_target(&root, "/etc/passwd");
		assert_eq!(ok(&abs).absolute_path, root.join("etc").join("passwd"));
	}

	#[test]
	fn resolves_explicit_sibling_root_labels() {
		let (_d, parent) = tmp();
		let primary = parent.join("app");
		let sibling = parent.join("shared-lib");
		fs::create_dir_all(&primary).unwrap();
		fs::create_dir_all(&sibling).unwrap();
		let r = resolve_write_target(
			&[&primary, &sibling],
			"shared-lib/src/New.ts",
		);
		assert_eq!(ok(&r).relative_path, "src/New.ts");
		assert_eq!(ok(&r).absolute_path, sibling.join("src").join("New.ts"));
	}

	#[test]
	fn marks_existing_files_across_roots_as_ambiguous() {
		let (_d, parent) = tmp();
		let primary = parent.join("app");
		let sibling = parent.join("shared-lib");
		fs::create_dir_all(primary.join("src")).unwrap();
		fs::create_dir_all(sibling.join("src")).unwrap();
		fs::write(primary.join("src/App.ts"), "primary").unwrap();
		fs::write(sibling.join("src/App.ts"), "sibling").unwrap();
		let roots = [&primary, &sibling];
		for r in [
			resolve_write_target(&roots, "src/App.ts"),
			resolve_delete_target(&roots, "src/App.ts"),
		] {
			let e = r.unwrap_err();
			assert_eq!(e.reason, RejectReason::AmbiguousPath);
			assert_eq!(e.candidates.map(|c| c.len()), Some(2));
		}
	}

	#[test]
	fn accepts_absolute_restore_paths_under_known_roots() {
		let (_d, root) = tmp();
		fs::create_dir_all(root.join("src")).unwrap();
		let abs = root.join("src").join("App.ts");
		let r = resolve_write_target(&[&root], abs.to_str().unwrap());
		assert_eq!(ok(&r).relative_path, "src/App.ts");
		assert_eq!(ok(&r).absolute_path, abs);
	}

	#[test]
	fn write_targets_may_follow_links_inside_never_escaping_ones() {
		let (_d, parent) = tmp();
		let root = parent.join("project");
		let outside = parent.join("outside");
		fs::create_dir_all(root.join("packages/ui")).unwrap();
		fs::create_dir_all(root.join("node_modules")).unwrap();
		fs::create_dir_all(&outside).unwrap();
		symlink(&outside, &root.join("linked"));
		symlink(&root.join("packages/ui"), &root.join("node_modules/ui"));

		let e = resolve_write_target(&[&root], "linked/escape.ts").unwrap_err();
		assert_eq!(e.reason, RejectReason::UnsafePath);
		assert!(
			resolve_write_target(&[&root], "node_modules/ui/index.ts").is_ok()
		);
	}

	#[test]
	fn missing_root_is_canonicalized_like_its_targets() {
		let (_d, parent) = tmp();
		let real = parent.join("real");
		fs::create_dir_all(&real).unwrap();
		symlink(&real, &parent.join("link"));

		let root = parent.join("link").join("workspace");
		assert!(resolve_write_target(&[&root], "src/New.ts").is_ok());

		let escape_root = parent.join("link").join("ws2");
		fs::create_dir_all(real.join("ws2")).unwrap();
		fs::create_dir_all(parent.join("outside")).unwrap();
		symlink(&parent.join("outside"), &real.join("ws2").join("out"));
		assert!(resolve_write_target(&[&escape_root], "out/escape.ts").is_err());
	}

	#[test]
	fn external_root_keeps_identity_through_cross_machine_suffix() {
		let (_d, base) = tmp();
		let app = base.join("dest/app");
		let shared = base.join("dest/shared-lib");
		fs::create_dir_all(app.join("shared-lib")).unwrap();
		fs::create_dir_all(&shared).unwrap();
		let r =
			resolve_write_target(&[&app, &shared], "/source/shared-lib/new.ts");
		assert_eq!(ok(&r).absolute_path, shared.join("new.ts"));
	}

	#[test]
	fn deletion_must_not_escape_through_a_directory_symlink() {
		let (_d, base) = tmp();
		let repo = base.join("repo");
		let outside = base.join("outside");
		fs::create_dir_all(&repo).unwrap();
		fs::create_dir_all(&outside).unwrap();
		fs::write(outside.join("keep.txt"), "must survive").unwrap();
		symlink(&outside, &repo.join("link"));

		assert!(resolve_delete_target(&[&repo], "link/keep.txt").is_err());
		fs::write(repo.join("inside.txt"), "x").unwrap();
		assert!(resolve_delete_target(&[&repo], "inside.txt").is_ok());
	}

	// Leaf file symlinks: unix only (Windows file links need privileges).
	#[cfg(unix)]
	#[test]
	fn relative_leaf_symlink_resolves_against_its_real_parent() {
		let (_d, base) = tmp();
		let ws = base.join("ws");
		let outside = base.join("outside");
		fs::create_dir_all(ws.join("sub")).unwrap();
		fs::create_dir_all(&outside).unwrap();
		fs::write(outside.join("x"), "must survive").unwrap();
		symlink(&ws, &ws.join("sub/dirlink"));
		symlink(Path::new("../outside/x"), &ws.join("file"));

		assert!(resolve_write_target(&[&ws], "sub/dirlink/file").is_err());
		assert!(resolve_delete_target(&[&ws], "sub/dirlink/file").is_err());

		symlink(Path::new("../outside/brand-new.txt"), &ws.join("newfile"));
		assert!(resolve_write_target(&[&ws], "sub/dirlink/newfile").is_err());
	}

	#[test]
	fn containment_holds_under_many_missing_levels() {
		let (_d, base) = tmp();
		let repo = base.join("repo");
		let outside = base.join("outside");
		fs::create_dir_all(&repo).unwrap();
		fs::create_dir_all(&outside).unwrap();
		symlink(&outside, &repo.join("link"));

		let deep = format!("link/{}new.txt", "d/".repeat(42));
		assert!(resolve_write_target(&[&repo], &deep).is_err());
		assert!(resolve_write_target(&[&repo], "link/new.txt").is_err());
		let inside = format!("{}new.txt", "d/".repeat(42));
		assert!(resolve_write_target(&[&repo], &inside).is_ok());
	}

	// On Windows a backslash IS a separator, so this only holds on POSIX.
	#[cfg(unix)]
	#[test]
	fn literal_backslash_in_a_real_directory_name_is_not_a_separator() {
		let (_d, base) = tmp();
		let repo = base.join("repo");
		let sibling = base.join("repo\\outside");
		fs::create_dir_all(&repo).unwrap();
		fs::create_dir_all(&sibling).unwrap();
		symlink(&sibling, &repo.join("backlink"));
		assert!(resolve_write_target(&[&repo], "backlink/new.txt").is_err());
	}
}
