//! One test in its own binary: Git trace2's process-wide environment cannot
//! race other tests, including under cargo test (nextest also isolates tests).
//! Holding the destination lock stops apply before freshness and writes, so
//! the trace counts only the lock-discovery stage through the public API.

use std::fs;
use std::path::Path;
use std::process::Command;

use snip_core::gitrun::RunOptions;
use snip_core::gitsrc::{Git, GitError};
use snip_core::restore::RestoreSelection;
use snip_core::transfer::{
	plan_import, CanonicalRootId, ImportMapping, TransferError,
};
use snip_core::workspace::{lock_heavy, RepoIdentity};

struct TraceEnv(Option<std::ffi::OsString>);

impl TraceEnv {
	fn start(path: &Path) -> Self {
		let previous = std::env::var_os("GIT_TRACE2_EVENT");
		std::env::set_var("GIT_TRACE2_EVENT", path);
		Self(previous)
	}
}

impl Drop for TraceEnv {
	fn drop(&mut self) {
		match &self.0 {
			Some(value) => std::env::set_var("GIT_TRACE2_EVENT", value),
			None => std::env::remove_var("GIT_TRACE2_EVENT"),
		}
	}
}

#[test]
fn paste_lock_git_calls_scale_with_repositories_instead_of_directories() {
	let tmp = tempfile::tempdir().unwrap();
	let root = dunce::canonicalize(tmp.path()).unwrap();
	let out = Command::new("git")
		.args(["init", "-q", "-b", "main"])
		.arg(&root)
		.output()
		.unwrap();
	assert!(out.status.success(), "{out:?}");
	for i in 0..200 {
		fs::create_dir(root.join(format!("d{i}"))).unwrap();
	}
	let mapping =
		ImportMapping::with_primary(CanonicalRootId::new(&root).unwrap());
	let plans: Vec<_> = [1, 200]
		.into_iter()
		.map(|directories| {
			let text: String = (0..directories)
				.map(|i| format!("// file: d{i}/new.txt\ncontent\n"))
				.collect();
			plan_import(
				&text,
				"// file: $FILE_PATH",
				std::slice::from_ref(&root),
				&mapping,
			)
			.unwrap()
		})
		.collect();
	// Resolving before tracing also warms Git::open_with's --version cache.
	let id = RepoIdentity::resolve(
		&Git::open(&root).unwrap(),
		&RunOptions::default(),
	)
	.unwrap();
	let held = lock_heavy(&id).unwrap();
	let calls: Vec<_> = plans
		.iter()
		.enumerate()
		.map(|(i, plan)| {
			let trace = root.join(format!("trace-{i}.json"));
			let result = {
				let _env = TraceEnv::start(&trace);
				plan.apply(&RestoreSelection::default())
			};
			assert!(
				matches!(
					result,
					Err(TransferError::Git(GitError::WorktreeBusy { .. }))
				),
				"{result:?}"
			);
			fs::read_to_string(trace)
				.unwrap()
				.lines()
				.filter(|line| {
					let event: serde_json::Value =
						serde_json::from_str(line).unwrap();
					event["event"] == "start"
				})
				.count()
		})
		.collect();
	assert_eq!(calls[1], calls[0], "200 existing directories in one repo must cost the same as one: {calls:?}");
	// Trace2 also records Git's own subprocesses (e.g. superproject lookup).
	assert!(
		calls[0] > 0 && calls[0] <= 6,
		"one repo must have a small fixed Git process budget: {calls:?}"
	);
	eprintln!(
		"lock discovery Git processes: 1 directory={}, 200 directories={}",
		calls[0], calls[1]
	);
	for i in 0..200 {
		assert!(!root.join(format!("d{i}/new.txt")).exists());
	}
	drop(held);
	let result = plans[1].apply(&RestoreSelection::default()).unwrap();
	assert!(result.errors.is_empty(), "{:?}", result.errors);
	assert_eq!(result.created_count, 200);
}
