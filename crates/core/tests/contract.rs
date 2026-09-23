//! Shared cross-tool contract fixture (ClipCodeVSCode commit 0aa24c8).
//!
//! The fixture is committed byte-identically in every consumer and pinned by
//! SHA-256. Section types reject unknown fields so a fixture change that adds
//! a field fails loudly here instead of being silently ignored.

// Types are consumed by the per-section tests added in T-01..T-05.
#![allow(dead_code)]

use std::collections::BTreeMap;

use serde::Deserialize;
use sha2::{Digest, Sha256};

const FIXTURE: &[u8] = include_bytes!(concat!(
	env!("CARGO_MANIFEST_DIR"),
	"/../../fixtures/clipboard-contract.json"
));

const EXPECTED_FIXTURE_SHA: &str =
	"df317eb7b412d4bd71222d71d4cd64a1652fbcac2d82468ec417e4ce95ec2468";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Fixture {
	#[serde(rename = "_comment")]
	pub comment: String,
	pub build_cases: Vec<BuildCase>,
	pub parse_cases: Vec<ParseCase>,
	pub token_cases: Vec<TokenCase>,
	pub path_layout: PathLayout,
	pub path_cases: Vec<PathCase>,
	pub restore_layout: RestoreLayout,
	pub restore_cases: Vec<RestoreCase>,
}

