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
#[ignore = "T-03"]
fn path_cases() {
	todo!()
}

#[test]
#[ignore = "T-05"]
fn restore_cases() {
	todo!()
}
