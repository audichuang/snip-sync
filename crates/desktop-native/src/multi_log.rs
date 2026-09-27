//! The workspace log over several repositories, like IntelliJ's log with
//! several VCS roots: one feed per repository, merged newest first while
//! each repository keeps its own (topological) order.
//!
//! Row ids in the merged log are namespaced `<sha>@<feed>`, so the same
//! commit in two clones stays two rows and the graph never joins them.

use std::collections::VecDeque;
use std::path::PathBuf;

use snip_core::browser::CommitSummary;

/// Commits one feed read brings in.
pub const FEED_PAGE: usize = 50;

/// `<sha>@<feed>` → (`sha`, feed). A plain SHA has no feed.
pub fn split_id(id: &str) -> (&str, Option<usize>) {
	match id.rsplit_once('@') {
		Some((sha, feed)) => match feed.parse() {
			Ok(feed) => (sha, Some(feed)),
			Err(_) => (id, None),
		},
		None => (id, None),
	}
}

pub fn ns_id(sha: &str, feed: usize) -> String {
	format!("{sha}@{feed}")
}

/// A commit's author date in seconds, for ordering; unreadable dates sort
/// last.
fn stamp(commit: &CommitSummary) -> i64 {
	chrono::DateTime::parse_from_rfc3339(&commit.author_date)
		.map(|d| d.timestamp())
		.unwrap_or(i64::MIN)
}

/// One repository of the merged log.
#[derive(Clone, Debug, Default)]
pub struct Feed {
	pub root: PathBuf,
	pub name: String,
	/// Paths the Paths chip picked inside this repository (repo-relative).
	pub paths: Vec<String>,
	/// Read, not yet merged commits (namespaced), in the repository's order.
	pub pending: VecDeque<CommitSummary>,
	/// Its HEAD, namespaced (its refs live in the model's ref list).
	pub head: Option<String>,
	/// The graph tips read with the first page; later pages walk these.
	pub tips: Vec<String>,
	/// Commits read so far (the next read's `--skip`).
	pub skip: usize,
	pub loaded: bool,
	pub more: bool,
	/// A failed read ends the feed; the other repositories go on.
	pub failed: bool,
	/// Date of the last commit read: how far down this feed is known.
	pub frontier: i64,
}

impl Feed {
	pub fn new(root: PathBuf, name: String, paths: Vec<String>) -> Self {
		Self {
			root,
			name,
			paths,
			frontier: i64::MAX,
			..Default::default()
		}
	}

	/// Can still produce commits nobody has read.
	pub fn can_read(&self) -> bool {
		!self.failed && (!self.loaded || self.more)
	}

	/// Takes one read page (already namespaced).
	pub fn push_page(&mut self, commits: Vec<CommitSummary>, more: bool) {
		self.skip += commits.len();
		if let Some(last) = commits.last() {
			self.frontier = stamp(last);
		}
		self.pending.extend(commits);
		self.loaded = true;
		self.more = more;
	}

	pub fn retained_bytes(&self) -> usize {
		use crate::graph_view::vec_bytes;
		let mut bytes = self.root.as_os_str().len()
			+ self.name.capacity()
			+ vec_bytes(&self.paths)
			+ vec_bytes(&self.tips)
			+ self.pending.capacity() * std::mem::size_of::<CommitSummary>();
		for s in self.paths.iter().chain(&self.tips).chain(&self.head) {
			bytes += s.capacity();
		}
		for c in &self.pending {
			bytes += crate::history::commit_bytes(c);
		}
		bytes
	}
}

/// Which feed each next merged row comes from, newest first, at most
/// `limit` rows. It stops where a feed that may still have commits has
/// none read: its next commit could be newer than any read one.
pub fn merge_order(feeds: &[Feed], limit: usize) -> Vec<usize> {
	let mut taken = vec![0usize; feeds.len()];
	let mut out = Vec::new();
	while out.len() < limit {
		let mut best: Option<(usize, i64)> = None;
		for (i, feed) in feeds.iter().enumerate() {
			match feed.pending.get(taken[i]) {
				Some(c) => {
					let t = stamp(c);
					if best.is_none_or(|(_, b)| t > b) {
						best = Some((i, t));
					}
				}
				None if feed.can_read() => return out,
				None => {}
			}
		}
		let Some((i, _)) = best else {
			break;
		};
		taken[i] += 1;
		out.push(i);
	}
	out
}

