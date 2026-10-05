//! Commit mode: serialize and replay a range of commits (spec section 4).
//!
//! snip-sync only; the IDE plugins have no counterpart. Selection follows
//! first parents, each commit carries its diff against its first parent, and
//! replay writes the files then commits ONLY those paths, so anything the
//! user had staged stays out of the new commits.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::blob::{BlobRead, BlobReader, NotText};
use crate::fsutil::{must_not_overwrite, write_text_file};
use crate::gitrun::{CancelToken, RunOptions};
use crate::gitsrc::{Git, GitError, RawZ, EMPTY_TREE};
use crate::paths::{escapes_all_roots, lands_in_git_dir, resolve_write_target};

/// Replay replaces a symlink at the target instead of writing through it,
/// so only the folder it lands in decides whether it is inside Git.
/// True when a write or delete at `abs` leaves the replay's write `scope`
/// (the folder the user pasted into, at most the repository root), or
/// lands in a Git directory, or where that cannot be established.
fn unsafe_replay_target(scope: &Path, abs: &Path) -> bool {
	escapes_all_roots(&[scope], abs)
		|| abs.parent().is_none_or(lands_in_git_dir)
}
use crate::workspace::{lock_heavy, RepoIdentity};

/// First line of a commit-mode payload; the rest is JSON.
pub use crate::clip::COMMIT_MARKER;

#[derive(Debug, thiserror::Error)]
pub enum CommitError {
	#[error(transparent)]
	Git(#[from] GitError),
	#[error("no commits selected")]
	Empty,
	#[error("requested {requested} commits, but HEAD has only {available} along its first parents")]
	NotEnoughCommits { requested: usize, available: usize },
	#[error("commits are not contiguous: following first parents back from {tip}, {at} has {}, so {base} is never reached", first_parent.as_deref().map_or("no parent".to_string(), |p| format!("first parent {p}")))]
	Discontinuous {
		base: String,
		tip: String,
		/// The oldest commit of the chain that is not in `base`'s history.
		at: String,
		first_parent: Option<String>,
	},
	#[error("not a snip-sync commits payload")]
	NotCommitPayload,
	#[error("invalid commits payload: {0}")]
	InvalidPayload(String),
	/// The replay's write scope is the folder the user pasted into; a
	/// target outside it is refused, and replaying the whole repository
	/// means opening the repository root.
	#[error("{path} lies outside the opened folder ({scope}); open the repository root to replay the whole repository")]
	OutsideScope { path: String, scope: PathBuf },
	/// The clipboard document (marker, newline, JSON) would exceed the cap.
	/// `actual` is the size already measured, or a lower bound when a blob
	/// was refused from its header before the body was kept.
	#[error(
		"commit export exceeds {limit} serialized bytes (at least {actual})"
	)]
	PayloadLimit { limit: usize, actual: usize },
}

/// Clipboard document produced by [`copy_commits_with`].
///
/// `text` is exactly [`to_clipboard_text`] of `payload`, counted by the same
/// serde JSON encoder. Native code can put `text` on the clipboard and use
/// `payload` for [`copy_summary`] without serializing a second time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitExport {
	pub payload: CommitsPayload,
	pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FileChange {
	Added,
	Modified,
	Deleted,
	/// `old_path` holds the source path.
	Renamed,
}

/// Why a file is listed without content. Replay neither writes nor deletes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum NotCopiedReason {
	Binary,
	NonUtf8,
	/// The path itself is not UTF-8; `path` is a lossy rendering.
	NonUtf8Path,
	/// A symlink or submodule: not a regular text file.
	UnsupportedType,
	Unreadable,
}

impl From<NotText> for NotCopiedReason {
	fn from(r: NotText) -> Self {
		match r {
			NotText::Binary => Self::Binary,
			NotText::NotUtf8 => Self::NonUtf8,
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitFile {
	pub path: String,
	pub old_path: Option<String>,
	pub change: FileChange,
	/// Full content after the change; `None` for deletions and not-copied.
	pub content: Option<String>,
	pub not_copied: Option<NotCopiedReason>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitRecord {
	/// Raw message (`%B`), replayed verbatim.
	pub message: String,
	pub author_name: String,
	pub author_email: String,
	/// Strict ISO 8601 with the original offset (`%aI`).
	pub author_date: String,
	pub files: Vec<CommitFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitsPayload {
	/// Oldest first, the order they are replayed in.
	pub commits: Vec<CommitRecord>,
}

impl CommitsPayload {
	/// Owned buffer capacities, excluding this inline struct and allocator
	/// bookkeeping. Wire fields and serialization are unchanged.
	pub fn retained_heap_bytes(&self) -> usize {
		let mut bytes =
			self.commits.capacity() * std::mem::size_of::<CommitRecord>();
		for commit in &self.commits {
			bytes = bytes
				.saturating_add(commit.message.capacity())
				.saturating_add(commit.author_name.capacity())
				.saturating_add(commit.author_email.capacity())
				.saturating_add(commit.author_date.capacity())
				.saturating_add(
					commit.files.capacity() * std::mem::size_of::<CommitFile>(),
				);
			for file in &commit.files {
				bytes = bytes
					.saturating_add(file.path.capacity())
					.saturating_add(
						file.old_path.as_ref().map_or(0, String::capacity),
					)
					.saturating_add(
						file.content.as_ref().map_or(0, String::capacity),
					);
			}
		}
		bytes
	}
}

fn contiguous_chain(
	git: &Git,
	rev_list_out: &[u8],
) -> Result<Vec<(String, Option<String>)>, CommitError> {
	// `rev-list --parents` lines: `<sha> [<first parent> ...]`, newest first.
	let chain: Vec<(String, Option<String>)> =
		String::from_utf8_lossy(rev_list_out)
			.lines()
			.filter_map(|l| {
				let mut it = l.split(' ');
				let sha = it.next().filter(|s| !s.is_empty())?;
				Some((sha.to_string(), it.next().map(str::to_string)))
			})
			.collect();
	if let Some((oldest, None)) = chain.last() {
		// A shallow boundary looks parentless; it is not a real root.
		if git.is_shallow()? {
			return Err(GitError::Shallow(oldest.clone()).into());
		}
	}
	Ok(chain)
}

/// The commits of `base..tip` (base excluded), oldest first. Every commit
/// must lie on `tip`'s first-parent chain down to `base`.
pub fn select_range(
	git: &Git,
	base: &str,
	tip: &str,
) -> Result<Vec<String>, CommitError> {
	let base = git.resolve_commit(base)?;
	let tip = git.resolve_commit(tip)?;
	let range = format!("{base}..{tip}");
	let out = git.run(&["rev-list", "--first-parent", "--parents", &range])?;
	let chain = contiguous_chain(git, &out)?;
	let Some((oldest, first_parent)) = chain.last() else {
		return Err(CommitError::Empty);
	};
	// `base..tip` drops everything reachable from base, so the chain is
	// contiguous exactly when its oldest commit sits directly on base.
	if first_parent.as_deref() != Some(base.as_str()) {
		return Err(CommitError::Discontinuous {
			at: oldest.clone(),
			first_parent: first_parent.clone(),
			base,
			tip,
		});
	}
	Ok(chain.into_iter().rev().map(|(sha, _)| sha).collect())
}

/// The last `n` commits on HEAD's first-parent chain, oldest first.
pub fn select_last(git: &Git, n: usize) -> Result<Vec<String>, CommitError> {
	select_last_from(git, "HEAD", n)
}

/// Select from an explicit tip, including a root on a non-current branch.
pub fn select_last_from(
	git: &Git,
	tip: &str,
	n: usize,
) -> Result<Vec<String>, CommitError> {
	if n == 0 {
		return Err(CommitError::Empty);
	}
	let tip = git.resolve_commit(tip)?;
	let count = n.to_string();
	let out = git.run(&[
		"rev-list",
		"--first-parent",
		"--parents",
		"-n",
		&count,
		&tip,
	])?;
	let chain = contiguous_chain(git, &out)?;
	if chain.len() < n {
		return Err(CommitError::NotEnoughCommits {
			requested: n,
			available: chain.len(),
		});
	}
	Ok(chain.into_iter().rev().map(|(sha, _)| sha).collect())
}

fn is_special_mode(mode: &str) -> bool {
	// 120000 = symlink, 160000 = submodule (gitlink).
	mode == "120000" || mode == "160000"
}

fn lossy(bytes: &[u8]) -> String {
	String::from_utf8_lossy(bytes).into_owned()
}

/// Reads `shas` (oldest first, as the selectors return them).
///
/// Legacy entry point: [`RunOptions::default`] and no total document cap.
/// A UI that must cancel or bound the clipboard uses [`copy_commits_with`].
/// Both share one reader. This wrapper does not apply the strict cap.
pub fn copy_commits(
	git: &Git,
	shas: &[String],
) -> Result<CommitsPayload, CommitError> {
	let (payload, _) =
		export_commits_counted(git, shas, &RunOptions::default(), None)?;
	Ok(payload)
}

/// Strict export of already-resolved commits for the native workbench.
///
/// `shas` is oldest first, the order [`select_last`] and [`select_range`]
/// return. Selection itself stays with the caller. `opts` is passed into
/// every Git process this call starts: commit metadata, the parent and
/// shallow checks, `diff-tree`, each `cat-file --batch` read, and closing
/// that session. Cancel kills and reaps that process tree; the budget slot
/// is released only after cleanup.
///
/// `max_serialized_bytes` counts the whole clipboard document: the commit
/// marker, the newline after it, and the JSON [`to_clipboard_text`] emits
/// (keys, escapes, commas, nulls). It is not [`RunOptions::max_stdout`].
/// A single Git command can still fail with [`GitError::OutputLimit`] when
/// its own stdout cap is hit, including when `opts.overflow` is
/// [`crate::gitrun::Overflow::Truncate`] — a short read is not a commit.
///
/// Admission walks one commit, then one diff record. A blob body is stored
/// only when its raw size still fits in the bytes left for that file's
/// JSON; a larger body is classified by a streaming scan and discarded.
/// Valid text that cannot fit fails the whole export with
/// [`CommitError::PayloadLimit`] (`actual` is then a lower bound: the
/// document size if those bytes were inserted without JSON escapes).
/// Binary, non-UTF-8, unreadable, symlink, and submodule files keep
/// [`NotCopiedReason`] and omit content, so their raw size is not itself
/// the budget. Nothing is truncated or dropped just to finish under the cap.
///
/// The returned [`CommitExport::text`] is `to_clipboard_text(&payload)` and
/// its length is at most `max_serialized_bytes`. Replay and
/// [`copy_summary`] keep using `payload`. Wire shape is unchanged.
///
/// ```ignore
/// let export = copy_commits_with(&git, &oids, &opts, max_bytes)?;
/// clipboard.write(&export.text);
/// let summary = copy_summary(&export.payload, &export.text);
/// ```
pub fn copy_commits_with(
	git: &Git,
	shas: &[String],
	opts: &RunOptions,
	max_serialized_bytes: usize,
) -> Result<CommitExport, CommitError> {
	let (payload, total_wire_len) =
		export_commits_counted(git, shas, opts, Some(max_serialized_bytes))?;
	let text = to_clipboard_text_bounded(
		&payload,
		total_wire_len,
		max_serialized_bytes,
		opts.cancel.as_ref(),
	)?;
	debug_assert_eq!(text.len(), total_wire_len);
	Ok(CommitExport { payload, text })
}

const EMPTY_PAYLOAD_WIRE_LEN: usize = COMMIT_MARKER.len() + 1 + 14;

/// Lower bound on the clipboard document for `count` commits.
///
/// An empty record is the smallest [`CommitRecord`] serde emits. Real
/// commits are at least this long, so a bound above `limit` cannot fit.
/// This is one serialization of that empty record, not a growing prefix.
pub(crate) fn min_commit_document_len(count: usize) -> usize {
	let record = empty_commit_record_json_len();
	if count == 0 {
		EMPTY_PAYLOAD_WIRE_LEN
	} else {
		EMPTY_PAYLOAD_WIRE_LEN + count * record + (count - 1)
	}
}

fn empty_commit_record_json_len() -> usize {
	static LEN: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
	*LEN.get_or_init(|| {
		let meta = CommitRecordMetaView {
			message: "",
			author_name: "",
			author_email: "",
			author_date: "",
			files: &[],
		};
		serde_json::to_vec(&meta)
			.expect("empty commit record serializes")
			.len()
	})
}

fn export_commits_counted(
	git: &Git,
	shas: &[String],
	opts: &RunOptions,
	limit: Option<usize>,
) -> Result<(CommitsPayload, usize), CommitError> {
	refuse_if_cancelled(opts, "commits")?;
	let empty_wire_len = EMPTY_PAYLOAD_WIRE_LEN;
	if let Some(max) = limit {
		if empty_wire_len > max {
			return Err(CommitError::PayloadLimit {
				limit: max,
				actual: empty_wire_len,
			});
		}
	}
	let mut commits = Vec::new();
	let mut current_wire_len = empty_wire_len;
	for sha in shas {
		refuse_if_cancelled(opts, "commits")?;
		export_one(git, sha, opts, limit, &mut commits, &mut current_wire_len)?;
	}
	refuse_if_cancelled(opts, "commits")?;
	Ok((CommitsPayload { commits }, current_wire_len))
}

fn export_one(
	git: &Git,
	sha: &str,
	opts: &RunOptions,
	limit: Option<usize>,
	commits: &mut Vec<CommitRecord>,
	current_wire_len: &mut usize,
) -> Result<(), CommitError> {
	refuse_if_cancelled(opts, "export commit")?;
	let out =
		read_commit_meta_stdout(git, sha, opts, limit, *current_wire_len)?;
	let (author_name, author_email, author_date, message) =
		parse_commit_meta_borrowed(&out)?;
	let meta_view = CommitRecordMetaView {
		message: &message,
		author_name: &author_name,
		author_email: &author_email,
		author_date: &author_date,
		files: &[],
	};
	let meta_wire_len = measure_meta(&meta_view, opts.cancel.as_ref())?;
	let commit_comma = if commits.is_empty() { 0 } else { 1 };
	let projected_len = *current_wire_len + commit_comma + meta_wire_len;
	if let Some(max) = limit {
		if projected_len > max {
			return Err(CommitError::PayloadLimit {
				limit: max,
				actual: projected_len,
			});
		}
	}
	refuse_if_cancelled(opts, "export commit")?;
	let parent = parent_or_empty(git, sha, opts)?;
	commits.push(CommitRecord {
		message: message.into_owned(),
		author_name: author_name.into_owned(),
		author_email: author_email.into_owned(),
		author_date: author_date.into_owned(),
		files: Vec::new(),
	});
	*current_wire_len = projected_len;

	// One diff buffer, capped by this call's stdout limit. Records are
	// admitted as they are parsed; blob bodies are not queued ahead.
	refuse_if_cancelled(opts, "diff-tree")?;
	let raw = git_bytes(
		git,
		&[
			"diff-tree",
			"-r",
			"-z",
			"--raw",
			"--no-abbrev",
			"--no-commit-id",
			"-M",
			&parent,
			sha,
		],
		opts,
	)?;
	#[cfg(test)]
	trigger_post_diff_cancel_hook_if_active();

	let mut records = RawZ::new(&raw);
	let mut blobs = BlobReader::new(opts);
	let mut failed: Option<CommitError> = None;
	while let Some(entry) = records.next_entry().transpose() {
		refuse_if_cancelled(opts, "diff-tree scan")?;
		match entry {
			Ok(entry) => {
				if let Err(err) = admit_entry(
					git,
					opts,
					limit,
					commits,
					current_wire_len,
					&mut blobs,
					entry,
				) {
					failed = Some(err);
					break;
				}
			}
			Err(err) => {
				failed = Some(err.into());
				break;
			}
		}
	}
	// Success closes the session (cleanup errors surface). Failure drops
	// it, which kills and reaps the tree before the slot is released.
	match failed {
		None => blobs.close()?,
		Some(err) => {
			drop(blobs);
			return Err(err);
		}
	}
	refuse_if_cancelled(opts, "export commit")?;
	Ok(())
}

fn read_commit_meta_stdout(
	git: &Git,
	sha: &str,
	opts: &RunOptions,
	limit: Option<usize>,
	current_wire_len: usize,
) -> Result<Vec<u8>, CommitError> {
	// Config such as log.showSignature or i18n.logOutputEncoding would
	// change the bytes the parser reads.
	let args = [
		"log",
		"-1",
		"-z",
		"--no-show-signature",
		"--encoding=UTF-8",
		"--format=%an%x00%ae%x00%aI%x00%B",
		sha,
	];
	let label = args.join(" ");
	refuse_if_cancelled(opts, &label)?;
	let mut meta_opts = opts.clone();
	let mut payload_limit_on_truncation = false;
	if let Some(max) = limit {
		let remaining = max.saturating_sub(current_wire_len);
		if remaining < opts.max_stdout {
			meta_opts.max_stdout = remaining.saturating_add(1);
			payload_limit_on_truncation = true;
		}
	}
	let out = match git.run_with(&args, &meta_opts) {
		Ok(out) => out,
		Err(GitError::OutputLimit { .. }) if payload_limit_on_truncation => {
			return Err(CommitError::PayloadLimit {
				limit: limit.unwrap(),
				actual: current_wire_len.saturating_add(meta_opts.max_stdout),
			});
		}
		Err(e) => return Err(e.into()),
	};
	if out.truncated {
		if payload_limit_on_truncation {
			return Err(CommitError::PayloadLimit {
				limit: limit.unwrap(),
				actual: current_wire_len.saturating_add(out.stdout.len()),
			});
		}
		return Err(GitError::OutputLimit {
			args: label,
			limit: opts.max_stdout,
		}
		.into());
	}
	Ok(out.stdout)
}

fn parent_or_empty(
	git: &Git,
	sha: &str,
	opts: &RunOptions,
) -> Result<String, CommitError> {
	match git.parents_with(sha, opts)?.into_iter().next() {
		Some(parent) => Ok(parent),
		None if git.is_shallow_with(opts)? => {
			Err(GitError::Shallow(sha.to_string()).into())
		}
		None => Ok(EMPTY_TREE.to_string()),
	}
}

type BorrowedMeta<'a> = (
	std::borrow::Cow<'a, str>,
	std::borrow::Cow<'a, str>,
	std::borrow::Cow<'a, str>,
	std::borrow::Cow<'a, str>,
);

fn parse_commit_meta_borrowed<'a>(
	out: &'a [u8],
) -> Result<BorrowedMeta<'a>, CommitError> {
	let mut fields = out.splitn(4, |&b| b == 0);
	let (Some(name), Some(email), Some(date)) =
		(fields.next(), fields.next(), fields.next())
	else {
		return Err(GitError::Malformed("commit metadata".into()).into());
	};
	let mut msg = fields.next().unwrap_or_default();
	if msg.ends_with(b"\0") {
		msg = &msg[..msg.len().saturating_sub(1)];
	}
	Ok((
		String::from_utf8_lossy(name),
		String::from_utf8_lossy(email),
		String::from_utf8_lossy(date),
		String::from_utf8_lossy(msg),
	))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CommitRecordMetaView<'a> {
	message: &'a str,
	author_name: &'a str,
	author_email: &'a str,
	author_date: &'a str,
	files: &'a [CommitFile],
}

fn admit_entry(
	git: &Git,
	opts: &RunOptions,
	limit: Option<usize>,
	commits: &mut [CommitRecord],
	current_wire_len: &mut usize,
	blobs: &mut BlobReader,
	entry: crate::gitsrc::RawEntry,
) -> Result<(), CommitError> {
	let change = match entry.status {
		b'A' | b'C' => FileChange::Added,
		b'D' => FileChange::Deleted,
		b'R' => FileChange::Renamed,
		_ => FileChange::Modified,
	};
	let path_ok = std::str::from_utf8(&entry.path).is_ok()
		&& entry
			.old_path
			.as_deref()
			.is_none_or(|p| std::str::from_utf8(p).is_ok());
	let special = if change == FileChange::Deleted {
		is_special_mode(&entry.old_mode)
	} else {
		is_special_mode(&entry.new_mode)
			|| (change == FileChange::Renamed
				&& is_special_mode(&entry.old_mode))
	};
	let mut file = CommitFile {
		path: lossy(&entry.path),
		old_path: entry.old_path.as_deref().map(lossy),
		change,
		content: None,
		not_copied: None,
	};
	if !path_ok {
		file.not_copied = Some(NotCopiedReason::NonUtf8Path);
		return push_file(commits, current_wire_len, file, limit, opts);
	}
	if special {
		file.not_copied = Some(NotCopiedReason::UnsupportedType);
		return push_file(commits, current_wire_len, file, limit, opts);
	}
	if change == FileChange::Deleted {
		// A deletion carries no content, so the pre-deletion blob is only
		// classified: retain nothing, scan it. Text of any size (or a blob
		// that cannot be read) stays a plain deletion and never counts
		// against the payload limit; NUL or invalid UTF-8 becomes
		// not-copied so paste leaves the target file alone (spec 4.2).
		file.not_copied = blobs
			.deleted_not_text(git, &entry.old_oid)?
			.map(NotCopiedReason::from);
		return push_file(commits, current_wire_len, file, limit, opts);
	}
	admit_blob(
		commits,
		current_wire_len,
		file,
		git,
		blobs,
		&entry.new_oid,
		opts,
		limit,
	)
}

#[allow(clippy::too_many_arguments)]
fn admit_blob(
	commits: &mut [CommitRecord],
	current_wire_len: &mut usize,
	mut file: CommitFile,
	git: &Git,
	blobs: &mut BlobReader,
	oid: &str,
	opts: &RunOptions,
	limit: Option<usize>,
) -> Result<(), CommitError> {
	let (max_retain, len_empty) = if let Some(max) = limit {
		file.content = Some(String::new());
		file.not_copied = None;
		let commit = commits.last().expect("commit under admission");
		let file_comma = if commit.files.is_empty() { 0 } else { 1 };
		let empty_file_wire_len = measure_file(&file, opts.cancel.as_ref())?;
		let len_empty = *current_wire_len + file_comma + empty_file_wire_len;
		file.content = None;
		if len_empty > max {
			return Err(CommitError::PayloadLimit {
				limit: max,
				actual: len_empty,
			});
		}
		((max - len_empty) as u64, Some(len_empty))
	} else {
		(u64::MAX, None)
	};
	file.content = None;
	file.not_copied = None;
	match blobs.read(git, oid, max_retain)? {
		BlobRead::Missing => {
			file.not_copied = Some(NotCopiedReason::Unreadable);
		}
		BlobRead::Text(t) => {
			file.content = Some(t);
		}
		BlobRead::NotText(r)
		| BlobRead::TooLarge {
			not_text: Some(r), ..
		} => {
			file.not_copied = Some(r.into());
		}
		BlobRead::NotABlob { .. } => {
			// lenient reader 絕不產出 NotABlob（可達情況不應宣稱 UnsupportedType），保留此分支僅為維持窮舉編譯。
			file.not_copied = Some(NotCopiedReason::UnsupportedType);
		}
		BlobRead::TooLarge {
			size,
			not_text: None,
			..
		} => {
			let max = limit.expect("oversize text is only skipped under a cap");
			let base = len_empty.expect("empty-content length");
			let extra = usize::try_from(size).unwrap_or(usize::MAX);
			return Err(CommitError::PayloadLimit {
				limit: max,
				actual: base.saturating_add(extra),
			});
		}
	}
	push_file(commits, current_wire_len, file, limit, opts)
}

fn push_file(
	commits: &mut [CommitRecord],
	current_wire_len: &mut usize,
	file: CommitFile,
	limit: Option<usize>,
	opts: &RunOptions,
) -> Result<(), CommitError> {
	refuse_if_cancelled(opts, "commit file admission")?;
	let commit = commits.last_mut().expect("commit under admission");
	let file_comma = if commit.files.is_empty() { 0 } else { 1 };
	let file_wire_len = measure_file(&file, opts.cancel.as_ref())?;
	let new_wire_len = *current_wire_len + file_comma + file_wire_len;
	if let Some(max) = limit {
		if new_wire_len > max {
			return Err(CommitError::PayloadLimit {
				limit: max,
				actual: new_wire_len,
			});
		}
	}
	commit.files.push(file);
	*current_wire_len = new_wire_len;
	Ok(())
}

fn git_bytes(
	git: &Git,
	args: &[&str],
	opts: &RunOptions,
) -> Result<Vec<u8>, CommitError> {
	let label = args.join(" ");
	refuse_if_cancelled(opts, &label)?;
	let out = git.run_with(args, opts)?;
	if out.truncated {
		return Err(GitError::OutputLimit {
			args: label,
			limit: opts.max_stdout,
		}
		.into());
	}
	Ok(out.stdout)
}

fn refuse_if_cancelled(
	opts: &RunOptions,
	args: &str,
) -> Result<(), CommitError> {
	if opts.cancel.as_ref().is_some_and(CancelToken::is_cancelled) {
		return Err(GitError::Cancelled {
			args: args.to_string(),
		}
		.into());
	}
	Ok(())
}

#[cfg(test)]
#[derive(Clone, Debug, Default)]
struct TestHookState {
	token: Option<CancelToken>,
	writes_before_cancel: usize,
	writes_completed: usize,
	bytes_before_cancel: usize,
	writer_hook_triggered: bool,
	cancel_post_diff: bool,
	post_diff_hook_reached: bool,
}

#[cfg(test)]
thread_local! {
	static TEST_HOOK: std::cell::RefCell<TestHookState> = std::cell::RefCell::new(TestHookState::default());
	static TEST_SERDE_BYTES_COUNTED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
struct TestHookGuard;

#[cfg(test)]
impl Drop for TestHookGuard {
	fn drop(&mut self) {
		TEST_HOOK.with(|h| *h.borrow_mut() = TestHookState::default());
	}
}

#[cfg(test)]
fn set_test_hook(state: TestHookState) -> TestHookGuard {
	TEST_HOOK.with(|h| *h.borrow_mut() = state);
	TestHookGuard
}

#[cfg(test)]
fn get_test_hook_state() -> TestHookState {
	TEST_HOOK.with(|h| h.borrow().clone())
}

#[cfg(test)]
fn reset_serde_bytes_counted() {
	TEST_SERDE_BYTES_COUNTED.with(|c| c.set(0));
}

#[cfg(test)]
fn get_serde_bytes_counted() -> usize {
	TEST_SERDE_BYTES_COUNTED.with(|c| c.get())
}

#[cfg(test)]
fn trigger_writer_cancel_after_progress(buf_len: usize) {
	TEST_HOOK.with(|hook| {
		let mut state = hook.borrow_mut();
		if state.writes_before_cancel > 0
			&& state.token.is_some()
			&& !state.writer_hook_triggered
		{
			state.writes_completed += 1;
			state.bytes_before_cancel += buf_len;
			if state.writes_completed >= state.writes_before_cancel {
				if let Some(token) = &state.token {
					token.cancel();
				}
				state.writer_hook_triggered = true;
			}
		}
	});
}

#[cfg(test)]
fn trigger_post_diff_cancel_hook_if_active() {
	TEST_HOOK.with(|hook| {
		let mut state = hook.borrow_mut();
		if state.cancel_post_diff {
			if let Some(token) = &state.token {
				token.cancel();
			}
			state.post_diff_hook_reached = true;
		}
	});
}

#[cfg(test)]
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PayloadView<'a> {
	commits: &'a [CommitRecord],
}

struct ByteCounter<'a> {
	count: usize,
	cancel: Option<&'a CancelToken>,
}

impl<'a> Write for ByteCounter<'a> {
	fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
		if let Some(token) = self.cancel {
			if token.is_cancelled() {
				return Err(io::Error::other("cancelled"));
			}
		}
		self.count = self.count.saturating_add(buf.len());
		#[cfg(test)]
		{
			TEST_SERDE_BYTES_COUNTED
				.with(|c| c.set(c.get().saturating_add(buf.len())));
			trigger_writer_cancel_after_progress(buf.len());
		}
		Ok(buf.len())
	}

