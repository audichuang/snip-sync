//! Git view request handling on the worker.

use std::path::Path;
use std::time::{Duration, Instant};

use snip_core::gitrun::{CancelToken, GitPool};
use snip_core::gitview::{ReadProfile, StatusSummary, SERVED_MAX_STDOUT};
use snip_core::workspace::ScanBudget;

use crate::proto::{
	valid_rel_path, ErrorCode, RepoScan, Response, ScannedRepo,
};
use crate::worker::{io_error, SharedRoot};

/// Answers a ScanRepos request within the shared root.
pub(crate) fn scan(
	root: &SharedRoot,
	under: Option<&str>,
	cancel: &CancelToken,
	scan_deadline: Duration,
) -> Response {
	let under_dir = match under {
		None | Some("") => None,
		Some(u) => {
			if !valid_rel_path(u, false) {
				return Response::Error {
					code: ErrorCode::BadRequest,
					message: "invalid folder".into(),
				};
			}
			let resolved = match snip_core::browser::inside(&root.path, u) {
				Ok(p) => p,
				Err(err) => return io_error(err),
			};
			if !resolved.is_dir() {
				return Response::Error {
					code: ErrorCode::NotFound,
					message: "not a directory".into(),
				};
			}
			Some(resolved)
		}
	};

	let mut opts = ReadProfile::Interactive.options(Some(cancel.clone()));
	opts.pool = GitPool::Served;
	opts.max_stdout = opts.max_stdout.min(SERVED_MAX_STDOUT);

	// Reserve 5s for network write / cleanup when scan_deadline > 6s;
	// for short test deadlines, clamp to half the deadline to avoid underflow.
	let reserve = Duration::from_secs(5);
	let budget_duration = if scan_deadline > Duration::from_secs(6) {
		scan_deadline.saturating_sub(reserve)
	} else {
		scan_deadline / 2
	};

	let budget = ScanBudget {
		max_visited: 200_000,
		deadline: Some(Instant::now() + budget_duration),
		cancel: Some(cancel.clone()),
	};

	let ws_scan = snip_core::gitview::scan_repos_within(
		&root.path,
		under_dir.as_deref(),
		8,
		256,
		&budget,
		&opts,
	);

	let canonical_share =
		dunce::canonicalize(&root.path).unwrap_or_else(|_| root.path.clone());

	let mut repos = Vec::with_capacity(ws_scan.repos.len());
	let mut errors = Vec::with_capacity(ws_scan.errors.len());

	for r in ws_scan.repos {
		let r_root = dunce::canonicalize(&r.root).unwrap_or(r.root);
		match r_root.strip_prefix(&canonical_share) {
			Ok(rel_path) => {
				let utf8 = rel_path.to_str().is_some();
				let rel = if rel_path.as_os_str().is_empty() {
					"".to_string()
				} else {
					rel_path
						.components()
						.map(|c| c.as_os_str().to_string_lossy())
						.collect::<Vec<_>>()
						.join("/")
				};
				let summary = match r.summary {
					Ok(s) => Ok(StatusSummary {
						head: s.head,
						branch: s.branch,
						changes: s.changes,
					}),
					Err(msg) => Err(scrub(&msg, &root.path)),
				};
				repos.push(ScannedRepo {
					rel,
					utf8,
					name: r.name,
					kind: r.kind,
					summary,
				});
			}
			Err(_) => {
				// Repo root is not under the share root: drop it and add an error row
				errors.push((
					".".to_string(),
					scrub(
						"repository root is outside the shared folder",
						&root.path,
					),
				));
			}
		}
	}

	for (err_path, err_msg) in ws_scan.errors {
		let canon_path = dunce::canonicalize(&err_path).unwrap_or(err_path);
		let rel = match canon_path.strip_prefix(&canonical_share) {
			Ok(rel_path) => {
				if rel_path.as_os_str().is_empty() {
					".".to_string()
				} else {
					rel_path
						.components()
						.map(|c| c.as_os_str().to_string_lossy())
						.collect::<Vec<_>>()
						.join("/")
				}
			}
			Err(_) => ".".to_string(),
		};
		errors.push((rel, scrub(&err_msg, &root.path)));
	}

	let depth_limited: Vec<String> = ws_scan
		.depth_limited
		.into_iter()
		.map(|p| {
			let canon = dunce::canonicalize(&p).unwrap_or(p);
			match canon.strip_prefix(&canonical_share) {
				Ok(rel_path) => {
					if rel_path.as_os_str().is_empty() {
						".".to_string()
					} else {
						rel_path
							.components()
							.map(|c| c.as_os_str().to_string_lossy())
							.collect::<Vec<_>>()
							.join("/")
					}
				}
				Err(_) => ".".to_string(),
			}
		})
		.collect();

	Response::Repos(RepoScan {
		repos,
		errors,
		error_overflow: ws_scan.error_overflow,
		depth_limited,
		depth_overflow: ws_scan.depth_overflow,
		status: ws_scan.status,
	})
}

