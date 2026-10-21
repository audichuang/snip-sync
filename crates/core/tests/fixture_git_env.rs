//! #230: a fixture repo made with a bare `git init` must still have git's
//! automatic maintenance off. `.cargo/config.toml` sets it for every test
//! process through `GIT_CONFIG_COUNT`; this fails when that stops reaching
//! tests (a changed runner, a cleared environment), instead of letting a
//! background `git maintenance` pack a fixture's loose objects under load.

use std::process::Command;

fn git(dir: &std::path::Path, args: &[&str]) -> String {
	let out = Command::new("git")
		.args(args)
		.current_dir(dir)
		.output()
		.unwrap();
	assert!(out.status.success(), "git {args:?}: {out:?}");
	String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn a_bare_git_init_has_automatic_maintenance_off() {
	let tmp = tempfile::tempdir().unwrap();
	git(tmp.path(), &["init", "-q"]);
	assert_eq!(git(tmp.path(), &["config", "--get", "gc.auto"]), "0");
	assert_eq!(
		git(tmp.path(), &["config", "--get", "maintenance.auto"]),
		"false"
	);
}