	fn flush(&mut self) -> io::Result<()> {
		Ok(())
	}
}

fn measure_meta(
	meta: &CommitRecordMetaView<'_>,
	cancel: Option<&CancelToken>,
) -> Result<usize, CommitError> {
	let mut counter = ByteCounter { count: 0, cancel };
	serde_json::to_writer(&mut counter, meta).map_err(|e| {
		if cancel.is_some_and(CancelToken::is_cancelled) {
			CommitError::Git(GitError::Cancelled {
				args: "meta serialize".to_string(),
			})
		} else {
			CommitError::InvalidPayload(e.to_string())
		}
	})?;
	Ok(counter.count)
}

fn measure_file(
	file: &CommitFile,
	cancel: Option<&CancelToken>,
) -> Result<usize, CommitError> {
	let mut counter = ByteCounter { count: 0, cancel };
	serde_json::to_writer(&mut counter, file).map_err(|e| {
		if cancel.is_some_and(CancelToken::is_cancelled) {
			CommitError::Git(GitError::Cancelled {
				args: "file serialize".to_string(),
			})
		} else {
			CommitError::InvalidPayload(e.to_string())
		}
	})?;
	Ok(counter.count)
}

struct CancellableBoundedWriter<'a> {
	buffer: Vec<u8>,
	max_bytes: usize,
	cancel: Option<&'a CancelToken>,
}

impl<'a> Write for CancellableBoundedWriter<'a> {
	fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
		if let Some(token) = self.cancel {
			if token.is_cancelled() {
				return Err(io::Error::other("cancelled"));
			}
		}
		if self.buffer.len().saturating_add(buf.len()) > self.max_bytes {
			return Err(io::Error::other("payload limit exceeded"));
		}
		self.buffer.extend_from_slice(buf);
		#[cfg(test)]
		{
			TEST_SERDE_BYTES_COUNTED
				.with(|c| c.set(c.get().saturating_add(buf.len())));
			trigger_writer_cancel_after_progress(buf.len());
		}
		Ok(buf.len())
	}

	fn flush(&mut self) -> io::Result<()> {
		Ok(())
	}
}

fn to_clipboard_text_bounded(
	payload: &CommitsPayload,
	expected_len: usize,
	max_bytes: usize,
	cancel: Option<&CancelToken>,
) -> Result<String, CommitError> {
	if let Some(token) = cancel {
		if token.is_cancelled() {
			return Err(GitError::Cancelled {
				args: "serialize".to_string(),
			}
			.into());
		}
	}
	if expected_len > max_bytes {
		return Err(CommitError::PayloadLimit {
			limit: max_bytes,
			actual: expected_len,
		});
	}
	let mut buffer = Vec::with_capacity(expected_len);
	buffer.extend_from_slice(COMMIT_MARKER.as_bytes());
	buffer.push(b'\n');
	let mut writer = CancellableBoundedWriter {
		buffer,
		max_bytes,
		cancel,
	};
	serde_json::to_writer(&mut writer, payload).map_err(|_| {
		if cancel.is_some_and(CancelToken::is_cancelled) {
			CommitError::Git(GitError::Cancelled {
				args: "serialize".to_string(),
			})
		} else {
			CommitError::PayloadLimit {
				limit: max_bytes,
				actual: writer.buffer.len(),
			}
		}
	})?;
	if let Some(token) = cancel {
		if token.is_cancelled() {
			return Err(GitError::Cancelled {
				args: "serialize".to_string(),
			}
			.into());
		}
	}
	let text =
		String::from_utf8(writer.buffer).expect("serde JSON is valid utf-8");
	Ok(text)
}

/// Marker, newline, and serde JSON of `commits`. Same bytes as
/// [`to_clipboard_text`] for that payload.
#[cfg(test)]
fn wire_len(commits: &[CommitRecord]) -> usize {
	let mut counter = ByteCounter {
		count: 0,
		cancel: None,
	};
	counter
		.write_all(COMMIT_MARKER.as_bytes())
		.expect("counter write");
	counter.write_all(b"\n").expect("counter write");
	serde_json::to_writer(&mut counter, &PayloadView { commits })
		.expect("commit payload serializes");
	counter.count
}

/// The clipboard text: marker line, then JSON.
pub fn to_clipboard_text(payload: &CommitsPayload) -> String {
	let mut buf = Vec::new();
	buf.extend_from_slice(COMMIT_MARKER.as_bytes());
	buf.push(b'\n');
	serde_json::to_writer(&mut buf, payload)
		.expect("commit payload always serializes");
	String::from_utf8(buf).expect("commit payload always produces valid utf-8")
}

/// Same rule as [`crate::clip::detect_mode`]: the marker is the first line.
pub fn is_commit_payload(text: &str) -> bool {
	crate::clip::detect_mode(text) == crate::clip::Mode::Commits
}