// ---- buildCases ----

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BuildCase {
	pub name: String,
	/// "regular" or "git".
	pub kind: String,
	pub options: BuildOptions,
	pub wire: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BuildOptions {
	pub header_format: String,
	pub pre_text: String,
	pub post_text: String,
	pub add_extra_line_between_files: bool,
	pub source_root: Option<String>,
	pub files: Vec<BuildFile>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BuildFile {
	pub path: String,
	/// Absent when the file was skipped (see `skipped_reason`).
	pub content: Option<String>,
	pub skipped_reason: Option<String>,
	pub change_type: Option<String>,
}

// ---- parseCases ----

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ParseCase {
	pub name: String,
	pub header_format: String,
	pub input: String,
	pub expected: Vec<ParsedFile>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ParsedFile {
	pub path: String,
	pub content: String,
	pub change_types: Vec<String>,
}

// ---- tokenCases ----

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TokenCase {
	pub name: String,
	pub text: String,
	pub chars: u64,
	pub lines: u64,
	pub words: u64,
	pub tokens: u64,
}

// ---- pathLayout + pathCases ----

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PathLayout {
	pub roots: Vec<String>,
	pub dirs: Vec<String>,
	/// Relative file path -> text content.
	pub files: BTreeMap<String, String>,
	/// Link path -> target path, both relative to the layout base.
	pub symlinks: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PathCase {
	pub input: String,
	pub write: PathOutcome,
	/// Delete outcome, e.g. "refused" or "missing".
	pub delete: String,
	#[serde(default)]
	pub needs_symlink: bool,
}

/// Either a resolved target or a bare verdict string such as "refused".
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum PathOutcome {
	Target(RootedPath),
	Verdict(String),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RootedPath {
	pub root: String,
	pub path: String,
}

// ---- restoreLayout + restoreCases ----

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RestoreLayout {
	pub roots: Vec<String>,
	pub dirs: Vec<String>,
	pub files: BTreeMap<String, LayoutFile>,
	pub symlinks: BTreeMap<String, String>,
}

/// File body given either as UTF-8 text or as raw bytes in base64.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum LayoutFile {
	Text(String),
	Base64(String),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RestoreCase {
	pub name: String,
	pub header_format: String,
	pub payload: String,
	pub creates: Vec<RestoreCreate>,
	pub deletes: Vec<RestoreDelete>,
	pub skips: Vec<RestoreSkip>,
	#[serde(default)]
	pub needs_symlink: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RestoreCreate {
	pub root: String,
	pub path: String,
	pub relative_path: String,
	pub content: String,
	pub existed: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RestoreDelete {
	pub root: String,
	pub path: String,
	pub relative_path: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RestoreSkip {
	pub raw_path: String,
	pub relative_path: Option<String>,
	pub reason: String,
}

pub fn load() -> Fixture {
	serde_json::from_slice(FIXTURE).expect("fixture deserializes")
}

#[test]
fn fixture_sha_matches() {
	let actual: String = Sha256::digest(FIXTURE)
		.iter()
		.map(|b| format!("{b:02x}"))
		.collect();
	assert_eq!(actual, EXPECTED_FIXTURE_SHA);
}

#[test]
fn fixture_deserializes() {
	let f = load();
	assert!(!f.build_cases.is_empty());
	assert!(!f.parse_cases.is_empty());
	assert!(!f.token_cases.is_empty());
	assert!(!f.path_cases.is_empty());
	assert!(!f.restore_cases.is_empty());
}

#[test]
#[ignore = "T-01"]
fn build_cases() {
	todo!()
}

#[test]
#[ignore = "T-01"]
fn parse_cases() {
	todo!()
}

#[test]
#[ignore = "T-02"]
fn token_cases() {
	todo!()
}

#[test]
fn path_cases() {
	use snip_core::paths::{
		resolve_delete_target, resolve_write_target, RejectReason,
		RestoreTargetResolution,
	};
	use std::path::{Path, PathBuf};

	#[cfg(unix)]
	fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
		std::os::unix::fs::symlink(target, link)
	}
	#[cfg(windows)]
	fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
		std::os::windows::fs::symlink_dir(target, link)
	}

	// Mirrors the TS `outcome` helper: {root, path} relative to the layout
	// base, or a verdict string.
	fn outcome(parent: &Path, r: &RestoreTargetResolution) -> String {
		match r {
			Ok(t) => {
				let rel =
					t.absolute_path.strip_prefix(parent).unwrap_or_else(|_| {
						panic!(
							"{} is outside the layout",
							t.absolute_path.display()
						)
					});
				let mut parts =
					rel.components().map(|c| c.as_os_str().to_string_lossy());
				let root = parts.next().unwrap_or_default();
				let rest: Vec<_> = parts.collect();
				format!("{root}:{}", rest.join("/"))
			}
			Err(e) if e.reason == RejectReason::MissingPath => "missing".into(),
			Err(e) if e.reason == RejectReason::AmbiguousPath => {
				"ambiguous".into()
			}
			Err(_) => "refused".into(),
		}
	}

	let fixture = load();
	let layout = &fixture.path_layout;
	let tmp = tempfile::tempdir().unwrap();
	// Canonicalize first: macOS /var is a symlink to /private/var.
	let parent = dunce::canonicalize(tmp.path()).unwrap();
	for dir in &layout.dirs {
		std::fs::create_dir_all(parent.join(dir)).unwrap();
	}
	for (file, text) in &layout.files {
		std::fs::write(parent.join(file), text).unwrap();
	}
	// A directory symlink needs privileges on Windows outside developer mode;
	// only the rows that depend on it are skipped, and loudly.
	let mut symlinks = true;
	for (link, target) in &layout.symlinks {
		match symlink_dir(&parent.join(target), &parent.join(link)) {
			Ok(()) => {}
			// 1314 = ERROR_PRIVILEGE_NOT_HELD on Windows.
			Err(e)
				if e.kind() == std::io::ErrorKind::PermissionDenied
					|| e.raw_os_error() == Some(1314) =>
			{
				symlinks = false;
			}
			Err(e) => panic!("symlink {link}: {e}"),
		}
	}
	let roots: Vec<PathBuf> =
		layout.roots.iter().map(|r| parent.join(r)).collect();

	let mut skipped = Vec::new();
	let mut failures = Vec::new();
	for c in &fixture.path_cases {
		if c.needs_symlink && !symlinks {
			skipped.push(c.input.clone());
			continue;
		}
		let input = c
			.input
			.replace("@ROOT@", &roots[0].to_string_lossy())
			.replace("@SIBLING@", &roots[1].to_string_lossy());
		let expected_write = match &c.write {
			PathOutcome::Target(t) => format!("{}:{}", t.root, t.path),
			PathOutcome::Verdict(v) => v.clone(),
		};
		let actual = (
			outcome(&parent, &resolve_write_target(&roots, &input)),
			outcome(&parent, &resolve_delete_target(&roots, &input)),
		);
		if actual != (expected_write.clone(), c.delete.clone()) {
			failures.push(format!(
				"{:?}: expected ({expected_write}, {}), got {actual:?}",
				c.input, c.delete
			));
		}
	}
	if !skipped.is_empty() {
		println!(
			"SKIPPED {} symlink row(s): this platform refused to create a \
			 directory symlink: {skipped:?}",
			skipped.len()
		);
	}
	assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
#[ignore = "T-05"]
fn restore_cases() {
	todo!()
}