/// Replaces the share's absolute path (both raw and canonical) with share-relative
/// form, and replaces any remaining whitespace/quote delimited absolute path tokens
/// with `<outside the share>`.
pub(crate) fn scrub(msg: &str, share: &Path) -> String {
	let share_str = share.to_string_lossy();
	let share_canon = dunce::canonicalize(share)
		.map(|p| p.to_string_lossy().into_owned())
		.unwrap_or_else(|_| share_str.clone().into_owned());

	let mut candidates = Vec::new();
	let s1 = share_canon.trim_end_matches(['/', '\\']).to_string();
	if !s1.is_empty() {
		candidates.push(s1);
	}
	let s2 = share_str.trim_end_matches(['/', '\\']).to_string();
	if !s2.is_empty() && !candidates.contains(&s2) {
		candidates.push(s2);
	}
	candidates.sort_by_key(|a| std::cmp::Reverse(a.len()));

	let mut result = msg.to_string();
	for c in candidates {
		result = result.replace(&format!("{c}/"), "");
		result = result.replace(&format!("{c}\\"), "");
		result = result.replace(&c, ".");
	}

	scrub_outside_paths(&result)
}

fn is_delim(c: char) -> bool {
	c.is_whitespace() || c == '"' || c == '\'' || c == '`'
}

fn is_absolute_path(s: &str) -> bool {
	if s.starts_with('/') {
		return true;
	}
	let bytes = s.as_bytes();
	if bytes.len() >= 3
		&& bytes[0].is_ascii_alphabetic()
		&& bytes[1] == b':'
		&& (bytes[2] == b'\\' || bytes[2] == b'/')
	{
		return true;
	}
	false
}

fn scrub_outside_paths(s: &str) -> String {
	let mut out = String::with_capacity(s.len());
	let mut chars = s.char_indices().peekable();

	while let Some(&(idx, ch)) = chars.peek() {
		if is_delim(ch) {
			out.push(ch);
			chars.next();
		} else {
			let start = idx;
			let mut end = idx + ch.len_utf8();
			chars.next();
			while let Some(&(next_idx, next_ch)) = chars.peek() {
				if is_delim(next_ch) {
					break;
				}
				end = next_idx + next_ch.len_utf8();
				chars.next();
			}
			let token = &s[start..end];
			let trimmed = token.trim_end_matches(|c| {
				matches!(c, ':' | ',' | '.' | ';' | ')' | ']' | '}')
			});
			if is_absolute_path(trimmed) {
				out.push_str("<outside the share>");
				out.push_str(&token[trimmed.len()..]);
			} else {
				out.push_str(token);
			}
		}
	}
	out
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::path::Path;

	#[test]
	fn scrub_inside_path() {
		let share = Path::new("/var/workspace");
		let msg = "failed at /var/workspace/subdir/file.txt";
		assert_eq!(scrub(msg, share), "failed at subdir/file.txt");

		let bare = "root at /var/workspace";
		assert_eq!(scrub(bare, share), "root at .");
	}

	#[test]
	fn scrub_outside_path() {
		let share = Path::new("/var/workspace");
		let msg = "repository at /opt/secret/repo is outside";
		assert_eq!(
			scrub(msg, share),
			"repository at <outside the share> is outside"
		);
	}

	#[test]
	fn scrub_quoted_path() {
		let share = Path::new("/var/workspace");
		let msg = "cannot open '/opt/secret/repo': not found";
		assert_eq!(
			scrub(msg, share),
			"cannot open '<outside the share>': not found"
		);

		let dquote = "cannot open \"/opt/secret/repo\"";
		assert_eq!(scrub(dquote, share), "cannot open \"<outside the share>\"");
	}

	#[test]
	fn scrub_no_path() {
		let share = Path::new("/var/workspace");
		let msg = "operation failed with permission denied";
		assert_eq!(
			scrub(msg, share),
			"operation failed with permission denied"
		);
	}

	#[test]
	fn scrub_windows_looking_path() {
		let share = Path::new("C:/Workspace");
		let msg1 = r"failed at D:\Secret\repo\config: invalid";
		assert_eq!(
			scrub(msg1, share),
			"failed at <outside the share>: invalid"
		);

		let msg2 = "failed at D:/Secret/repo/config";
		assert_eq!(scrub(msg2, share), "failed at <outside the share>");
	}
}