pub fn parse_commit_payload(text: &str) -> Result<CommitsPayload, CommitError> {
	if !is_commit_payload(text) {
		return Err(CommitError::NotCommitPayload);
	}
	let json = text.split_once('\n').map_or("", |(_, rest)| rest);
	serde_json::from_str(json)
		.map_err(|e| CommitError::InvalidPayload(e.to_string()))
}

/// Copy notification numbers (spec 4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitCopySummary {
	pub commit_count: usize,
	pub file_count: usize,
	/// UTF-16 code units of the clipboard text.
	pub chars: usize,
	pub not_copied_count: usize,
}

/// A commit copy as its status line needs it, without the payload's file
/// contents: what a remote worker sends back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitCopyOutcome {
	pub text: String,
	pub commit_count: usize,
	pub file_count: usize,
	/// UTF-16 code units of `text`.
	pub chars: usize,
	/// (commit index, path) of every file left out, in commit order.
	pub not_copied: Vec<(usize, String)>,
}

impl CommitExport {
	pub fn outcome(self) -> CommitCopyOutcome {
		let sum = copy_summary(&self.payload, &self.text);
		let not_copied = self
			.payload
			.commits
			.iter()
			.enumerate()
			.flat_map(|(n, c)| {
				c.files
					.iter()
					.filter(|f| f.not_copied.is_some())
					.map(move |f| (n, f.path.clone()))
			})
			.collect();
		CommitCopyOutcome {
			text: self.text,
			commit_count: sum.commit_count,
			file_count: sum.file_count,
			chars: sum.chars,
			not_copied,
		}
	}
}