/// The feed to read next: one whose read commits are all merged although
/// it has more, the one known least far down first (it gates the merge).
pub fn next_read(feeds: &[Feed]) -> Option<usize> {
	feeds
		.iter()
		.enumerate()
		.filter(|(_, f)| f.pending.is_empty() && f.can_read())
		.max_by(|(ia, a), (ib, b)| a.frontier.cmp(&b.frontier).then(ib.cmp(ia)))
		.map(|(i, _)| i)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn commit(sha: &str, date: &str) -> CommitSummary {
		CommitSummary {
			sha: sha.into(),
			parents: Vec::new(),
			author_name: "a".into(),
			author_email: "a@x".into(),
			author_date: date.into(),
			subject: sha.into(),
		}
	}

	fn feed(commits: &[(&str, &str)], more: bool) -> Feed {
		let mut f = Feed::new(PathBuf::from("/r"), "r".into(), Vec::new());
		f.push_page(commits.iter().map(|(s, d)| commit(s, d)).collect(), more);
		f
	}

	fn rows(feeds: &[Feed], limit: usize) -> Vec<String> {
		let mut taken = vec![0; feeds.len()];
		merge_order(feeds, limit)
			.into_iter()
			.map(|i| {
				taken[i] += 1;
				feeds[i].pending[taken[i] - 1].sha.clone()
			})
			.collect()
	}

	#[test]
	fn merges_newest_first_and_keeps_each_repository_order() {
		// b2 is older than b3 but comes first in b's topological order; the
		// offsets differ, so string order would be wrong.
		let a = feed(
			&[
				("a1", "2026-01-05T10:00:00+00:00"),
				("a2", "2026-01-03T10:00:00+00:00"),
			],
			false,
		);
		let b = feed(
			&[
				("b1", "2026-01-06T01:00:00+08:00"), // 01-05 17:00 UTC
				("b2", "2026-01-01T00:00:00+00:00"),
				("b3", "2026-01-04T00:00:00+00:00"),
			],
			false,
		);
		assert_eq!(rows(&[a, b], 10), ["b1", "a1", "a2", "b2", "b3"]);
	}

	#[test]
	fn stops_at_a_feed_that_may_have_newer_commits() {
		let a = feed(&[("a1", "2026-01-05T00:00:00Z")], true);
		let b = feed(
			&[
				("b1", "2026-01-06T00:00:00Z"),
				("b2", "2026-01-04T00:00:00Z"),
			],
			false,
		);
		// a1 merges; a's next commit could be newer than b2.
		assert_eq!(rows(&[a.clone(), b.clone()], 10), ["b1", "a1"]);
		assert_eq!(rows(&[a, b], 1), ["b1"]);
		// An unread feed gates everything.
		let unread = Feed::new(PathBuf::from("/u"), "u".into(), Vec::new());
		assert!(rows(
			&[unread, feed(&[("c", "2026-01-01T00:00:00Z")], false)],
			5
		)
		.is_empty());
	}

	#[test]
	fn reads_the_gating_feed_one_at_a_time() {
		let mut a = feed(&[("a1", "2026-01-05T00:00:00Z")], true);
		let mut b = feed(&[("b1", "2026-01-07T00:00:00Z")], true);
		let done = feed(&[], false);
		// Nothing merged yet: nobody's pending ran out.
		assert_eq!(next_read(&[a.clone(), b.clone(), done.clone()]), None);
		a.pending.clear();
		b.pending.clear();
		// Both ran out; b is known only down to a newer date, so it gates.
		assert_eq!(next_read(&[a.clone(), b.clone(), done.clone()]), Some(1));
		// A feed that failed or ended is never read again.
		b.failed = true;
		assert_eq!(next_read(&[a.clone(), b, done.clone()]), Some(0));
		// Unread feeds first, in order.
		let u = |n: &str| Feed::new(PathBuf::from(n), n.into(), Vec::new());
		assert_eq!(next_read(&[a, u("x"), u("y"), done]), Some(1));
	}

	#[test]
	fn ids_round_trip() {
		assert_eq!(split_id(&ns_id("abc", 3)), ("abc", Some(3)));
		assert_eq!(split_id("abc"), ("abc", None));
	}
}
