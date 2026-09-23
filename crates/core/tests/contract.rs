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
fn build_cases() {
	use snip_core::format::{
		build_git_payload, build_payload, BuildPayloadOptions, ChangeType,
		PayloadFile,
	};
	for c in load().build_cases {
		let o = c.options;
		let options = BuildPayloadOptions {
			header_format: o.header_format,
			pre_text: o.pre_text,
			post_text: o.post_text,
			add_extra_line_between_files: o.add_extra_line_between_files,
			source_root: o.source_root,
			files: o
				.files
				.into_iter()
				.map(|f| PayloadFile {
					path: f.path,
					content: f.content,
					skipped_reason: f.skipped_reason,
					change_type: f.change_type.map(|t| {
						ChangeType::from_label(&t).expect("known label")
					}),
				})
				.collect(),
		};
		let built = if c.kind == "git" {
			build_git_payload(&options)
		} else {
			build_payload(&options)
		};
		assert_eq!(built, c.wire, "build {}: {}", c.kind, c.name);
	}
}

#[test]
fn parse_cases() {
	use snip_core::format::parse_clipboard;
	for c in load().parse_cases {
		let parsed: Vec<(String, String, Vec<String>)> =
			parse_clipboard(&c.input, &c.header_format)
				.into_iter()
				.map(|e| {
					let mut types: Vec<String> = e
						.change_types
						.iter()
						.map(|t| t.as_str().to_string())
						.collect();
					types.sort();
					(e.path, e.content, types)
				})
				.collect();
		let expected: Vec<(String, String, Vec<String>)> = c
			.expected
			.into_iter()
			.map(|e| (e.path, e.content, e.change_types))
			.collect();
		assert_eq!(parsed, expected, "parse: {}", c.name);
	}
}

#[test]
fn token_cases() {
	for c in load().token_cases {
		let s = snip_core::stats::payload_stats(&c.text);
		let got = [s.chars, s.lines, s.words, s.tokens].map(|n| n as u64);
		assert_eq!(
			got,
			[c.chars, c.lines, c.words, c.tokens],
			"case {}",
			c.name
		);
		assert_eq!(
			snip_core::stats::estimate_tokens(&c.text) as u64,
			c.tokens,
			"case {}",
			c.name
		);
	}
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
fn restore_cases() {
	use snip_core::format::parse_clipboard;
	use snip_core::restore::plan_restore;
	use std::path::{Path, PathBuf};

	#[cfg(unix)]
	fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
		std::os::unix::fs::symlink(target, link)
	}
	#[cfg(windows)]
	fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
		std::os::windows::fs::symlink_dir(target, link)
	}

	// {root, path} of an absolute path relative to the layout base.
	fn place(parent: &Path, abs: &Path) -> (String, String) {
		let rel = abs.strip_prefix(parent).unwrap_or_else(|_| {
			panic!("{} is outside the layout", abs.display())
		});
		let mut parts =
			rel.components().map(|c| c.as_os_str().to_string_lossy());
		let root = parts.next().unwrap_or_default().into_owned();
		let rest: Vec<_> = parts.collect();
		(root, rest.join("/"))
	}

	type Planned = (
		Vec<(String, String, String, String, bool)>,
		Vec<(String, String, String)>,
		Vec<(String, Option<String>, String)>,
	);

	let fixture = load();
	let layout = &fixture.restore_layout;
	let tmp = tempfile::tempdir().unwrap();
	// Canonicalize first: macOS /var is a symlink to /private/var.
	let parent = dunce::canonicalize(tmp.path()).unwrap();
	for dir in &layout.dirs {
		std::fs::create_dir_all(parent.join(dir)).unwrap();
	}
	for (file, spec) in &layout.files {
		let bytes = match spec {
			LayoutFile::Text(t) => t.as_bytes().to_vec(),
			LayoutFile::Base64(b) => base64_decode(b),
		};
		std::fs::write(parent.join(file), bytes).unwrap();
	}
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

	let mut failures = Vec::new();
	for c in &fixture.restore_cases {
		if c.needs_symlink && !symlinks {
			println!(
				"SKIPPED {:?}: this platform refused to create a directory \
				 symlink",
				c.name
			);
			continue;
		}
		let plan = plan_restore(
			&roots,
			&parse_clipboard(&c.payload, &c.header_format),
		);
		let actual: Planned = (
			plan.create_operations
				.iter()
				.map(|op| {
					let (root, path) = place(&parent, &op.absolute_path);
					let rel = op.relative_path.clone();
					(root, path, rel, op.content.clone(), op.existed)
				})
				.collect(),
			plan.delete_operations
				.iter()
				.map(|op| {
					let (root, path) = place(&parent, &op.absolute_path);
					(root, path, op.relative_path.clone())
				})
				.collect(),
			plan.skipped_operations
				.iter()
				.map(|op| {
					let reason = op.reason.as_str().to_string();
					(op.raw_path.clone(), op.relative_path.clone(), reason)
				})
				.collect(),
		);
		let expected: Planned = (
			c.creates
				.iter()
				.map(|o| {
					let (root, path) = (o.root.clone(), o.path.clone());
					let rel = o.relative_path.clone();
					(root, path, rel, o.content.clone(), o.existed)
				})
				.collect(),
			c.deletes
				.iter()
				.map(|o| {
					(o.root.clone(), o.path.clone(), o.relative_path.clone())
				})
				.collect(),
			c.skips
				.iter()
				.map(|o| {
					(
						o.raw_path.clone(),
						o.relative_path.clone(),
						o.reason.clone(),
					)
				})
				.collect(),
		);
		if actual != expected {
			failures.push(format!(
				"{}:\n  expected {expected:?}\n  got      {actual:?}",
				c.name
			));
		}
	}
	assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Minimal standard base64 decoder for layout files (no new dependency).
fn base64_decode(s: &str) -> Vec<u8> {
	const ALPHABET: &[u8] =
		b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
	let mut out = Vec::new();
	let (mut acc, mut bits) = (0u32, 0);
	for b in s.bytes().filter(|&b| b != b'=') {
		let v = ALPHABET.iter().position(|&a| a == b).expect("base64") as u32;
		acc = (acc << 6) | v;
		bits += 6;
		if bits >= 8 {
			bits -= 8;
			out.push((acc >> bits) as u8);
		}
	}
	out
}
