//! An untracked, never-committed file must survive copy -> clipboard text ->
//! paste into a checkout that has neither the file nor its directory.

use std::fs;
use std::path::Path;
use std::process::Command;

use snip_core::format::{
	build_payload, parse_clipboard, BuildPayloadOptions, ChangeType,
};
use snip_core::gitsrc::{collect, Git, GitSource};
use snip_core::restore::{
	execute_restore_plan, plan_restore, RestoreSelection,
};

const HEADER: &str = "// file: $FILE_PATH";

fn git(dir: &Path, args: &[&str]) {
	let out = Command::new("git")
		.args(args)
		.current_dir(dir)
		.env("GIT_CONFIG_NOSYSTEM", "1")
		.output()
		.unwrap();
	assert!(
		out.status.success(),
		"git {args:?}: {}",
		String::from_utf8_lossy(&out.stderr)
	);
}

#[test]
fn untracked_new_file_copies_and_pastes_into_empty_checkout() {
	let src = tempfile::tempdir().unwrap();
	git(src.path(), &["init", "-q", "-b", "main"]);
	git(src.path(), &["config", "user.name", "T"]);
	git(src.path(), &["config", "user.email", "t@example.com"]);
	fs::write(src.path().join("base.txt"), "base\n").unwrap();
	git(src.path(), &["add", "."]);
	git(src.path(), &["commit", "-qm", "base"]);

	// Never `git add`ed, and in a directory the destination lacks.
	fs::create_dir_all(src.path().join("brand/new dir")).unwrap();
	fs::write(src.path().join("brand/new dir/新檔.txt"), "fresh\n").unwrap();
	fs::write(src.path().join("ignored.log"), "x\n").unwrap();
	fs::write(src.path().join(".gitignore"), "*.log\n").unwrap();

	let copied =
		collect(&Git::open(src.path()).unwrap(), &GitSource::Working).unwrap();
	let paths: Vec<_> = copied.files.iter().map(|f| f.path.as_str()).collect();
	assert!(paths.contains(&"brand/new dir/新檔.txt"), "{paths:?}");
	assert!(!paths.contains(&"ignored.log"), "gitignored file copied");

	let text = build_payload(&BuildPayloadOptions {
		header_format: HEADER.into(),
		files: copied.files,
		..Default::default()
	});

	let dst = tempfile::tempdir().unwrap();
	let entries = parse_clipboard(&text, HEADER);
	let entry = entries
		.iter()
		.find(|e| e.path.ends_with("新檔.txt"))
		.expect("new file missing from clipboard text");
	assert!(entry.change_types.contains(&ChangeType::New));

	let plan = plan_restore(&[dst.path()], &entries);
	let r = execute_restore_plan(&plan, &RestoreSelection::default());
	assert_eq!(r.created_count, 2, "new file + .gitignore");
	assert_eq!(
		fs::read_to_string(dst.path().join("brand/new dir/新檔.txt")).unwrap(),
		// The clipboard format drops the final newline.
		"fresh"
	);
	assert!(!dst.path().join("ignored.log").exists());
}
