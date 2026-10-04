//! Reaching a worker over ssh: the command that starts it, and the hosts
//! the user's `~/.ssh/config` names.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

/// Replaces the whole worker command line, split on whitespace: tests run
/// a built `snip serve --stdio` directly, and acceptance runs the build
/// under test on the second machine (`ssh host /tmp/…/snip serve --stdio`).
pub const EXEC_ENV: &str = "SNIP_REMOTE_EXEC";

/// Where `snip` may be on the far end when a non-interactive shell's PATH
/// lacks it. Exit 127 means none was found.
const REMOTE_SCRIPT: &str = "for p in snip ~/.local/bin/snip /opt/homebrew/bin/snip \
	/home/linuxbrew/.linuxbrew/bin/snip /usr/local/bin/snip; do \
	command -v \"$p\" >/dev/null 2>&1 && exec \"$p\" serve --stdio; done; exit 127";

/// The command that starts a worker on `host`: ssh without a terminal or
/// password prompts, running `snip serve --stdio` there.
pub fn command(host: &str) -> Vec<OsString> {
	if let Some(exec) = std::env::var_os(EXEC_ENV).filter(|v| !v.is_empty()) {
		return exec
			.to_string_lossy()
			.split_whitespace()
			.map(OsString::from)
			.collect();
	}
	[
		"ssh",
		"-T",
		"-o",
		"BatchMode=yes",
		"-o",
		"ConnectTimeout=10",
		"--",
		host,
		&format!("sh -c '{REMOTE_SCRIPT}'"),
	]
	.into_iter()
	.map(OsString::from)
	.collect()
}

/// The concrete host names in `~/.ssh/config` and the files it includes,
/// in file order, without wildcard patterns or repeats.
pub fn config_hosts() -> Vec<String> {
	let Some(home) = crate::worker::home_dir() else {
		return Vec::new();
	};
	let ssh_dir = home.join(".ssh");
	let mut hosts = Vec::new();
	let mut seen_files = Vec::new();
	read_hosts(&ssh_dir.join("config"), &ssh_dir, &mut hosts, &mut seen_files);
	hosts
}

fn read_hosts(
	file: &Path,
	ssh_dir: &Path,
	hosts: &mut Vec<String>,
	seen_files: &mut Vec<PathBuf>,
) {
	// An include cycle, or a file listed twice, is read once.
	if seen_files.iter().any(|f| f == file) || seen_files.len() > 64 {
		return;
	}
	seen_files.push(file.to_path_buf());
	let Ok(text) = fs::read_to_string(file) else {
		return;
	};
	for line in text.lines() {
		let line = line.trim();
		if line.starts_with('#') {
			continue;
		}
		let mut words = line
			.splitn(2, |c: char| c.is_whitespace() || c == '=')
			.map(str::trim);
		let (Some(key), Some(value)) = (words.next(), words.next()) else {
			continue;
		};
		if key.eq_ignore_ascii_case("host") {
			for name in value.split_whitespace() {
				let name = name.trim_matches('"');
				let pattern = name.contains(['*', '?', '!']);
				if !pattern && !hosts.iter().any(|h| h == name) {
					hosts.push(name.to_string());
				}
			}
		} else if key.eq_ignore_ascii_case("include") {
			for spelled in value.split_whitespace() {
				for path in include_paths(spelled.trim_matches('"'), ssh_dir) {
					read_hosts(&path, ssh_dir, hosts, seen_files);
				}
			}
		}
	}
}

/// An `Include` argument: `~` expanded, relative to `~/.ssh`, and a `*` or
/// `?` in the last component matched against that folder.
fn include_paths(spelled: &str, ssh_dir: &Path) -> Vec<PathBuf> {
	let expanded = match spelled.strip_prefix("~/") {
		Some(rest) => match crate::worker::home_dir() {
			Some(home) => home.join(rest),
			None => return Vec::new(),
		},
		None => PathBuf::from(spelled),
	};
	let path = if expanded.is_absolute() {
		expanded
	} else {
		ssh_dir.join(expanded)
	};
	let name = path
		.file_name()
		.map(|n| n.to_string_lossy().into_owned())
		.unwrap_or_default();
	if !name.contains(['*', '?']) {
		return vec![path];
	}
	let Some(dir) = path.parent() else {
		return Vec::new();
	};
	let mut found: Vec<PathBuf> = fs::read_dir(dir)
		.into_iter()
		.flatten()
		.flatten()
		.filter(|e| wildcard(&name, &e.file_name().to_string_lossy()))
		.map(|e| e.path())
		.collect();
	found.sort();
	found
}

/// Shell-style `*` and `?` matching.
fn wildcard(pattern: &str, text: &str) -> bool {
	let (p, t): (Vec<char>, Vec<char>) =
		(pattern.chars().collect(), text.chars().collect());
	let (mut pi, mut ti, mut star, mut mark) = (0, 0, None, 0);
	while ti < t.len() {
		if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
			pi += 1;
			ti += 1;
		} else if pi < p.len() && p[pi] == '*' {
			star = Some(pi);
			mark = ti;
			pi += 1;
		} else if let Some(s) = star {
			pi = s + 1;
			mark += 1;
			ti = mark;
		} else {
			return false;
		}
	}
	p[pi..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn hosts_follow_includes_and_skip_patterns() {
		let dir = tempfile::tempdir().unwrap();
		let ssh = dir.path();
		fs::create_dir(ssh.join("conf.d")).unwrap();
		fs::write(
			ssh.join("config"),
			"# comment\nInclude conf.d/*.conf\nHost ubuntu mini\n  HostName 10.0.0.1\nHost *\n  User x\nHost=laptop\nHost !bad *.corp db?\nInclude config\n",
		)
		.unwrap();
		fs::write(ssh.join("conf.d/a.conf"), "Host vm-a\n").unwrap();
		fs::write(ssh.join("conf.d/b.conf"), "Host vm-b ubuntu\n").unwrap();
		fs::write(ssh.join("conf.d/notes.txt"), "Host skipped\n").unwrap();
		let mut hosts = Vec::new();
		read_hosts(&ssh.join("config"), ssh, &mut hosts, &mut Vec::new());
		assert_eq!(hosts, ["vm-a", "vm-b", "ubuntu", "mini", "laptop"]);
	}

	#[test]
	fn wildcard_matches_like_a_shell() {
		assert!(wildcard("*.conf", "a.conf"));
		assert!(wildcard("db?", "db1"));
		assert!(!wildcard("db?", "db12"));
		assert!(wildcard("*", ""));
		assert!(!wildcard("*.conf", "a.txt"));
	}

	#[test]
	fn the_ssh_command_never_prompts_and_runs_serve() {
		if std::env::var_os(EXEC_ENV).is_some() {
			return;
		}
		let argv: Vec<String> = command("ubuntu")
			.into_iter()
			.map(|a| a.to_string_lossy().into_owned())
			.collect();
		assert_eq!(argv[0], "ssh");
		assert!(argv.iter().any(|a| a == "BatchMode=yes"));
		let host = argv.iter().position(|a| a == "ubuntu").unwrap();
		assert_eq!(argv[host - 1], "--");
		assert!(argv[host + 1].contains("serve --stdio"));
	}
}