pub fn copy_summary(payload: &CommitsPayload, text: &str) -> CommitCopySummary {
	let files = payload.commits.iter().flat_map(|c| &c.files);
	CommitCopySummary {
		commit_count: payload.commits.len(),
		file_count: files.clone().count(),
		chars: text.encode_utf16().count(),
		not_copied_count: files.filter(|f| f.not_copied.is_some()).count(),
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReplayAction {
	/// Write the content (a rename also deletes `old_path` first).
	Write,
	Delete,
	Skip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReplaySkipReason {
	/// See the file's `not_copied`.
	NotCopied,
	/// Rejected by the path rules (`paths`), or not repo-relative.
	UnsafePath,
	/// The file on disk is not UTF-8 or cannot be verified.
	NonUtf8Target,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum LayoutConflict {
	RenamedFromIsDirectory,
	DeleteTargetIsDirectory,
	DirectoryInTheWay,
	FileInTheWayOfParent,
}

impl LayoutConflict {
	pub fn describe(self) -> &'static str {
		match self {
			Self::RenamedFromIsDirectory => {
				"the renamed-from path is a directory"
			}
			Self::DeleteTargetIsDirectory => {
				"the path to delete is a directory"
			}
			Self::DirectoryInTheWay => "a directory is in the way of the file",
			Self::FileInTheWayOfParent => {
				"a file is in the way of its parent directory"
			}
		}
	}
}

impl std::fmt::Display for LayoutConflict {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", self.describe())
	}
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FilePlan {
	pub path: String,
	pub old_path: Option<String>,
	pub change: FileChange,
	pub action: ReplayAction,
	/// The target exists now: a write overwrites, a delete removes it.
	pub existed: bool,
	/// A rename's old path exists once the earlier commits of the batch are
	/// replayed, so the preview can tell a real deletion from a no-op.
	pub old_existed: bool,
	pub not_copied: Option<NotCopiedReason>,
	pub skip_reason: Option<ReplaySkipReason>,
	pub layout_conflict: Option<LayoutConflict>,
	pub absolute_path: Option<PathBuf>,
	pub old_absolute_path: Option<PathBuf>,
}

impl FilePlan {
	/// Destination paths whose on-disk state decides this file's replay: the target and a
	/// rename's old path, with their repo-relative spelling. Nothing for a NotCopied skip:
	/// the payload has no bytes, so no destination change can turn it into a write.
	pub(crate) fn freshness_targets(
		&self,
	) -> impl Iterator<Item = (&PathBuf, &str)> + '_ {
		let not_copied = self.skip_reason == Some(ReplaySkipReason::NotCopied);
		let target = (!not_copied)
			.then(|| {
				self.absolute_path.as_ref().map(|p| (p, self.path.as_str()))
			})
			.flatten();
		let old = (!not_copied)
			.then(|| {
				self.old_absolute_path
					.as_ref()
					.zip(self.old_path.as_deref())
			})
			.flatten();
		[target, old].into_iter().flatten()
	}
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitPlan {
	pub message: String,
	pub author_name: String,
	pub author_email: String,
	pub author_date: String,
	/// Index-aligned with the payload commit's `files`.
	pub files: Vec<FilePlan>,
}

impl CommitPlan {
	pub fn refused_by(&self) -> Option<LayoutConflict> {
		self.files.iter().find_map(|f| f.layout_conflict)
	}
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitReplayPlan {
	pub root: PathBuf,
	pub commits: Vec<CommitPlan>,
}

impl CommitReplayPlan {
	/// Owned buffer capacities, excluding this inline struct and allocator
	/// bookkeeping. Includes every replay path and repeated commit metadata.
	pub fn retained_heap_bytes(&self) -> usize {
		let mut bytes = self.root.capacity()
			+ self.commits.capacity() * std::mem::size_of::<CommitPlan>();
		for commit in &self.commits {
			bytes = bytes
				.saturating_add(commit.message.capacity())
				.saturating_add(commit.author_name.capacity())
				.saturating_add(commit.author_email.capacity())
				.saturating_add(commit.author_date.capacity())
				.saturating_add(
					commit.files.capacity() * std::mem::size_of::<FilePlan>(),
				);
			for file in &commit.files {
				bytes = bytes
					.saturating_add(file.path.capacity())
					.saturating_add(
						file.old_path.as_ref().map_or(0, String::capacity),
					)
					.saturating_add(
						file.absolute_path
							.as_ref()
							.map_or(0, PathBuf::capacity),
					)
					.saturating_add(
						file.old_absolute_path
							.as_ref()
							.map_or(0, PathBuf::capacity),
					);
			}
		}
		bytes
	}
}

/// A repo-relative path that passes the restore path rules. Anything the
/// resolver would rewrite (absolute, root-labelled, `./`) is refused: git
/// only ever produces plain relative paths.
///
/// A parent the batch's earlier commits already replaced (`layout` holds an
/// override for it) is no longer the symlink the disk may still show.
fn target(root: &Path, path: &str, layout: &PlannedLayout) -> Option<PathBuf> {
	// Like git ("beyond a symbolic link"), refuse paths whose parent
	// directories go through a symlink: containment alone would let the
	// write or delete land on another tracked path.
	let mut dir = root.to_path_buf();
	let parents = path.split('/').collect::<Vec<_>>();
	for segment in &parents[..parents.len().saturating_sub(1)] {
		dir.push(segment);
		if !layout.overrides.contains_key(&dir) && is_symlink(&dir) {
			return None;
		}
	}
	resolve_write_target(&[root], path)
		.ok()
		.filter(|t| t.relative_path == path)
		.map(|t| t.absolute_path)
		.filter(|abs| !unsafe_replay_target(root, abs))
}

fn plan_file(root: &Path, f: &CommitFile, layout: &PlannedLayout) -> FilePlan {
	let mut plan = FilePlan {
		path: f.path.clone(),
		old_path: f.old_path.clone(),
		change: f.change,
		action: ReplayAction::Skip,
		existed: false,
		old_existed: false,
		not_copied: f.not_copied,
		skip_reason: None,
		layout_conflict: None,
		absolute_path: None,
		old_absolute_path: None,
	};
	let deleted = f.change == FileChange::Deleted;
	if !deleted && f.content.is_none() && plan.not_copied.is_none() {
		plan.not_copied = Some(NotCopiedReason::Unreadable);
	}
	if plan.not_copied.is_some() {
		plan.skip_reason = Some(ReplaySkipReason::NotCopied);
		return plan;
	}
	let old = f.old_path.as_deref().map(|p| target(root, p, layout));
	let (Some(abs), None | Some(Some(_))) =
		(target(root, &f.path, layout), &old)
	else {
		plan.skip_reason = Some(ReplaySkipReason::UnsafePath);
		return plan;
	};
	// A symlink is replaced, not written through: its target is irrelevant.
	// Keep the path on a non-UTF-8 skip so freshness can see that exact file
	// without inventing a second planner. An earlier commit of the batch may
	// have deleted a symlink or non-UTF-8 file at this exact path (the layout
	// holds an override for it), so only an untouched path is checked on disk.
	// Known limit (issue #76): overrides match the exact path only, so a
	// deleted symlink ancestor, or a case-only alias on a case-insensitive
	// filesystem, still reads the real disk and the preview may disagree
	// with Apply.
	// A hard link needs no check of its own: the write replaces this entry,
	// never the shared file (see `fsutil::write_text_file`).
	if !deleted
		&& !layout.overrides.contains_key(&abs)
		&& !is_symlink(&abs)
		&& must_not_overwrite(&abs)
	{
		plan.skip_reason = Some(ReplaySkipReason::NonUtf8Target);
		plan.existed = abs.exists();
		plan.absolute_path = Some(abs);
		return plan;
	}
	plan.action = if deleted {
		ReplayAction::Delete
	} else {
		ReplayAction::Write
	};
	// A delete asks the simulated layout (an earlier commit of the batch may
	// have added or removed the file). A write keeps the real disk: overwrite
	// consent protects a file that exists before the replay starts.
	plan.existed = if deleted {
		layout.exists(&abs)
	} else {
		abs.exists()
	};
	plan.absolute_path = Some(abs);
	plan.old_absolute_path = old.flatten();
	plan.old_existed = plan
		.old_absolute_path
		.as_deref()
		.is_some_and(|o| layout.exists(o));
	plan
}

fn plan_commit(
	root: &Path,
	c: &CommitRecord,
	layout: &PlannedLayout,
) -> CommitPlan {
	CommitPlan {
		message: c.message.clone(),
		author_name: c.author_name.clone(),
		author_email: c.author_email.clone(),
		author_date: c.author_date.clone(),
		files: c.files.iter().map(|f| plan_file(root, f, layout)).collect(),
	}
}

/// What a path holds once the earlier commits of a batch are replayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Node {
	File,
	Dir,
	Absent,
}

/// The on-disk tree seen through the effects of the commits planned so far:
/// a path answers from `overrides` first and from `fs::symlink_metadata`
/// otherwise. Empty, it is the plain disk (what `replay_commit` plans against).
///
/// Only the layout is simulated. Symlink checks in `target` and
/// `must_not_overwrite` stay on the real disk, except for a path with an
/// override: a payload creates neither symlinks nor non-UTF-8 files, but an
/// earlier commit can delete one, and what is left there is then a regular
/// file or nothing.
///
/// An override applies to the exact path only. Known limits (issue #76), where
/// the preview and dry-run may disagree with Apply: a symlink ancestor deleted
/// by an earlier commit is still looked through on the real disk, and on a
/// case-insensitive filesystem a path differing only by case from an earlier
/// commit's path is a different key.
#[derive(Debug, Default)]
struct PlannedLayout {
	overrides: std::collections::HashMap<PathBuf, Node>,
	/// The non-absent overrides, by parent directory.
	added:
		std::collections::HashMap<PathBuf, std::collections::HashSet<PathBuf>>,
}

impl PlannedLayout {
	fn node(&self, path: &Path) -> Node {
		if let Some(&n) = self.overrides.get(path) {
			return n;
		}
		match fs::symlink_metadata(path) {
			Ok(m) if m.is_dir() => Node::Dir,
			Ok(_) => Node::File,
			Err(_) => Node::Absent,
		}
	}

	fn is_dir(&self, path: &Path) -> bool {
		self.node(path) == Node::Dir
	}

	/// Like `Path::exists`: a symlink counts by its target on disk.
	fn exists(&self, path: &Path) -> bool {
		match self.overrides.get(path) {
			Some(&n) => n != Node::Absent,
			None => path.exists(),
		}
	}

	fn set(&mut self, path: &Path, node: Node) {
		self.overrides.insert(path.to_path_buf(), node);
		let Some(parent) = path.parent() else {
			return;
		};
		if node == Node::Absent {
			if let Some(set) = self.added.get_mut(parent) {
				set.remove(path);
			}
		} else {
			self.added
				.entry(parent.to_path_buf())
				.or_default()
				.insert(path.to_path_buf());
		}
	}

	/// The entries of `dir`: disk entries minus those overridden as absent,
	/// plus overridden entries directly inside it.
	fn children(&self, dir: &Path) -> Vec<(PathBuf, Node)> {
		let mut out: std::collections::HashMap<PathBuf, Node> =
			std::collections::HashMap::new();
		if let Ok(entries) = fs::read_dir(dir) {
			for entry in entries {
				let Ok(entry) = entry else {
					// An unreadable entry counts as a live file nobody deletes,
					// so `dir` is never taken for empty.
					out.insert(dir.join("<unreadable entry>"), Node::File);
					continue;
				};
				let node = match entry.file_type() {
					Ok(t) if t.is_dir() => Node::Dir,
					_ => Node::File,
				};
				out.insert(entry.path(), node);
			}
		}
		out.retain(|path, _| self.overrides.get(path) != Some(&Node::Absent));
		for path in self.added.get(dir).into_iter().flatten() {
			out.insert(path.clone(), self.overrides[path]);
		}
		out.into_iter().collect()
	}

	/// Whether `dir` still holds an entry. Stops at the first live one.
	fn has_children(&self, dir: &Path) -> bool {
		if self.added.get(dir).is_some_and(|set| !set.is_empty()) {
			return true;
		}
		let Ok(entries) = fs::read_dir(dir) else {
			return false;
		};
		entries.into_iter().any(|entry| match entry {
			Ok(entry) => {
				self.overrides.get(&entry.path()) != Some(&Node::Absent)
			}
			Err(_) => true,
		})
	}

	/// Records what replaying `files` leaves behind, in replay's order:
	/// deletions (a rename's old path included) first, removing the parents
	/// they empty, then the writes with their parent directories.
	fn apply(&mut self, root: &Path, files: &[FilePlan]) {
		let live = files.iter().filter(|f| f.action != ReplayAction::Skip);
		// All files go first and the emptied parents are walked afterwards:
		// the final set does not depend on the order, and each directory is
		// read once per walk instead of once per removed file.
		let mut parents = Vec::new();
		for f in live.clone() {
			let del = (f.action == ReplayAction::Delete)
				.then_some(f.absolute_path.as_deref())
				.flatten();
			for abs in f.old_absolute_path.as_deref().into_iter().chain(del) {
				if self.node(abs) == Node::File {
					self.set(abs, Node::Absent);
					parents.extend(abs.parent());
				}
			}
		}
		parents.sort_unstable();
		parents.dedup();
		for parent in parents {
			self.prune_empty(root, parent);
		}
		for f in live.filter(|f| f.action == ReplayAction::Write) {
			let Some(abs) = f.absolute_path.as_deref() else {
				continue;
			};
			self.set(abs, Node::File);
			for a in abs.ancestors().skip(1) {
				if a == root || !a.starts_with(root) {
					break;
				}
				self.set(a, Node::Dir);
			}
		}
	}

	/// Mirrors `delete`: a removed file walks up, dropping each parent it
	/// leaves empty; the root stays.
	fn prune_empty(&mut self, root: &Path, from: &Path) {
		let mut dir = Some(from);
		while let Some(d) = dir.filter(|d| *d != root && d.starts_with(root)) {
			if self.node(d) != Node::Dir || self.has_children(d) {
				break;
			}
			self.set(d, Node::Absent);
			dir = d.parent();
		}
	}
}

/// Planned writes and deletes the layout would make fail halfway
/// through a commit, as `(file index, reason)`: a directory where a file is
/// deleted, a directory where a file is written (unless this commit's own
/// deletions empty it, as `delete` removes emptied parents), or a file where
/// a write needs a directory (unless this commit deletes that file).
fn layout_conflicts(
	root: &Path,
	files: &[FilePlan],
	layout: &PlannedLayout,
) -> Vec<(usize, LayoutConflict)> {
	let mut deleted = std::collections::HashSet::new();
	for f in files.iter().filter(|f| f.action != ReplayAction::Skip) {
		deleted.extend(f.old_absolute_path.as_deref());
		if f.action == ReplayAction::Delete {
			deleted.extend(f.absolute_path.as_deref());
		}
	}
	// Whether deleting `deleted` leaves `dir` empty and so removed. An empty
	// directory is never on the upward walk, so it stays.
	fn emptied(
		dir: &Path,
		deleted: &std::collections::HashSet<&Path>,
		layout: &PlannedLayout,
	) -> bool {
		let children = layout.children(dir);
		!children.is_empty()
			&& children.iter().all(|(path, node)| match node {
				Node::Dir => emptied(path, deleted, layout),
				_ => deleted.contains(path.as_path()),
			})
	}
	let mut out = Vec::new();
	for (i, f) in files.iter().enumerate() {
		let Some(abs) = f.absolute_path.as_deref() else {
			continue;
		};
		let old = f.old_absolute_path.as_deref().filter(|o| layout.is_dir(o));
		let conflict = match f.action {
			ReplayAction::Skip => None,
			_ if old.is_some() => Some(LayoutConflict::RenamedFromIsDirectory),
			ReplayAction::Delete => layout
				.is_dir(abs)
				.then_some(LayoutConflict::DeleteTargetIsDirectory),
			ReplayAction::Write if layout.is_dir(abs) => {
				(!emptied(abs, &deleted, layout))
					.then_some(LayoutConflict::DirectoryInTheWay)
			}
			ReplayAction::Write => abs
				.ancestors()
				.skip(1)
				.take_while(|a| *a != root && a.starts_with(root))
				.find(|a| layout.node(a) == Node::File && !deleted.contains(a))
				.map(|_| LayoutConflict::FileInTheWayOfParent),
		};
		if let Some(reason) = conflict {
			out.push((i, reason));
		}
	}
	out
}

/// Marks [`layout_conflicts`] as unsafe skips, until skipping one (and so
/// not deleting its paths) uncovers no new conflict.
///
/// `layout` already holds the effects of the batch's earlier commits, so a
/// blocker an earlier commit removes or creates is seen as replay will see it.
fn skip_layout_conflicts(
	root: &Path,
	files: &mut [FilePlan],
	layout: &PlannedLayout,
) {
	loop {
		let conflicts = layout_conflicts(root, files, layout);
		if conflicts.is_empty() {
			return;
		}
		for (i, conflict) in conflicts {
			files[i].action = ReplayAction::Skip;
			files[i].skip_reason = Some(ReplaySkipReason::UnsafePath);
			files[i].layout_conflict = Some(conflict);
		}
	}
}

/// Preview: what replaying `payload` onto the current disk state would do via
/// [`crate::transfer::CommitReplayPreview::apply`].
pub fn plan_commit_replay(
	git: &Git,
	payload: &CommitsPayload,
) -> CommitReplayPlan {
	plan_commit_replay_with(git, payload, &RunOptions::default())
		.expect("replay planning with default options has no cancel token")
}

/// [`plan_commit_replay`] that polls `opts` before each commit and file.
///
/// Each file still goes through the same `plan_file` rules. Encoding classification reads
/// at most 8 MiB in one `read` ([`crate::fsutil::must_not_overwrite`]); that
/// call cannot be interrupted. The token is checked before the next file.
pub fn plan_commit_replay_with(
	git: &Git,
	payload: &CommitsPayload,
	opts: &RunOptions,
) -> Result<CommitReplayPlan, CommitError> {
	plan_commit_replay_in(git, git.root(), payload, opts)
}

/// [`plan_commit_replay_with`] with the replay's write scope: only
/// `scope`'s targets may be written or deleted. A whole-repository replay
/// passes the repository root. A write or delete (a rename's old path
/// included) outside `scope` is refused before anything is planned as
/// writable.
pub fn plan_commit_replay_in(
	git: &Git,
	scope: &Path,
	payload: &CommitsPayload,
	opts: &RunOptions,
) -> Result<CommitReplayPlan, CommitError> {
	refuse_if_cancelled(opts, "replay-plan")?;
	let root = git.root().to_path_buf();
	let mut commits = Vec::new();
	// Each commit is planned after the earlier ones, as replay writes them.
	let mut layout = PlannedLayout::default();
	for commit in &payload.commits {
		refuse_if_cancelled(opts, "replay-plan")?;
		let mut files = Vec::new();
		for file in &commit.files {
			refuse_if_cancelled(opts, "replay-plan")?;
			files.push(plan_file(&root, file, &layout));
		}
		skip_layout_conflicts(&root, &mut files, &layout);
		let plan = CommitPlan {
			message: commit.message.clone(),
			author_name: commit.author_name.clone(),
			author_email: commit.author_email.clone(),
			author_date: commit.author_date.clone(),
			files,
		};
		// A refused commit writes nothing, and replay stops there.
		if plan.refused_by().is_none() {
			layout.apply(&root, &plan.files);
		}
		commits.push(plan);
	}
	refuse_if_cancelled(opts, "replay-plan")?;
	let plan = CommitReplayPlan { commits, root };
	for commit in &plan.commits {
		for f in &commit.files {
			for abs in f.old_absolute_path.iter().chain(f.absolute_path.iter())
			{
				if unsafe_replay_target(scope, abs) {
					return Err(CommitError::OutsideScope {
						path: f.path.clone(),
						scope: scope.to_path_buf(),
					});
				}
			}
		}
	}
	Ok(plan)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayFailure {
	/// Index into the payload's commits.
	pub index: usize,
	pub message: String,
	pub error: String,
	pub layout_conflict: Option<LayoutConflict>,
	pub conflict_path: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayResult {
	/// New commit OIDs, in replay order. Never rolled back.
	pub created: Vec<String>,
	/// The commit replay stopped at, if any.
	pub failure: Option<ReplayFailure>,
}

pub(crate) struct ReplaySession {
	no_hooks: NoHooks,
	/// The write scope: every target is held to it, as the preview was.
	scope: PathBuf,
	_guard: crate::workspace::HeavyGuard,
}

impl ReplaySession {
	/// Heavy lock + empty hooks dir, taken before any write, with the
	/// replay's write scope ([`plan_commit_replay_in`]). Err is the
	/// ReplayResult the old replay returned for that failure (failure at
	/// index 0, or none for an empty payload).
	pub(crate) fn begin_in(
		git: &Git,
		scope: &Path,
		payload: &CommitsPayload,
		opts: &RunOptions,
	) -> Result<Self, ReplayResult> {
		let mut result = ReplayResult::default();
		let guard = RepoIdentity::resolve(git, opts)
			.and_then(|id| lock_heavy(&id, opts));
		let _guard = match guard {
			Ok(g) => g,
			Err(e) => {
				result.failure =
					payload.commits.first().map(|c| ReplayFailure {
						index: 0,
						message: c.message.clone(),
						error: e.to_string(),
						layout_conflict: None,
						conflict_path: None,
					});
				return Err(result);
			}
		};
		let no_hooks = match NoHooks::create() {
			Ok(h) => h,
			Err(e) => {
				result.failure =
					payload.commits.first().map(|c| ReplayFailure {
						index: 0,
						message: c.message.clone(),
						error: format!(
							"cannot create an empty hooks directory: {e}"
						),
						layout_conflict: None,
						conflict_path: None,
					});
				return Err(result);
			}
		};
		Ok(Self {
			no_hooks,
			scope: scope.to_path_buf(),
			_guard,
		})
	}

	/// Replays every commit; never polls cancellation.
	pub(crate) fn run(
		&self,
		git: &Git,
		payload: &CommitsPayload,
	) -> ReplayResult {
		let mut result = ReplayResult::default();
		for (index, commit) in payload.commits.iter().enumerate() {
			match replay_commit(git, &self.scope, commit, &self.no_hooks.config)
			{
				Ok(sha) => result.created.push(sha),
				Err(err) => {
					result.failure = Some(ReplayFailure {
						index,
						message: commit.message.clone(),
						error: err.error,
						layout_conflict: err.layout_conflict,
						conflict_path: err.conflict_path,
					});
					break;
				}
			}
		}
		result
	}
}

#[cfg(test)]
pub(crate) fn replay(git: &Git, payload: &CommitsPayload) -> ReplayResult {
	match ReplaySession::begin_in(
		git,
		git.root(),
		payload,
		&RunOptions::default(),
	) {
		Ok(session) => session.run(git, payload),
		Err(refused) => refused,
	}
}

/// A fresh empty directory for `core.hooksPath`, so replay runs none of the
/// destination's hooks: `--no-verify` alone still runs prepare-commit-msg,
/// post-commit and post-index-change. `/dev/null` does not exist on Windows.
struct NoHooks {
	dir: PathBuf,
	/// `core.hooksPath=<dir>`, for `git -c`.
	config: String,
}

impl NoHooks {
	fn create() -> io::Result<Self> {
		let nanos = std::time::SystemTime::now()
			.duration_since(std::time::UNIX_EPOCH)
			.map_or(0, |d| d.as_nanos());
		let dir = std::env::temp_dir()
			.join(format!("snip-no-hooks-{}-{nanos}", std::process::id()));
		// `create_dir`, not `create_dir_all`: an existing directory could
		// already hold hooks.
		fs::create_dir(&dir)?;
		let Some(config) = dir.to_str().map(|d| format!("core.hooksPath={d}"))
		else {
			let _ = fs::remove_dir(&dir);
			return Err(io::Error::other("temp directory is not UTF-8"));
		};
		Ok(Self { dir, config })
	}
}

impl Drop for NoHooks {
	fn drop(&mut self) {
		let _ = fs::remove_dir(&self.dir);
	}
}

struct ReplayCommitError {
	error: String,
	layout_conflict: Option<LayoutConflict>,
	conflict_path: Option<String>,
}

impl From<String> for ReplayCommitError {
	fn from(error: String) -> Self {
		Self {
			error,
			layout_conflict: None,
			conflict_path: None,
		}
	}
}

fn replay_commit(
	git: &Git,
	scope: &Path,
	commit: &CommitRecord,
	no_hooks: &str,
) -> Result<String, ReplayCommitError> {
	let root = git.root();
	// Replay plans against the disk as the earlier commits left it.
	let layout = PlannedLayout::default();
	let plan = plan_commit(root, commit, &layout);
	// The preview skips these; replay refuses the commit before touching
	// anything rather than stop halfway with a half-staged worktree.
	if let Some(&(i, conflict)) =
		layout_conflicts(root, &plan.files, &layout).first()
	{
		return Err(ReplayCommitError {
			error: format!("{}: {}", plan.files[i].path, conflict.describe()),
			layout_conflict: Some(conflict),
			conflict_path: Some(plan.files[i].path.clone()),
		});
	}
	// Paths whose change is on disk now; they alone go into the commit.
	let mut paths: Vec<&str> = Vec::new();
	// Deleted paths: staged only if HEAD tracks them. A path that is only in
	// the index (a staged new file) has nothing to commit, and `commit
	// --only` would reject it once `add` dropped it from the index.
	let mut deleted: Vec<&str> = Vec::new();

	// Recheck every write target before anything destructive happens, so a
	// target that changed since planning stops the replay instead of
	// leaving a commit that only records the rename's deletion.
	for (f, src) in plan.files.iter().zip(&commit.files) {
		let (ReplayAction::Write, Some(abs), Some(_)) =
			(f.action, &f.absolute_path, &src.content)
		else {
			continue;
		};
		if unsafe_replay_target(scope, abs) {
			return Err(format!("{}: unsafe path", f.path).into());
		}
		if !is_symlink(abs) && must_not_overwrite(abs) {
			return Err(format!(
				"{}: target is not UTF-8 or cannot be verified",
				f.path
			)
			.into());
		}
	}

	// Deletions first: a rename swap or a rename onto a re-added path must
	// not delete what this commit just wrote.
	for f in &plan.files {
		let old = f.old_absolute_path.as_deref().zip(f.old_path.as_deref());
		let del = (f.action == ReplayAction::Delete)
			.then(|| f.absolute_path.as_deref().map(|a| (a, f.path.as_str())))
			.flatten();
		for (abs, rel) in old.into_iter().chain(del) {
			delete(scope, root, abs, rel)?;
			deleted.push(rel);
		}
	}
	for (f, src) in plan.files.iter().zip(&commit.files) {
		let (ReplayAction::Write, Some(abs), Some(content)) =
			(f.action, &f.absolute_path, &src.content)
		else {
			continue;
		};
		if unsafe_replay_target(root, abs) {
			return Err(format!("{}: unsafe path", f.path).into());
		}
		// The write replaces the entry: a symlink or a hard link here is
		// itself replaced, never written through.
		write_text_file(abs, content)
			.map_err(|e| format!("{}: {e}", f.path))?;
		paths.push(&f.path);
	}

	let err = |e: GitError| e.to_string();
	if !deleted.is_empty() {
		let mut args = vec![
			"-c",
			no_hooks,
			"--literal-pathspecs",
			"ls-tree",
			"-z",
			"--name-only",
			"HEAD",
			"--",
		];
		args.extend(&deleted);
		// An unborn HEAD tracks nothing.
		let out = if git.head().map_err(err)?.is_some() {
			git.run(&args).map_err(err)?
		} else {
			Vec::new()
		};
		let tracked: Vec<&[u8]> =
			out.split(|&b| b == 0).filter(|p| !p.is_empty()).collect();
		paths
			.extend(deleted.iter().filter(|p| tracked.contains(&p.as_bytes())));
	}
	paths.sort_unstable();
	paths.dedup();

	if !paths.is_empty() {
		// `-f`: the source tracked it, even if it is ignored here.
		let mut args = vec![
			"-c",
			no_hooks,
			"--literal-pathspecs",
			"add",
			"-A",
			"-f",
			"--",
		];
		args.extend(&paths);
		git.run(&args).map_err(err)?;
	}
	// `--only` with no paths still commits HEAD's tree, never the index.
	let mut args = vec![
		"-c",
		no_hooks,
		"--literal-pathspecs",
		"commit",
		"--quiet",
		"--only",
		"--no-verify",
		"--allow-empty",
		"--allow-empty-message",
		"--cleanup=verbatim",
		"-F",
		"-",
		"--",
	];
	args.extend(&paths);
	run_commit(git, &args, commit).map_err(err)?;
	let head = git
		.run(&["-c", no_hooks, "rev-parse", "HEAD"])
		.map_err(err)?;
	Ok(String::from_utf8_lossy(&head).trim().to_string())
}

/// Removes `abs`; already absent is fine. Parent directories left empty go
/// too (as git checkout does), so a later write may put a file there. The
/// safety check is held to the replay's write `scope`; the upward walk is
/// bounded by the resolved repository `root`, never above it.
fn delete(
	scope: &Path,
	root: &Path,
	abs: &Path,
	rel: &str,
) -> Result<(), String> {
	if unsafe_replay_target(scope, abs) {
		return Err(format!("{rel}: unsafe path"));
	}
	match fs::remove_file(abs) {
		Ok(()) => {}
		// A file where a parent directory should be: the path is absent.
		Err(e)
			if matches!(
				e.kind(),
				io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
			) =>
		{
			return Ok(())
		}
		Err(e) => return Err(format!("{rel}: {e}")),
	}
	let mut dir = abs.parent();
	// `remove_dir` fails on a non-empty directory, which ends the walk.
	while let Some(d) = dir.filter(|d| *d != root && d.starts_with(root)) {
		if fs::remove_dir(d).is_err() {
			break;
		}
		dir = d.parent();
	}
	Ok(())
}

fn is_symlink(p: &Path) -> bool {
	fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink())
}

fn run_commit(
	git: &Git,
	args: &[&str],
	commit: &CommitRecord,
) -> Result<(), GitError> {
	// Author through the environment: `--author` would treat a value
	// without `<email>` as a search pattern. The committer stays local.
	let mut cmd = git.command();
	cmd.args(args)
		.env("GIT_AUTHOR_NAME", &commit.author_name)
		.env("GIT_AUTHOR_EMAIL", &commit.author_email)
		.env("GIT_AUTHOR_DATE", &commit.author_date);
	git.exec(
		cmd,
		&args.join(" "),
		Some(commit.message.as_bytes()),
		&RunOptions::default(),
	)?;
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::process::Command;
	use std::time::Duration;

	struct Repo {
		dir: tempfile::TempDir,
		cfg: PathBuf,
	}

	impl Repo {
		fn new(branch: &str) -> Self {
			let dir = tempfile::tempdir().unwrap();
			let cfg = dir.path().join("empty.gitconfig");
			fs::write(&cfg, "").unwrap();
			fs::create_dir(dir.path().join("r")).unwrap();
			let repo = Self { dir, cfg };
			repo.git(&["init", "-q", "-b", branch]);
			repo.git(&["config", "user.name", "Local"]);
			repo.git(&["config", "user.email", "local@example.com"]);
			repo.git(&["config", "core.autocrlf", "false"]);
			repo.git(&["config", "commit.gpgsign", "false"]);
			repo
		}

		fn path(&self) -> PathBuf {
			self.dir.path().join("r")
		}

		fn cmd(&self, args: &[&str]) -> Command {
			let mut c = Command::new("git");
			c.args(args)
				.current_dir(self.path())
				.env("GIT_CONFIG_GLOBAL", &self.cfg)
				.env("GIT_CONFIG_NOSYSTEM", "1");
			c
		}

		fn git(&self, args: &[&str]) -> String {
			let out = self.cmd(args).output().unwrap();
			assert!(
				out.status.success(),
				"git {args:?}: {}",
				String::from_utf8_lossy(&out.stderr)
			);
			String::from_utf8(out.stdout).unwrap().trim().to_string()
		}

		fn write(&self, name: &str, content: &[u8]) {
			let p = self.path().join(name);
			fs::create_dir_all(p.parent().unwrap()).unwrap();
			fs::write(p, content).unwrap();
		}

		/// Commits everything as `Alice` at `date`.
		fn commit(&self, msg: &str, date: &str) -> String {
			self.git(&["add", "-A"]);
			let out = self
				.cmd(&[
					"commit",
					"-q",
					"--allow-empty",
					"--cleanup=verbatim",
					"-m",
					msg,
				])
				.env("GIT_AUTHOR_NAME", "Alice")
				.env("GIT_AUTHOR_EMAIL", "alice@example.com")
				.env("GIT_AUTHOR_DATE", date)
				.output()
				.unwrap();
			assert!(out.status.success());
			self.git(&["rev-parse", "HEAD"])
		}

		fn open(&self) -> Git {
			Git::open(&self.path()).unwrap()
		}

		/// `%an|%ae|%aI` of `rev`, plus its raw message.
		fn meta(&self, rev: &str) -> (String, String) {
			let who = self.git(&["log", "-1", "--format=%an|%ae|%aI", rev]);
			let raw = self.git(&["cat-file", "commit", rev]);
			let msg = raw.split_once("\n\n").unwrap().1.to_string();
			(who, msg)
		}
	}

	/// Source history: base, then a merge, a rename + binary, a delete.
	fn source() -> (Repo, String, Vec<String>) {
		let r = Repo::new("main");
		r.write("keep.txt", b"keep\n");
		r.write("old.txt", b"rename me\nline 2\nline 3\n");
		r.write("gone.txt", b"to be deleted\n");
		let base = r.commit("base", "2020-01-01T00:00:00+00:00");
		r.git(&["checkout", "-q", "-b", "side"]);
		r.write("side.txt", b"from side\n");
		r.commit("side work", "2020-01-01T12:00:00+00:00");
		r.git(&["checkout", "-q", "main"]);
		r.write("keep.txt", b"keep v2\n");
		r.commit("main work", "2020-01-01T13:00:00+00:00");
		// First parent main, second parent side: only side.txt differs
		// from the first parent.
		let out = r
			.cmd(&["merge", "-q", "--no-ff", "--no-edit", "side"])
			.env("GIT_AUTHOR_NAME", "Alice")
			.env("GIT_AUTHOR_EMAIL", "alice@example.com")
			.env("GIT_AUTHOR_DATE", "2020-01-02T03:04:05+09:00")
			.output()
			.unwrap();
		assert!(out.status.success());
		let merge = r.git(&["rev-parse", "HEAD"]);
		fs::create_dir(r.path().join("dir")).unwrap();
		r.git(&["mv", "old.txt", "dir/new.txt"]);
		r.write("img.bin", &[0x89, b'P', b'N', b'G', 0, 1, 2]);
		r.write("big5.txt", &[0xa4, 0xe9, 0xa5, 0xbb]);
		let second = r.commit(
			"rename and binary\n\n\nbody with two blank lines  \n# not a comment\n",
			"2021-06-07T08:09:10-05:30",
		);
		r.git(&["rm", "-q", "gone.txt"]);
		let third = r.commit("delete", "2022-02-03T04:05:06+01:00");
		let _ = base;
		(r, merge.clone(), vec![merge, second, third])
	}

	#[test]
	fn commits_select_copy_and_replay_round_trip() {
		let (src, _, expected) = source();
		let g = src.open();
		let shas = select_last(&g, 3).unwrap();
		assert_eq!(shas, expected);
		let range =
			select_range(&g, &format!("{}^", expected[0]), "HEAD").unwrap();
		assert_eq!(range, expected);

		let payload = copy_commits(&g, &shas).unwrap();
		let text = to_clipboard_text(&payload);
		assert!(text.starts_with("// snip-sync commits v1\n{"));
		assert!(is_commit_payload(&text));
		assert!(!is_commit_payload("// clipcode-root: x\n"));
		let parsed = parse_commit_payload(&text).unwrap();
		assert_eq!(parsed, payload);

		// Merge: only the diff against the first parent.
		let merge = &payload.commits[0];
		assert_eq!(merge.files.len(), 1);
		assert_eq!(merge.files[0].path, "side.txt");
		assert_eq!(merge.files[0].change, FileChange::Added);
		let second = &payload.commits[1];
		let find = |p: &str| second.files.iter().find(|f| f.path == p).unwrap();
		assert_eq!(find("dir/new.txt").change, FileChange::Renamed);
		assert_eq!(find("dir/new.txt").old_path.as_deref(), Some("old.txt"));
		assert_eq!(find("img.bin").not_copied, Some(NotCopiedReason::Binary));
		assert_eq!(find("img.bin").content, None);
		assert_eq!(find("big5.txt").not_copied, Some(NotCopiedReason::NonUtf8));
		let third = &payload.commits[2];
		assert_eq!(third.files[0].change, FileChange::Deleted);
		assert_eq!(third.files[0].content, None);
		let summary = copy_summary(&payload, &text);
		assert_eq!(
			(
				summary.commit_count,
				summary.file_count,
				summary.not_copied_count
			),
			(3, 5, 2)
		);
		assert_eq!(summary.chars, text.encode_utf16().count());

		// Target: unrelated history on another branch, with a foreign
		// staged file that must stay out of every replayed commit.
		let dst = Repo::new("feature");
		dst.write("old.txt", b"rename me\nline 2\nline 3\n");
		dst.write("gone.txt", b"to be deleted\n");
		dst.write("keep.txt", b"target keep\n");
		dst.commit("target root", "2019-01-01T00:00:00+00:00");
		dst.write("foreign.txt", b"staged by the user\n");
		dst.git(&["add", "foreign.txt"]);
		let tg = dst.open();

		let plan = plan_commit_replay(&tg, &parsed);
		assert_eq!(plan.commits.len(), 3);
		let p2 = &plan.commits[1].files;
		let pf = |p: &str| p2.iter().find(|f| f.path == p).unwrap();
		assert_eq!(pf("dir/new.txt").action, ReplayAction::Write);
		assert!(pf("dir/new.txt").old_absolute_path.is_some());
		assert_eq!(pf("img.bin").action, ReplayAction::Skip);
		assert_eq!(
			pf("img.bin").skip_reason,
			Some(ReplaySkipReason::NotCopied)
		);
		assert_eq!(plan.commits[2].files[0].action, ReplayAction::Delete);
		assert!(plan.commits[2].files[0].existed);

		let result = replay(&tg, &parsed);
		assert_eq!(result.failure, None);
		assert_eq!(result.created.len(), 3);
		assert_eq!(dst.git(&["rev-parse", "HEAD"]), result.created[2]);

		for (i, created) in result.created.iter().enumerate() {
			assert_eq!(dst.meta(created), src.meta(&expected[i]), "commit {i}");
			let committer = dst.git(&["log", "-1", "--format=%cn", created]);
			assert_eq!(committer, "Local");
			let tree = dst.git(&["ls-tree", "-r", "--name-only", created]);
			assert!(!tree.contains("foreign.txt"), "commit {i}: {tree}");
		}
		assert_eq!(
			dst.git(&["diff", "--cached", "--name-only"]),
			"foreign.txt"
		);
		let show = |r: &Repo, spec: &str| r.git(&["show", spec]);
		assert_eq!(show(&dst, "HEAD:side.txt"), show(&src, "HEAD:side.txt"));
		assert_eq!(
			show(&dst, "HEAD:dir/new.txt"),
			show(&src, "HEAD:dir/new.txt")
		);
		assert_eq!(show(&dst, "HEAD:keep.txt"), "target keep");
		let tree = dst.git(&["ls-tree", "-r", "--name-only", "HEAD"]);
		assert_eq!(tree, "dir/new.txt\nkeep.txt\nside.txt");
		// The second commit contains exactly the rename.
		let changed = dst.git(&[
			"diff-tree",
			"-r",
			"--name-status",
			"--no-commit-id",
			"-M",
			&result.created[1],
		]);
		assert_eq!(changed, "R100\told.txt\tdir/new.txt");
		assert!(!dst.path().join("img.bin").exists());
	}

	#[test]
	fn commits_non_contiguous_selection_is_refused() {
		let (src, merge, _) = source();
		let g = src.open();
		// `side work` is merged in through the second parent only.
		let side = src.git(&["rev-parse", "side"]);
		let err = select_range(&g, &side, "HEAD").unwrap_err();
		match &err {
			CommitError::Discontinuous {
				at, first_parent, ..
			} => {
				let main_work = src.git(&["rev-parse", &format!("{merge}^1")]);
				assert_eq!(at, &main_work);
				let base = src.git(&["rev-parse", &format!("{merge}^1^")]);
				assert_eq!(first_parent.as_deref(), Some(base.as_str()));
			}
			e => panic!("{e}"),
		}
		assert!(err.to_string().contains("not contiguous"));
		assert!(matches!(
			select_range(&g, "HEAD", "HEAD"),
			Err(CommitError::Empty)
		));
		assert!(matches!(
			select_last(&g, 99),
			Err(CommitError::NotEnoughCommits { available: 5, .. })
		));
	}

	#[test]
	fn commits_payload_detection_and_errors() {
		assert!(is_commit_payload("// snip-sync commits v1\r\n{}"));
		assert!(!is_commit_payload("// snip-sync commits v2\n{}"));
		assert!(matches!(
			parse_commit_payload("// snip-sync commits v1\n{bad"),
			Err(CommitError::InvalidPayload(_))
		));
		assert!(matches!(
			parse_commit_payload("x"),
			Err(CommitError::NotCommitPayload)
		));
	}

	#[test]
	fn commits_unsafe_paths_are_skipped_and_replay_stops_on_failure() {
		let dst = Repo::new("main");
		dst.write("a.txt", b"a\n");
		dst.commit("root", "2019-01-01T00:00:00+00:00");
		let file = |path: &str| CommitFile {
			path: path.into(),
			old_path: None,
			change: FileChange::Added,
			content: Some("x\n".into()),
			not_copied: None,
		};
		let record = |msg: &str, files| CommitRecord {
			message: msg.into(),
			author_name: "Bob".into(),
			author_email: "bob@example.com".into(),
			author_date: "2020-01-01T00:00:00+00:00".into(),
			files,
		};
		let payload = CommitsPayload {
			commits: vec![
				record(
					"ok\n",
					vec![
						file("../escape.txt"),
						file("/abs.txt"),
						file("b.txt"),
					],
				),
				record("bad date\n", vec![file("c.txt")]),
				record("never\n", vec![file("d.txt")]),
			],
		};
		let mut payload = payload;
		payload.commits[1].author_date = "not a date".into();
		let g = dst.open();
		let plan = plan_commit_replay(&g, &payload);
		let skips: Vec<_> = plan.commits[0]
			.files
			.iter()
			.map(|f| f.skip_reason)
			.collect();
		assert_eq!(
			skips,
			vec![
				Some(ReplaySkipReason::UnsafePath),
				Some(ReplaySkipReason::UnsafePath),
				None
			]
		);
		let result = replay(&g, &payload);
		assert_eq!(result.created.len(), 1);
		let failure = result.failure.unwrap();
		assert_eq!(failure.index, 1);
		assert!(failure.error.contains("commit"), "{}", failure.error);
		assert_eq!(failure.layout_conflict, None);
		assert_eq!(failure.conflict_path, None);
		assert_eq!(plan.commits[0].refused_by(), None);
		assert_eq!(plan.commits[0].files[0].layout_conflict, None);
		assert_eq!(plan.commits[0].files[1].layout_conflict, None);
		assert!(!dst.dir.path().join("escape.txt").exists());
		let tree =
			dst.git(&["ls-tree", "-r", "--name-only", &result.created[0]]);
		assert_eq!(tree, "a.txt\nb.txt");
	}

	#[test]
	fn commits_git_path_and_old_path_are_skipped_as_unsafe_path() {
		let dst = Repo::new("main");
		dst.write("a.txt", b"a\n");
		dst.commit("root", "2019-01-01T00:00:00+00:00");
		let git_config = dst.path().join(".git").join("config");
		assert!(git_config.exists());
		let original_config = fs::read(&git_config).unwrap();

		let file = |path: &str, old_path: Option<&str>, change: FileChange| {
			CommitFile {
				path: path.into(),
				old_path: old_path.map(str::to_string),
				change,
				content: Some("data\n".into()),
				not_copied: None,
			}
		};
		let record = |msg: &str, files| CommitRecord {
			message: msg.into(),
			author_name: "Bob".into(),
			author_email: "bob@example.com".into(),
			author_date: "2020-01-01T00:00:00+00:00".into(),
			files,
		};
		let payload = CommitsPayload {
			commits: vec![record(
				"git paths\n",
				vec![
					file(".git/evil.txt", None, FileChange::Added),
					file("dest.txt", Some(".git/config"), FileChange::Renamed),
					file("safe.txt", None, FileChange::Added),
				],
			)],
		};

		let g = dst.open();
		let plan = plan_commit_replay(&g, &payload);
		let skips: Vec<_> = plan.commits[0]
			.files
			.iter()
			.map(|f| f.skip_reason)
			.collect();
		assert_eq!(
			skips,
			vec![
				Some(ReplaySkipReason::UnsafePath),
				Some(ReplaySkipReason::UnsafePath),
				None,
			]
		);

		let result = replay(&g, &payload);
		assert_eq!(result.created.len(), 1);
		assert_eq!(result.failure, None);

		// Nothing is written or removed under .git
		assert!(!dst.path().join(".git").join("evil.txt").exists());
		assert!(git_config.exists());
		assert_eq!(fs::read(&git_config).unwrap(), original_config);

		// Safe file is committed
		let tree =
			dst.git(&["ls-tree", "-r", "--name-only", &result.created[0]]);
		assert_eq!(tree, "a.txt\nsafe.txt");
	}

	#[test]
	fn commit_plan_detects_layout_conflicts_and_distinguishes_path_unsafe() {
		let dst = Repo::new("main");
		dst.write("blocker_file", b"regular file\n");
		fs::create_dir_all(dst.path().join("existing_dir")).unwrap();
		dst.write("existing_dir/keep.txt", b"keep\n");
		dst.commit("base", "2019-01-01T00:00:00+00:00");

		let file = |path: &str| CommitFile {
			path: path.into(),
			old_path: None,
			change: FileChange::Added,
			content: Some("x\n".into()),
			not_copied: None,
		};
		let payload = CommitsPayload {
			commits: vec![
				CommitRecord {
					message: "layout conflicts\n".into(),
					author_name: "Author".into(),
					author_email: "author@example.com".into(),
					author_date: "2020-01-01T00:00:00+00:00".into(),
					files: vec![
						file("blocker_file/child.txt"),
						file("existing_dir"),
						file("normal.txt"),
					],
				},
				CommitRecord {
					message: "path rule unsafe\n".into(),
					author_name: "Author".into(),
					author_email: "author@example.com".into(),
					author_date: "2020-01-01T00:00:00+00:00".into(),
					files: vec![file("../escape.txt"), file("fine.txt")],
				},
			],
		};
		let g = dst.open();
		let plan = plan_commit_replay(&g, &payload);

		// Layout conflict commit
		let c0 = &plan.commits[0];
		assert_eq!(
			c0.files[0].layout_conflict,
			Some(LayoutConflict::FileInTheWayOfParent)
		);
		assert_eq!(c0.files[0].skip_reason, Some(ReplaySkipReason::UnsafePath));
		assert_eq!(
			c0.files[1].layout_conflict,
			Some(LayoutConflict::DirectoryInTheWay)
		);
		assert_eq!(c0.files[1].skip_reason, Some(ReplaySkipReason::UnsafePath));
		assert_eq!(c0.files[2].layout_conflict, None);
		assert_eq!(c0.files[2].skip_reason, None);
		assert_eq!(c0.refused_by(), Some(LayoutConflict::FileInTheWayOfParent));

		// Path-rule unsafe commit
		let c1 = &plan.commits[1];
		assert_eq!(c1.files[0].skip_reason, Some(ReplaySkipReason::UnsafePath));
		assert_eq!(c1.files[0].layout_conflict, None);
		assert_eq!(c1.files[1].skip_reason, None);
		assert_eq!(c1.files[1].layout_conflict, None);
		assert_eq!(c1.refused_by(), None);
	}

	#[test]
	fn commit_replay_layout_conflict_refuses_with_structured_kind() {
		let dst = Repo::new("main");
		dst.write("blocker_file", b"regular file\n");
		dst.commit("base", "2019-01-01T00:00:00+00:00");

		let file = |path: &str| CommitFile {
			path: path.into(),
			old_path: None,
			change: FileChange::Added,
			content: Some("x\n".into()),
			not_copied: None,
		};
		let payload = CommitsPayload {
			commits: vec![CommitRecord {
				message: "incoming\n".into(),
				author_name: "Author".into(),
				author_email: "author@example.com".into(),
				author_date: "2020-01-01T00:00:00+00:00".into(),
				files: vec![file("blocker_file/x.txt"), file("fresh.txt")],
			}],
		};
		let g = dst.open();
		let result = replay(&g, &payload);

		assert!(result.created.is_empty());
		let failure =
			result.failure.expect("replay must fail on layout conflict");
		assert_eq!(failure.index, 0);
		assert_eq!(
			failure.layout_conflict,
			Some(LayoutConflict::FileInTheWayOfParent)
		);
		assert_eq!(
			failure.conflict_path.as_deref(),
			Some("blocker_file/x.txt")
		);
		assert_eq!(
			failure.error,
			"blocker_file/x.txt: a file is in the way of its parent directory"
		);
		assert_eq!(
			fs::read_to_string(dst.path().join("blocker_file")).unwrap(),
			"regular file\n"
		);
		assert!(!dst.path().join("fresh.txt").exists());
	}

	fn batch_file(path: &str, change: FileChange) -> CommitFile {
		CommitFile {
			path: path.into(),
			old_path: None,
			change,
			content: (change != FileChange::Deleted).then(|| "x\n".into()),
			not_copied: None,
		}
	}

	fn batch_commit(message: &str, files: Vec<CommitFile>) -> CommitRecord {
		CommitRecord {
			message: format!("{message}\n"),
			author_name: "Author".into(),
			author_email: "author@example.com".into(),
			author_date: "2020-01-01T00:00:00+00:00".into(),
			files,
		}
	}

	#[test]
	fn batch_plan_sees_blocker_removed_by_an_earlier_commit() {
		let dst = Repo::new("main");
		dst.write("newdir", b"regular file\n");
		dst.commit("base", "2019-01-01T00:00:00+00:00");
		let payload = CommitsPayload {
			commits: vec![
				batch_commit(
					"remove blocker",
					vec![batch_file("newdir", FileChange::Deleted)],
				),
				batch_commit(
					"write under it",
					vec![batch_file("newdir/x.txt", FileChange::Added)],
				),
			],
		};
		let g = dst.open();
		let plan = plan_commit_replay(&g, &payload);
		assert_eq!(plan.commits[1].refused_by(), None);
		assert_eq!(plan.commits[1].files[0].action, ReplayAction::Write);
		let result = replay(&g, &payload);
		assert_eq!(result.created.len(), 2);
		assert_eq!(result.failure, None);
	}

	#[test]
	fn batch_plan_refuses_commit_whose_parent_an_earlier_commit_makes_a_file() {
		let dst = Repo::new("main");
		dst.write("keep.txt", b"keep\n");
		dst.commit("base", "2019-01-01T00:00:00+00:00");
		let payload = CommitsPayload {
			commits: vec![
				batch_commit(
					"create blocker",
					vec![batch_file("newdir", FileChange::Added)],
				),
				batch_commit(
					"write under it",
					vec![batch_file("newdir/x.txt", FileChange::Added)],
				),
			],
		};
		let g = dst.open();
		let plan = plan_commit_replay(&g, &payload);
		assert_eq!(plan.commits[0].refused_by(), None);
		assert_eq!(
			plan.commits[1].refused_by(),
			Some(LayoutConflict::FileInTheWayOfParent)
		);
		let result = replay(&g, &payload);
		assert_eq!(result.created.len(), 1);
		let failure = result.failure.expect("commit 2 must be refused");
		assert_eq!(failure.index, 1);
		assert_eq!(
			failure.layout_conflict,
			Some(LayoutConflict::FileInTheWayOfParent)
		);
	}

	#[test]
	fn batch_plan_deletes_a_file_an_earlier_commit_adds() {
		let dst = Repo::new("main");
		dst.write("keep.txt", b"keep\n");
		dst.commit("base", "2019-01-01T00:00:00+00:00");
		let payload = CommitsPayload {
			commits: vec![
				batch_commit(
					"add",
					vec![batch_file("a.txt", FileChange::Added)],
				),
				batch_commit(
					"delete",
					vec![batch_file("a.txt", FileChange::Deleted)],
				),
			],
		};
		let g = dst.open();
		let plan = plan_commit_replay(&g, &payload);
		let del = &plan.commits[1].files[0];
		assert_eq!(del.action, ReplayAction::Delete);
		assert!(del.existed, "the delete removes what commit 1 writes");
		let result = replay(&g, &payload);
		assert_eq!(result.created.len(), 2);
		assert!(!dst.path().join("a.txt").exists());
	}

	#[test]
	fn batch_plan_directory_emptied_by_an_earlier_commit_no_longer_blocks() {
		let dst = Repo::new("main");
		fs::create_dir_all(dst.path().join("d")).unwrap();
		dst.write("d/f.txt", b"f\n");
		dst.commit("base", "2019-01-01T00:00:00+00:00");
		let payload = CommitsPayload {
			commits: vec![
				batch_commit(
					"empty the dir",
					vec![batch_file("d/f.txt", FileChange::Deleted)],
				),
				batch_commit(
					"file at its path",
					vec![batch_file("d", FileChange::Added)],
				),
			],
		};
		let g = dst.open();
		let plan = plan_commit_replay(&g, &payload);
		assert_eq!(plan.commits[1].refused_by(), None);
		assert_eq!(plan.commits[1].files[0].layout_conflict, None);
		// The real path captures a freshness snapshot first, which meets the
		// directory still standing at `d`.
		let preview = crate::transfer::CommitReplayPreview::capture(
			&dst.path(),
			&payload,
		)
		.expect("a directory at a replay target is not a special file");
		let result = preview.apply().expect("preview stays fresh");
		assert_eq!(result.failure, None);
		assert_eq!(result.created.len(), 2);
		assert!(dst.path().join("d").is_file());
	}

	#[test]
	fn batch_plan_keeps_overwrite_consent_on_the_real_disk() {
		let dst = Repo::new("main");
		dst.write("a.txt", b"old\n");
		dst.commit("base", "2019-01-01T00:00:00+00:00");
		let payload = CommitsPayload {
			commits: vec![
				batch_commit(
					"delete",
					vec![batch_file("a.txt", FileChange::Deleted)],
				),
				batch_commit(
					"re-add",
					vec![batch_file("a.txt", FileChange::Added)],
				),
			],
		};
		let plan = plan_commit_replay(&dst.open(), &payload);
		assert!(plan.commits[0].files[0].existed);
		assert!(
			plan.commits[1].files[0].existed,
			"a file on disk before the replay still needs overwrite consent"
		);
	}

	#[test]
	fn batch_plan_rename_of_a_file_an_earlier_commit_adds_removes_it() {
		let dst = Repo::new("main");
		dst.write("keep.txt", b"keep\n");
		dst.commit("base", "2019-01-01T00:00:00+00:00");
		let mut rename = batch_file("b.txt", FileChange::Renamed);
		rename.old_path = Some("a.txt".into());
		let payload = CommitsPayload {
			commits: vec![
				batch_commit(
					"add",
					vec![batch_file("a.txt", FileChange::Added)],
				),
				batch_commit("rename", vec![rename]),
			],
		};
		let g = dst.open();
		let plan = plan_commit_replay(&g, &payload);
		assert!(
			plan.commits[1].files[0].old_existed,
			"the rename deletes what commit 1 writes"
		);
		assert!(!plan.commits[0].files[0].old_existed);
		let result = replay(&g, &payload);
		assert_eq!(result.created.len(), 2);
		assert!(!dst.path().join("a.txt").exists());
		assert!(dst.path().join("b.txt").is_file());
	}

	#[test]
	fn batch_plan_sees_blocker_renamed_away_by_an_earlier_commit() {
		let dst = Repo::new("main");
		dst.write("newdir", b"regular file\n");
		dst.commit("base", "2019-01-01T00:00:00+00:00");
		let mut rename = batch_file("moved.txt", FileChange::Renamed);
		rename.old_path = Some("newdir".into());
		let payload = CommitsPayload {
			commits: vec![
				batch_commit("move it away", vec![rename]),
				batch_commit(
					"write under it",
					vec![batch_file("newdir/x.txt", FileChange::Added)],
				),
			],
		};
		let g = dst.open();
		let plan = plan_commit_replay(&g, &payload);
		assert_eq!(plan.commits[1].refused_by(), None);
		let result = replay(&g, &payload);
		assert_eq!(result.failure, None);
		assert_eq!(result.created.len(), 2);
		assert!(dst.path().join("newdir/x.txt").is_file());
	}

	#[test]
	fn batch_plan_rewrites_a_non_utf8_file_an_earlier_commit_deleted() {
		let dst = Repo::new("main");
		dst.write("bad.txt", &[0xff, 0xfe, 0xfd]);
		dst.commit("base", "2019-01-01T00:00:00+00:00");
		let payload = CommitsPayload {
			commits: vec![
				batch_commit(
					"delete",
					vec![batch_file("bad.txt", FileChange::Deleted)],
				),
				batch_commit(
					"re-add",
					vec![batch_file("bad.txt", FileChange::Added)],
				),
			],
		};
		let g = dst.open();
		let plan = plan_commit_replay(&g, &payload);
		assert_eq!(plan.commits[1].files[0].skip_reason, None);
		assert_eq!(plan.commits[1].files[0].action, ReplayAction::Write);
		let result = replay(&g, &payload);
		assert_eq!(result.created.len(), 2);
		assert_eq!(fs::read(dst.path().join("bad.txt")).unwrap(), b"x\n");
	}

	#[cfg(unix)]
	#[test]
	fn batch_plan_writes_under_a_symlink_an_earlier_commit_deleted() {
		let dst = Repo::new("main");
		fs::create_dir(dst.path().join("other")).unwrap();
		dst.write("other/keep.txt", b"keep\n");
		std::os::unix::fs::symlink("other", dst.path().join("link")).unwrap();
		dst.commit("base", "2019-01-01T00:00:00+00:00");
		let payload = CommitsPayload {
			commits: vec![
				batch_commit(
					"delete the link",
					vec![batch_file("link", FileChange::Deleted)],
				),
				batch_commit(
					"write under it",
					vec![batch_file("link/x.txt", FileChange::Added)],
				),
			],
		};
		let g = dst.open();
		let plan = plan_commit_replay(&g, &payload);
		assert_eq!(plan.commits[1].files[0].skip_reason, None);
		assert_eq!(plan.commits[1].files[0].action, ReplayAction::Write);
		let result = replay(&g, &payload);
		assert_eq!(result.failure, None);
		assert_eq!(result.created.len(), 2);
		assert!(dst.path().join("link/x.txt").is_file());
		assert!(!dst.path().join("other/x.txt").exists());
	}

	#[test]
	fn batch_plan_walks_a_large_deletion_in_linear_time() {
		let dst = Repo::new("main");
		let n = 3000;
		fs::create_dir(dst.path().join("big")).unwrap();
		for i in 0..n {
			dst.write(&format!("big/f{i}.txt"), b"x\n");
		}
		dst.commit("base", "2019-01-01T00:00:00+00:00");
		let files = (0..n)
			.map(|i| batch_file(&format!("big/f{i}.txt"), FileChange::Deleted))
			.collect();
		let payload = CommitsPayload {
			commits: vec![
				batch_commit("delete all", files),
				batch_commit(
					"then add",
					vec![batch_file("big", FileChange::Added)],
				),
			],
		};
		let g = dst.open();
		let started = std::time::Instant::now();
		let plan = plan_commit_replay(&g, &payload);
		assert_eq!(plan.commits[1].refused_by(), None, "big is emptied");
		assert!(
			started.elapsed() < std::time::Duration::from_secs(10),
			"planning took {:?}",
			started.elapsed()
		);
	}

	#[test]
	fn commits_empty_commit_keeps_index_and_untracked_delete_is_fine() {
		let dst = Repo::new("main");
		dst.write("a.txt", b"a\n");
		dst.commit("root", "2019-01-01T00:00:00+00:00");
		let before = dst.git(&["rev-parse", "HEAD^{tree}"]);
		dst.write("foreign.txt", b"staged\n");
		dst.git(&["add", "foreign.txt"]);
		// Untracked at the target; the source deletes it.
		dst.write("gone.txt", b"untracked\n");
		let record = |msg: &str, file: CommitFile| CommitRecord {
			message: msg.into(),
			author_name: "Bob".into(),
			author_email: "bob@example.com".into(),
			author_date: "2020-01-01T00:00:00+00:00".into(),
			files: vec![file],
		};
		let file = |path: &str, change, not_copied| CommitFile {
			path: path.into(),
			old_path: None,
			change,
			content: None,
			not_copied,
		};
		let payload = CommitsPayload {
			commits: vec![
				record(
					"only binary\n",
					file(
						"img.bin",
						FileChange::Added,
						Some(NotCopiedReason::Binary),
					),
				),
				record(
					"delete untracked\n",
					file("gone.txt", FileChange::Deleted, None),
				),
			],
		};
		let result = replay(&dst.open(), &payload);
		assert_eq!(result.failure, None);
		assert_eq!(result.created.len(), 2);
		for created in &result.created {
			let tree = dst.git(&["rev-parse", &format!("{created}^{{tree}}")]);
			assert_eq!(tree, before);
		}
		assert!(!dst.path().join("gone.txt").exists());
		assert_eq!(
			dst.git(&["diff", "--cached", "--name-only"]),
			"foreign.txt"
		);
	}

	#[cfg(unix)]
	#[test]
	fn commits_replay_replaces_symlink_dir_and_keeps_staged_new_file() {
		let dst = Repo::new("main");
		dst.write("real.txt", b"real\n");
		dst.write("a/b", b"nested\n");
		std::os::unix::fs::symlink("real.txt", dst.path().join("l")).unwrap();
		dst.commit("root", "2019-01-01T00:00:00+00:00");
		// A new file only in the index; the replayed commit deletes it.
		dst.write("f", b"staged new\n");
		dst.git(&["add", "f"]);
		let file = |path: &str, change, content: Option<&str>| CommitFile {
			path: path.into(),
			old_path: None,
			change,
			content: content.map(Into::into),
			not_copied: None,
		};
		let payload = CommitsPayload {
			commits: vec![CommitRecord {
				message: "mixed\n".into(),
				author_name: "Bob".into(),
				author_email: "bob@example.com".into(),
				author_date: "2020-01-01T00:00:00+00:00".into(),
				files: vec![
					file("a/b", FileChange::Deleted, None),
					file("a", FileChange::Added, Some("now a file\n")),
					file("l", FileChange::Modified, Some("now regular\n")),
					file("f", FileChange::Deleted, None),
				],
			}],
		};
		let result = replay(&dst.open(), &payload);
		assert_eq!(result.failure, None);
		assert_eq!(result.created.len(), 1);
		let tree = dst.git(&["ls-tree", "-r", "HEAD"]);
		assert!(tree.contains("100644 blob"), "{tree}");
		assert!(!tree.contains("120000"), "{tree}");
		assert_eq!(dst.git(&["show", "HEAD:a"]), "now a file");
		assert_eq!(dst.git(&["show", "HEAD:l"]), "now regular");
		assert_eq!(
			fs::read_to_string(dst.path().join("real.txt")).unwrap(),
			"real\n"
		);
		// `f` never reached HEAD, so it stays staged and out of the commit.
		assert_eq!(dst.git(&["diff", "--cached", "--name-only"]), "f");
		let names = dst.git(&["ls-tree", "-r", "--name-only", "HEAD"]);
		assert_eq!(names, "a\nl\nreal.txt");
	}

	#[cfg(unix)]
	#[test]
	fn commits_replay_refuses_paths_through_a_symlinked_directory() {
		let dst = Repo::new("main");
		dst.write("other/x.txt", b"x\n");
		dst.write("other/y.txt", b"y\n");
		std::os::unix::fs::symlink("other", dst.path().join("d")).unwrap();
		dst.commit("root", "2019-01-01T00:00:00+00:00");
		let file = |path: &str, change, content: Option<&str>| CommitFile {
			path: path.into(),
			old_path: None,
			change,
			content: content.map(Into::into),
			not_copied: None,
		};
		let payload = CommitsPayload {
			commits: vec![CommitRecord {
				message: "through link\n".into(),
				author_name: "Bob".into(),
				author_email: "bob@example.com".into(),
				author_date: "2020-01-01T00:00:00+00:00".into(),
				files: vec![
					file("d/y.txt", FileChange::Deleted, None),
					file("d/x.txt", FileChange::Modified, Some("NEW\n")),
				],
			}],
		};
		let plan = plan_commit_replay(&dst.open(), &payload);
		assert!(plan.commits[0]
			.files
			.iter()
			.all(|f| f.action == ReplayAction::Skip));
		replay(&dst.open(), &payload);
		// The real files behind the link are untouched.
		assert_eq!(
			fs::read_to_string(dst.path().join("other/x.txt")).unwrap(),
			"x\n"
		);
		assert!(dst.path().join("other/y.txt").exists());
	}

	#[test]
	fn binary_delete_is_not_copied_and_replay_keeps_the_target_file() {
		let src = Repo::new("main");
		src.write("bin.dat", &[0x89, b'P', 0, 1, 2]);
		src.write("big5.txt", &[0xa4, 0xe9, 0xa5, 0xbb]);
		src.write("empty.txt", b"");
		src.write("gone.txt", b"to be deleted\n");
		src.write("big.txt", "x".repeat(100_000).as_bytes());
		src.write("keep.txt", b"keep\n");
		src.commit("base", "2020-01-01T00:00:00+00:00");
		for f in ["bin.dat", "big5.txt", "empty.txt", "gone.txt", "big.txt"] {
			src.git(&["rm", "-q", f]);
		}
		src.write("keep.txt", b"keep v2\n");
		let sha = src.commit("delete", "2020-01-02T00:00:00+00:00");

		let payload = copy_commits(&src.open(), &[sha]).unwrap();
		let files = &payload.commits[0].files;
		let find = |p: &str| files.iter().find(|f| f.path == p).unwrap();
		for (path, reason) in [
			("bin.dat", Some(NotCopiedReason::Binary)),
			("big5.txt", Some(NotCopiedReason::NonUtf8)),
			("empty.txt", None),
			("gone.txt", None),
			("big.txt", None),
		] {
			let f = find(path);
			assert_eq!(f.change, FileChange::Deleted, "{path}");
			assert_eq!(f.not_copied, reason, "{path}");
			assert_eq!(f.content, None, "{path}");
		}

		// Through the clipboard text, as paste sees it.
		let text = to_clipboard_text(&payload);
		let parsed = parse_commit_payload(&text).unwrap();
		let dst = Repo::new("feature");
		for f in ["bin.dat", "big5.txt", "empty.txt", "gone.txt", "big.txt"] {
			dst.write(f, b"target \0 copy\n");
		}
		dst.write("keep.txt", b"keep\n");
		dst.commit("target root", "2019-01-01T00:00:00+00:00");
		let tg = dst.open();

		let plan = plan_commit_replay(&tg, &parsed);
		let pf = |p: &str| {
			plan.commits[0].files.iter().find(|f| f.path == p).unwrap()
		};
		for p in ["bin.dat", "big5.txt"] {
			assert_eq!(pf(p).action, ReplayAction::Skip, "{p}");
			assert_eq!(
				pf(p).skip_reason,
				Some(ReplaySkipReason::NotCopied),
				"{p}"
			);
		}
		for p in ["empty.txt", "gone.txt", "big.txt"] {
			assert_eq!(pf(p).action, ReplayAction::Delete, "{p}");
		}

		let result = replay(&tg, &parsed);
		assert_eq!(result.failure, None);
		assert_eq!(result.created.len(), 1);
		assert!(dst.path().join("bin.dat").exists());
		assert!(dst.path().join("big5.txt").exists());
		for p in ["empty.txt", "gone.txt", "big.txt"] {
			assert!(!dst.path().join(p).exists(), "{p}");
		}
		assert_eq!(
			dst.git(&["ls-tree", "-r", "--name-only", "HEAD"]),
			"big5.txt\nbin.dat\nkeep.txt"
		);
		assert_eq!(dst.git(&["show", "HEAD:keep.txt"]), "keep v2");
	}

	#[test]
	fn wire_len_matches_clipboard_text_for_escapes() {
		let file = |path: &str, content: Option<&str>, reason| CommitFile {
			path: path.into(),
			old_path: Some("old \"name\".txt".into()),
			change: FileChange::Renamed,
			content: content.map(str::to_string),
			not_copied: reason,
		};
		let payload = CommitsPayload {
			commits: vec![
				CommitRecord {
					message: "say \"hi\"\nline\\two\n".into(),
					author_name: "Alice \"A\"".into(),
					author_email: "a@ex.com".into(),
					author_date: "2024-03-04T05:06:07+08:00".into(),
					files: vec![
						file("你好.txt", Some("\"q\"\n\\p\n\u{1}😀"), None),
						file("bin", None, Some(NotCopiedReason::Binary)),
					],
				},
				CommitRecord {
					message: String::new(),
					author_name: String::new(),
					author_email: String::new(),
					author_date: String::new(),
					files: Vec::new(),
				},
			],
		};
		let text = to_clipboard_text(&payload);
		assert_eq!(wire_len(&payload.commits), text.len());
		assert_eq!(parse_commit_payload(&text).unwrap(), payload);
		assert!(text.contains("\\u0001"));
		assert!(text.starts_with("// snip-sync commits v1\n{"));
	}

	#[test]
	fn min_commit_document_len_matches_empty_commits() {
		let empty = CommitRecord {
			message: String::new(),
			author_name: String::new(),
			author_email: String::new(),
			author_date: String::new(),
			files: Vec::new(),
		};
		for n in 0..4 {
			let payload = CommitsPayload {
				commits: vec![empty.clone(); n],
			};
			assert_eq!(
				min_commit_document_len(n),
				to_clipboard_text(&payload).len(),
				"n={n}"
			);
		}
	}

	#[test]
	fn bounded_writer_matches_to_clipboard_text() {
		let payload = CommitsPayload {
			commits: vec![CommitRecord {
				message: "test message\n".into(),
				author_name: "Alice \"Special\"".into(),
				author_email: "alice@example.com".into(),
				author_date: "2024-01-01T00:00:00Z".into(),
				files: vec![CommitFile {
					path: "test.txt".into(),
					old_path: None,
					change: FileChange::Added,
					content: Some("hello \"world\"\n\\escaped\n".into()),
					not_copied: None,
				}],
			}],
		};
		let text = to_clipboard_text(&payload);
		let bounded =
			to_clipboard_text_bounded(&payload, text.len(), text.len(), None)
				.unwrap();
		assert_eq!(bounded, text);

		let err = to_clipboard_text_bounded(
			&payload,
			text.len(),
			text.len() - 1,
			None,
		)
		.unwrap_err();
		match err {
			CommitError::PayloadLimit { limit, actual } => {
				assert_eq!(limit, text.len() - 1);
				assert_eq!(actual, text.len());
			}
			other => panic!("expected PayloadLimit, got {other}"),
		}
	}

	#[test]
	fn mid_count_cancellation_aborts_immediately_without_looping() {
		let token = CancelToken::new();
		let file = CommitFile {
			path: "foo.txt".to_string(),
			old_path: None,
			change: FileChange::Added,
			content: Some("a".repeat(1000)),
			not_copied: None,
		};
		let (tx, rx) = std::sync::mpsc::channel();
		let token_clone = token.clone();
		let handle = std::thread::spawn(move || {
			let _guard = set_test_hook(TestHookState {
				token: Some(token_clone.clone()),
				writes_before_cancel: 2,
				..TestHookState::default()
			});
			assert!(
				!token_clone.is_cancelled(),
				"token must be uncancelled at entry guard"
			);
			let res = measure_file(&file, Some(&token_clone));
			let state = get_test_hook_state();
			let _ = tx.send((res, state));
		});
		let (result, hook_state) =
			rx.recv_timeout(Duration::from_secs(2)).expect(
				"timed out: old Write::write_all retried Interrupted forever",
			);
		let _ = handle.join();

		assert!(
			hook_state.writer_hook_triggered,
			"target writer hook must have triggered"
		);
		assert!(
			hook_state.writes_completed >= 2,
			"must have completed at least 2 writes before cancel"
		);
		assert!(
			hook_state.bytes_before_cancel > 0,
			"must have accepted bytes before cancel"
		);
		match result {
			Err(CommitError::Git(GitError::Cancelled { args })) => {
				assert!(
					args.contains("file serialize"),
					"unexpected args: {args}"
				);
			}
			other => panic!("expected Cancelled, got {other:?}"),
		}
	}

	#[test]
	fn mid_final_serialization_cancellation_aborts_immediately_without_looping()
	{
		let token = CancelToken::new();
		let payload = CommitsPayload {
			commits: vec![CommitRecord {
				message: "test message\n".to_string(),
				author_name: "Ada".to_string(),
				author_email: "ada@example.com".to_string(),
				author_date: "2024-01-01T00:00:00Z".to_string(),
				files: vec![CommitFile {
					path: "foo.txt".to_string(),
					old_path: None,
					change: FileChange::Added,
					content: Some("hello world".to_string()),
					not_copied: None,
				}],
			}],
		};
		let (tx, rx) = std::sync::mpsc::channel();
		let token_clone = token.clone();
		let handle = std::thread::spawn(move || {
			let _guard = set_test_hook(TestHookState {
				token: Some(token_clone.clone()),
				writes_before_cancel: 2,
				..TestHookState::default()
			});
			assert!(
				!token_clone.is_cancelled(),
				"token must be uncancelled at entry guard"
			);
			let res = to_clipboard_text_bounded(
				&payload,
				500,
				1000,
				Some(&token_clone),
			);
			let state = get_test_hook_state();
			let _ = tx.send((res, state));
		});
		let (result, hook_state) =
			rx.recv_timeout(Duration::from_secs(2)).expect(
				"timed out: old Write::write_all retried Interrupted forever",
			);
		let _ = handle.join();

		assert!(
			hook_state.writer_hook_triggered,
			"target writer hook must have triggered"
		);
		assert!(
			hook_state.writes_completed >= 2,
			"must have completed at least 2 writes before cancel"
		);
		assert!(
			hook_state.bytes_before_cancel > 0,
			"must have accepted bytes before cancel"
		);
		match result {
			Err(CommitError::Git(GitError::Cancelled { args })) => {
				assert!(args.contains("serialize"), "unexpected args: {args}");
			}
			other => panic!("expected Cancelled, got {other:?}"),
		}
	}

	#[test]
	fn no_blob_last_commit_post_diff_cancellation_returns_cancelled() {
		let repo = Repo::new("main");
		repo.write("a.txt", b"hello world\n");
		let _sha1 = repo.commit("add a.txt", "2024-01-01T00:00:00Z");
		repo.cmd(&["rm", "a.txt"]).output().unwrap();
		let sha2 = repo.commit("delete a.txt", "2024-01-02T00:00:00Z");
		let git = repo.open();

		let token = CancelToken::new();
		let (tx, rx) = std::sync::mpsc::channel();
		let token_clone = token.clone();
		let handle = std::thread::spawn(move || {
			let _guard = set_test_hook(TestHookState {
				token: Some(token_clone.clone()),
				cancel_post_diff: true,
				..TestHookState::default()
			});
			assert!(
				!token_clone.is_cancelled(),
				"token must be uncancelled at start"
			);
			let opts = RunOptions {
				cancel: Some(token_clone),
				..RunOptions::default()
			};
			let res = copy_commits_with(&git, &[sha2], &opts, usize::MAX);
			let state = get_test_hook_state();
			let _ = tx.send((res, state));
		});
		let (res, hook_state) = rx
			.recv_timeout(Duration::from_secs(5))
			.expect("worker timed out");
		let _ = handle.join();

		assert!(
			hook_state.post_diff_hook_reached,
			"post-diff hook must have been reached"
		);
		match res {
			Err(CommitError::Git(GitError::Cancelled { .. })) => {}
			Ok(_) => panic!("expected cancellation error, but export succeeded on cancelled deletion commit"),
			Err(other) => panic!("expected Cancelled error, got: {other}"),
		}
	}

	#[test]
	fn many_files_scaling_proves_linear_serde_counting_not_quadratic() {
		// Test with 40 files
		let repo1 = Repo::new("main");
		for i in 0..40 {
			repo1.write(
				&format!("file_{i:03}.txt"),
				format!("content for file {i}\n").as_bytes(),
			);
		}
		let sha1 = repo1.commit("40 files", "2024-01-01T00:00:00Z");
		let git1 = repo1.open();
		reset_serde_bytes_counted();
		let export1 = copy_commits_with(
			&git1,
			&[sha1],
			&RunOptions::default(),
			usize::MAX,
		)
		.unwrap();
		let counted1 = get_serde_bytes_counted();
		let len1 = export1.text.len();

		// Test with 80 files
		let repo2 = Repo::new("main");
		for i in 0..80 {
			repo2.write(
				&format!("file_{i:03}.txt"),
				format!("content for file {i}\n").as_bytes(),
			);
		}
		let sha2 = repo2.commit("80 files", "2024-01-01T00:00:00Z");
		let git2 = repo2.open();
		reset_serde_bytes_counted();
		let export2 = copy_commits_with(
			&git2,
			&[sha2],
			&RunOptions::default(),
			usize::MAX,
		)
		.unwrap();
		let counted2 = get_serde_bytes_counted();
		let len2 = export2.text.len();

		assert!(
			counted1 < len1 * 3,
			"counted1 {counted1} exceeded 3x text length {len1}"
		);
		assert!(
			counted2 < len2 * 3,
			"counted2 {counted2} exceeded 3x text length {len2}"
		);
		let scaling_ratio = (counted2 as f64) / (counted1 as f64);
		let size_ratio = (len2 as f64) / (len1 as f64);
		eprintln!(
			"Scaling metrics: 40 files -> len={len1}, counted={counted1}; 80 files -> len={len2}, counted={counted2}; size_ratio={size_ratio:.2}, scaling_ratio={scaling_ratio:.2}"
		);
		assert!(
			scaling_ratio < size_ratio * 1.5,
			"scaling ratio {scaling_ratio:.2} indicates super-linear/quadratic growth (size ratio {size_ratio:.2})"
		);
	}

	fn bob(message: &str, files: Vec<CommitFile>) -> CommitsPayload {
		CommitsPayload {
			commits: vec![CommitRecord {
				message: message.into(),
				author_name: "Bob".into(),
				author_email: "bob@example.com".into(),
				author_date: "2020-01-01T00:00:00+00:00".into(),
				files,
			}],
		}
	}

	fn change(
		path: &str,
		change: FileChange,
		content: Option<&str>,
	) -> CommitFile {
		CommitFile {
			path: path.into(),
			old_path: None,
			change,
			content: content.map(Into::into),
			not_copied: None,
		}
	}

	#[cfg(unix)]
	#[test]
	fn commits_replay_runs_no_hooks() {
		use std::os::unix::fs::PermissionsExt;
		let dst = Repo::new("main");
		dst.write("a.txt", b"a\n");
		dst.commit("root", "2019-01-01T00:00:00+00:00");
		let hooks = dst.path().join(".git/hooks");
		let marker = dst.dir.path().join("hook-ran");
		for (name, body) in [
			(
				"prepare-commit-msg",
				"echo rewritten > \"$1\"\n".to_string(),
			),
			("post-commit", format!("touch '{}'\n", marker.display())),
			(
				"post-index-change",
				format!("touch '{}'\n", marker.display()),
			),
		] {
			let p = hooks.join(name);
			fs::write(&p, format!("#!/bin/sh\n{body}")).unwrap();
			fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
		}
		let payload = bob(
			"original\n",
			vec![change("b.txt", FileChange::Added, Some("b\n"))],
		);
		let result = replay(&dst.open(), &payload);
		assert_eq!(result.failure, None);
		assert_eq!(dst.meta("HEAD").1, "original");
		assert!(!marker.exists(), "a hook ran");
	}

	#[test]
	fn commits_export_ignores_signature_and_encoding_config() {
		let src = Repo::new("main");
		src.write("a.txt", b"a\n");
		let sha = src.commit("caf\u{e9}\n", "2019-01-01T00:00:00+00:00");
		src.git(&["config", "i18n.logOutputEncoding", "ISO-8859-1"]);
		src.git(&["config", "log.showSignature", "true"]);
		let payload = copy_commits(&src.open(), &[sha]).unwrap();
		assert_eq!(payload.commits[0].message, "caf\u{e9}\n");
	}

	#[test]
	fn commits_export_of_a_signed_commit_ignores_show_signature() {
		let src = Repo::new("main");
		let key = src.dir.path().join("key");
		let keygen = Command::new("ssh-keygen")
			.args(["-q", "-t", "ed25519", "-N", "", "-C", "t", "-f"])
			.arg(&key)
			.output();
		if !keygen.is_ok_and(|o| o.status.success()) {
			assert!(
				std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(),
				"ssh-keygen is required"
			);
			eprintln!("skipping: no ssh-keygen");
			return;
		}
		let public = fs::read_to_string(key.with_extension("pub")).unwrap();
		let allowed = src.dir.path().join("allowed");
		fs::write(&allowed, format!("alice@example.com {public}")).unwrap();
		src.git(&["config", "gpg.format", "ssh"]);
		src.git(&["config", "user.signingkey", key.to_str().unwrap()]);
		src.git(&[
			"config",
			"gpg.ssh.allowedSignersFile",
			allowed.to_str().unwrap(),
		]);
		src.git(&["config", "commit.gpgsign", "true"]);
		src.write("a.txt", b"a\n");
		let sha = src.commit("signed\n", "2019-01-01T00:00:00+00:00");
		src.git(&["config", "log.showSignature", "true"]);
		let payload = copy_commits(&src.open(), &[sha]).unwrap();
		let c = &payload.commits[0];
		assert_eq!(c.author_name, "Alice");
		assert_eq!(c.message, "signed\n");
	}

	#[test]
	fn commits_replay_refuses_a_directory_at_a_write_target_up_front() {
		let dst = Repo::new("main");
		dst.write("a.txt", b"a\n");
		dst.write("d/keep.txt", b"keep\n");
		dst.commit("root", "2019-01-01T00:00:00+00:00");
		let payload = bob(
			"write onto dir\n",
			vec![
				change("a.txt", FileChange::Deleted, None),
				change("b.txt", FileChange::Added, Some("b\n")),
				change("d", FileChange::Added, Some("file\n")),
			],
		);
		let g = dst.open();
		let plan = plan_commit_replay(&g, &payload);
		assert_eq!(
			plan.commits[0].files[2].skip_reason,
			Some(ReplaySkipReason::UnsafePath)
		);
		let head = dst.git(&["rev-parse", "HEAD"]);
		let result = replay(&g, &payload);
		let failure = result.failure.expect("refused");
		assert!(failure.error.contains("directory"), "{}", failure.error);
		assert_eq!(dst.git(&["rev-parse", "HEAD"]), head);
		assert!(dst.path().join("a.txt").exists());
		assert!(!dst.path().join("b.txt").exists());
		assert_eq!(dst.git(&["status", "--porcelain"]), "");
	}

	#[test]
	fn commits_replay_refuses_a_directory_at_a_delete_target_up_front() {
		let dst = Repo::new("main");
		dst.write("a.txt", b"a\n");
		dst.write("gone/inner.txt", b"x\n");
		dst.commit("root", "2019-01-01T00:00:00+00:00");
		let payload = bob(
			"delete dir\n",
			vec![
				change("a.txt", FileChange::Modified, Some("changed\n")),
				change("gone", FileChange::Deleted, None),
			],
		);
		let g = dst.open();
		let plan = plan_commit_replay(&g, &payload);
		assert_eq!(
			plan.commits[0].files[1].skip_reason,
			Some(ReplaySkipReason::UnsafePath)
		);
		let result = replay(&g, &payload);
		assert!(result.failure.is_some());
		assert!(result.created.is_empty());
		assert_eq!(
			fs::read_to_string(dst.path().join("a.txt")).unwrap(),
			"a\n"
		);
		assert!(dst.path().join("gone/inner.txt").exists());
	}

	#[test]
	fn commits_replay_refuses_a_file_where_a_write_needs_a_directory() {
		let dst = Repo::new("main");
		dst.write("f", b"a file\n");
		dst.commit("root", "2019-01-01T00:00:00+00:00");
		let payload = bob(
			"under a file\n",
			vec![
				change("b.txt", FileChange::Added, Some("b\n")),
				change("f/x.txt", FileChange::Added, Some("x\n")),
			],
		);
		let g = dst.open();
		let plan = plan_commit_replay(&g, &payload);
		assert_eq!(
			plan.commits[0].files[1].skip_reason,
			Some(ReplaySkipReason::UnsafePath)
		);
		let result = replay(&g, &payload);
		assert!(result.failure.is_some());
		assert!(!dst.path().join("b.txt").exists());
	}

	#[test]
	fn commits_replay_deleting_under_a_file_is_already_absent() {
		let dst = Repo::new("main");
		dst.write("f", b"a file\n");
		dst.commit("root", "2019-01-01T00:00:00+00:00");
		let payload = bob(
			"delete under a file\n",
			vec![
				change("b.txt", FileChange::Added, Some("b\n")),
				change("f/x.txt", FileChange::Deleted, None),
			],
		);
		let result = replay(&dst.open(), &payload);
		assert_eq!(result.failure, None);
		assert_eq!(dst.git(&["show", "HEAD:b.txt"]), "b");
	}

	#[test]
	fn commits_replay_keeps_an_empty_message() {
		let dst = Repo::new("main");
		dst.write("a.txt", b"a\n");
		dst.commit("root", "2019-01-01T00:00:00+00:00");
		let payload =
			bob("", vec![change("b.txt", FileChange::Added, Some("b\n"))]);
		let result = replay(&dst.open(), &payload);
		assert_eq!(result.failure, None);
		assert_eq!(dst.git(&["log", "-1", "--format=%B"]), "");
		assert_eq!(dst.git(&["show", "HEAD:b.txt"]), "b");
	}

	/// A write target that is a hard link alias of a Git directory's file:
	/// the replay's write REPLACES the directory entry, so `.git/config`
	/// keeps its bytes and the alias path ends up a regular file with the
	/// payload's content. Same on every OS — nothing reads a link count.
	#[cfg(unix)]
	#[test]
	fn a_replay_replaces_a_hard_link_alias_of_a_git_file() {
		let repo = Repo::new("main");
		repo.write("old.txt", b"base\n");
		repo.commit("init", "2020-01-01T00:00:00+00:00");
		let config =
			fs::read_to_string(repo.path().join(".git/config")).unwrap();
		fs::hard_link(
			repo.path().join(".git/config"),
			repo.path().join("alias.txt"),
		)
		.unwrap();
		let payload = CommitsPayload {
			commits: vec![CommitRecord {
				message: "alias\n".into(),
				author_name: "Author".into(),
				author_email: "author@example.invalid".into(),
				author_date: "2020-01-01T00:00:00+00:00".into(),
				files: vec![CommitFile {
					path: "alias.txt".into(),
					old_path: None,
					change: FileChange::Added,
					content: Some("owned\n".into()),
					not_copied: None,
				}],
			}],
		};
		let plan = plan_commit_replay(&repo.open(), &payload);
		assert_eq!(plan.commits[0].files[0].action, ReplayAction::Write);
		let result = replay(&repo.open(), &payload);
		assert_eq!(result.failure, None);
		assert_eq!(
			fs::read_to_string(repo.path().join(".git/config")).unwrap(),
			config,
			"the alias's other name is never written through"
		);
		assert_eq!(
			fs::read_to_string(repo.path().join("alias.txt")).unwrap(),
			"owned\n"
		);

		// An alias that appears after the preview is replaced the same way.
		fs::remove_file(repo.path().join("alias.txt")).unwrap();
		fs::hard_link(
			repo.path().join(".git/config"),
			repo.path().join("alias.txt"),
		)
		.unwrap();
		let _ = replay(&repo.open(), &payload);
		assert_eq!(
			fs::read_to_string(repo.path().join(".git/config")).unwrap(),
			config
		);
		assert_eq!(
			fs::read_to_string(repo.path().join("alias.txt")).unwrap(),
			"owned\n"
		);
	}

	/// The replay's write scope is the folder the user pasted into, not the
	/// whole repository: opening a subfolder refuses targets outside it,
	/// and only the repository root replays the whole repo.
	#[test]
	fn a_replay_scoped_to_a_subfolder_refuses_targets_outside_it() {
		let repo = Repo::new("main");
		repo.write("keep.txt", b"keep\n");
		repo.commit("init", "2020-01-01T00:00:00+00:00");
		fs::create_dir_all(repo.path().join("sub")).unwrap();
		let payload = CommitsPayload {
			commits: vec![CommitRecord {
				message: "both\n".into(),
				author_name: "Author".into(),
				author_email: "author@example.invalid".into(),
				author_date: "2020-01-01T00:00:00+00:00".into(),
				files: vec![
					CommitFile {
						path: "outside.txt".into(),
						old_path: None,
						change: FileChange::Added,
						content: Some("out\n".into()),
						not_copied: None,
					},
					CommitFile {
						path: "sub/inside.txt".into(),
						old_path: None,
						change: FileChange::Added,
						content: Some("in\n".into()),
						not_copied: None,
					},
				],
			}],
		};
		// Scoped to the subfolder: the root-level target is refused before
		// anything is written.
		let err = plan_commit_replay_in(
			&repo.open(),
			&repo.path().join("sub"),
			&payload,
			&RunOptions::default(),
		)
		.unwrap_err();
		assert!(matches!(
			err,
			CommitError::OutsideScope { ref path, .. } if path == "outside.txt"
		));
		assert!(!repo.path().join("outside.txt").exists());

		// Scoped to the repository root: the whole repo may be replayed.
		let plan = plan_commit_replay_in(
			&repo.open(),
			&repo.path(),
			&payload,
			&RunOptions::default(),
		)
		.unwrap();
		assert_eq!(plan.commits[0].files.len(), 2);
	}

	/// A bare repository kept inside the worktree is a Git directory: a
	/// replayed file landing in it is an unsafe-path skip, never written.
	#[test]
	fn replay_never_writes_into_a_git_directory_inside_the_worktree() {
		let repo = Repo::new("main");
		repo.write("vendor/lib.git/HEAD", b"ref: refs/heads/main\n");
		fs::create_dir_all(repo.path().join("vendor/lib.git/objects")).unwrap();
		fs::create_dir_all(repo.path().join("vendor/lib.git/refs")).unwrap();
		repo.write("vendor/lib.git/hooks/.keep", b"");
		repo.commit("init", "2020-01-01T00:00:00+00:00");
		let file = |path: &str| CommitFile {
			path: path.into(),
			old_path: None,
			change: FileChange::Added,
			content: Some("x\n".into()),
			not_copied: None,
		};
		let payload = CommitsPayload {
			commits: vec![CommitRecord {
				message: "hooks\n".into(),
				author_name: "Author".into(),
				author_email: "author@example.invalid".into(),
				author_date: "2020-01-01T00:00:00+00:00".into(),
				files: vec![
					file("vendor/lib.git/hooks/pre-commit"),
					file("ok.txt"),
				],
			}],
		};
		let plan = plan_commit_replay(&repo.open(), &payload);
		let files = &plan.commits[0].files;
		assert_eq!(files[0].skip_reason, Some(ReplaySkipReason::UnsafePath));
		assert_eq!(files[1].action, ReplayAction::Write);
		let result = replay(&repo.open(), &payload);
		assert_eq!(result.failure, None);
		assert!(!repo.path().join("vendor/lib.git/hooks/pre-commit").exists());
		assert_eq!(fs::read(repo.path().join("ok.txt")).unwrap(), b"x\n");
	}

	#[test]
	fn freshness_targets_follow_the_planner_rules() {
		let repo = Repo::new("main");
		repo.write("old.txt", b"old content\n");
		repo.write("bad_utf8.txt", &[0xff, 0xfe, 0xfd]);
		repo.commit("init", "2020-01-01T00:00:00+00:00");

		#[cfg(unix)]
		{
			repo.write("other/x.txt", b"x\n");
			std::os::unix::fs::symlink("other", repo.path().join("symdir"))
				.unwrap();
		}

		#[allow(unused_mut)]
		let mut files = vec![
			// NotCopied
			CommitFile {
				path: "bin.dat".into(),
				old_path: None,
				change: FileChange::Added,
				content: None,
				not_copied: Some(NotCopiedReason::Binary),
			},
			// 一般 Write
			CommitFile {
				path: "write.txt".into(),
				old_path: None,
				change: FileChange::Added,
				content: Some("write\n".into()),
				not_copied: None,
			},
			// rename
			CommitFile {
				path: "renamed.txt".into(),
				old_path: Some("old.txt".into()),
				change: FileChange::Renamed,
				content: Some("renamed\n".into()),
				not_copied: None,
			},
			// 非 UTF-8 目標 (NonUtf8Target)
			CommitFile {
				path: "bad_utf8.txt".into(),
				old_path: None,
				change: FileChange::Modified,
				content: Some("utf8 payload\n".into()),
				not_copied: None,
			},
		];

		#[cfg(unix)]
		files.push(CommitFile {
			path: "symdir/x.txt".into(),
			old_path: None,
			change: FileChange::Added,
			content: Some("sub\n".into()),
			not_copied: None,
		});

		let payload = CommitsPayload {
			commits: vec![CommitRecord {
				message: "test targets\n".into(),
				author_name: "Author".into(),
				author_email: "author@example.invalid".into(),
				author_date: "2020-01-01T00:00:00+00:00".into(),
				files,
			}],
		};

		let plan = plan_commit_replay(&repo.open(), &payload);
		let planned_files = &plan.commits[0].files;

		// 1. NotCopied 產出 0 個
		assert_eq!(
			planned_files[0].skip_reason,
			Some(ReplaySkipReason::NotCopied)
		);
		assert_eq!(planned_files[0].freshness_targets().count(), 0);

		// 2. 一般 Write 產出 (abs, path)
		assert_eq!(planned_files[1].action, ReplayAction::Write);
		let write_targets =
			planned_files[1].freshness_targets().collect::<Vec<_>>();
		assert_eq!(write_targets.len(), 1);
		assert_eq!(
			write_targets[0],
			(
				planned_files[1].absolute_path.as_ref().unwrap(),
				"write.txt"
			)
		);

		// 3. rename 另外產出 (old_abs, old_path)
		assert_eq!(planned_files[2].action, ReplayAction::Write);
		let rename_targets =
			planned_files[2].freshness_targets().collect::<Vec<_>>();
		assert_eq!(rename_targets.len(), 2);
		assert_eq!(
			rename_targets[0],
			(
				planned_files[2].absolute_path.as_ref().unwrap(),
				"renamed.txt"
			)
		);
		assert_eq!(
			rename_targets[1],
			(
				planned_files[2].old_absolute_path.as_ref().unwrap(),
				"old.txt"
			)
		);

		// 4. 非 UTF-8 目標（NonUtf8Target）只產出目標
		assert_eq!(
			planned_files[3].skip_reason,
			Some(ReplaySkipReason::NonUtf8Target)
		);
		let non_utf8_targets =
			planned_files[3].freshness_targets().collect::<Vec<_>>();
		assert_eq!(non_utf8_targets.len(), 1);
		assert_eq!(
			non_utf8_targets[0],
			(
				planned_files[3].absolute_path.as_ref().unwrap(),
				"bad_utf8.txt"
			)
		);

		// 5. 經過 symlink 父目錄的 UnsafePath 產出 0 個（unix）
		#[cfg(unix)]
		{
			assert_eq!(
				planned_files[4].skip_reason,
				Some(ReplaySkipReason::UnsafePath)
			);
			assert_eq!(planned_files[4].freshness_targets().count(), 0);
		}
	}
}
