//! Wire format: one JSON value per frame, each frame a big-endian `u32`
//! length followed by that many bytes. Frames are capped on both sides, so
//! a peer can never make the other allocate more than [`MAX_FRAME`].

use std::io::{self, Read, Write};
use std::time::Duration;

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use snip_core::browser::{
	BlobText, CommitSummary, GitPreview, LogQuery, RefSnapshot, TreeEntry,
};
use snip_core::format::ChangeType;
use snip_core::gitsrc::GitSource;
use snip_core::gitview::{
	ChangeList, ChangedPathList, CommitDetails, FoundKind, ReadProfile,
	StatusSummary,
};
use snip_core::graph::MAX_REF_NAME_LEN;
use snip_core::workspace::ScanStatus;

/// Base protocol, both sides must speak it.
pub const PROTOCOL_VERSION: u32 = 1;

/// Newest protocol this build speaks. 2 = Git views, 3 = copy.
pub const PROTOCOL_MAX: u32 = 3;
/// The first protocol with Git views.
pub const GIT_VIEWS_VERSION: u32 = 2;
/// The first protocol with copy ([`Request::Export`]).
pub const TRANSFER_VERSION: u32 = 3;
/// Payload bytes one [`Response::Chunk`] carries: JSON escaping can grow
/// text several times and must stay under [`MAX_FRAME`].
pub const CHUNK_BYTES: usize = 1024 * 1024;
pub const GIT_CALL_LIMIT: Duration = Duration::from_secs(90); // > worker job deadlines 60/75 s
pub const MAX_GIT_CALLS_IN_FLIGHT: usize = 4;
pub const REMOTE_MAX_LOG_LIMIT: usize = 1_000;
pub const REMOTE_MAX_TIPS: usize = 50_000;

/// Largest frame either side sends or accepts. A preview is at most 1 MiB
/// of text, and JSON escaping can grow it several times.
pub const MAX_FRAME: usize = 8 * 1024 * 1024;

/// Entries one `ListDir` reply carries at most; the rest is reported as
/// `truncated`.
pub const MAX_DIR_ENTRIES: usize = 1000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
	/// First frame of every connection.
	Hello {
		version: u32,
		name: String,
		#[serde(default)]
		max_version: Option<u32>,
	},
	/// Resolves `path` (absolute, or `~` / `~/…` for the worker's home)
	/// to the folder every later request names as its `workspace`.
	OpenWorkspace {
		path: String,
	},
	ListDir {
		workspace: String,
		path: String,
	},
	Stat {
		workspace: String,
		path: String,
	},
	Read {
		workspace: String,
		path: String,
	},
	/// Reserved for the next slices; a worker answers `Unsupported`.
	Write {
		workspace: String,
		path: String,
		content: String,
	},
	Rename {
		workspace: String,
		from: String,
		to: String,
	},
	ScanRepos {
		workspace: String,
		#[serde(default)]
		under: Option<String>,
	},
	GitView {
		workspace: String,
		repo: String,
		profile: ReadProfile,
		query: GitQuery,
	},
	/// Copies files or changes as one snip-sync payload, with the same
	/// engine a local copy uses ([`snip_core::transfer::copy_selection`]).
	Export {
		workspace: String,
		items: Vec<ExportTarget>,
		settings: snip_core::settings::Settings,
		file_limit: usize,
	},
	/// Copies commits of `repo` as a commit payload.
	ExportCommits {
		workspace: String,
		repo: String,
		tip: String,
		selected: Vec<String>,
	},
}

/// One item to copy: `path` under `root`, both relative to the workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportTarget {
	/// The repository (or folder) the path is relative to; "" is the
	/// workspace itself.
	pub root: String,
	pub path: String,
	pub source: snip_core::transfer::SourceKind,
	pub change_type: Option<ChangeType>,
}

impl Request {
	pub fn needs_version(&self) -> u32 {
		match self {
			Self::ScanRepos { .. } | Self::GitView { .. } => GIT_VIEWS_VERSION,
			Self::Export { .. } | Self::ExportCommits { .. } => {
				TRANSFER_VERSION
			}
			_ => 1,
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "q", rename_all = "snake_case")]
pub enum GitQuery {
	ChangeList,
	Refs,
	ResolveCommit {
		rev: String,
	},
	LogFromTips {
		tips: Vec<String>,
		skip: usize,
		limit: usize,
	},
	HistoryQuery {
		reference: Option<String>,
		query: LogQuery,
		skip: usize,
		limit: usize,
	},
	CommitDetails {
		sha: String,
	},
	UserEmail,
	ChangedPaths {
		source: GitSource,
	},
	Preview {
		source: GitSource,
		path: String,
		change: Option<ChangeType>,
	},
	ChangedFileText {
		source: GitSource,
		path: String,
		max: u64,
	},
	CommitDirectory {
		rev: String,
		dir: String,
		limit: usize,
	},
	CommitBlob {
		rev: String,
		path: String,
		max: u64,
	},
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Response {
	Hello {
		version: u32,
		name: String,
		/// The worker's home folder, where a master starts browsing.
		#[serde(default)]
		home: Option<String>,
		#[serde(default)]
		max_version: Option<u32>,
	},
	Workspace(RemoteWorkspace),
	Dir {
		entries: Vec<DirEntry>,
		truncated: bool,
	},
	Stat(Stat),
	/// `content` is `None` for a binary or non-UTF-8 file, as local preview.
	Text {
		content: Option<String>,
	},
	Pending,
	Repos(RepoScan),
	Git(GitReply),
	Copied(snip_core::transfer::CopyOutcome),
	CommitsCopied(snip_core::commits::CommitCopyOutcome),
	/// Part of the next `Copied` / `CommitsCopied` text, which then
	/// arrives with that text empty.
	Chunk {
		data: String,
	},
	Error {
		code: ErrorCode,
		message: String,
	},
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "r", content = "v", rename_all = "snake_case")]
pub enum GitReply {
	ChangeList(ChangeList),
	Refs(RefSnapshot),
	Commit(String),
	Log {
		commits: Vec<CommitSummary>,
		more: bool,
	},
	Details(CommitDetails),
	UserEmail(Option<String>),
	ChangedPaths(ChangedPathList),
	Preview(GitPreview),
	FileText(Option<String>),
	Directory {
		entries: Vec<TreeEntry>,
		more: bool,
	},
	Blob(BlobText),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoScan {
	pub repos: Vec<ScannedRepo>,
	pub errors: Vec<(String, String)>,
	pub error_overflow: usize,
	pub depth_limited: Vec<String>,
	pub depth_overflow: usize,
	pub status: ScanStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScannedRepo {
	pub rel: String,
	pub utf8: bool,
	pub name: String,
	pub kind: FoundKind,
	pub summary: Result<StatusSummary, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteWorkspace {
	/// The folder's real path on the worker: the `workspace` of every
	/// request about it.
	pub id: String,
	pub name: String,
	/// The worker's own spelling of the path, for display only.
	pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntry {
	/// Lossy when the name is not UTF-8; then `utf8` is false and the entry
	/// cannot be addressed.
	pub name: String,
	pub utf8: bool,
	pub directory: bool,
	pub symlink: bool,
	/// A directory with its own `.git`.
	pub nested_repo: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
	File,
	Directory,
	Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stat {
	pub kind: EntryKind,
	pub size: u64,
	/// Seconds since the Unix epoch, when the OS reports it.
	pub modified: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
	/// Outside the workspace, or not readable.
	Forbidden,
	NotFound,
	Unsupported,
	BadRequest,
	TooLarge,
	Io,
	VersionMismatch,
	/// Target folder is not a valid git repository.
	NotARepository,
	/// Revision argument is invalid or malformed.
	InvalidRevision,
	/// Worker operation timed out.
	Timeout,
	/// Worker has reached concurrency limits.
	Busy,
	/// Worker operation was cancelled.
	Cancelled,
	/// Repository escapes shared boundary.
	OutsideShare,
}

pub fn write_frame<T: Serialize>(
	w: &mut impl Write,
	value: &T,
) -> io::Result<()> {
	let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
	if bytes.len() > MAX_FRAME {
		return Err(io::Error::new(
			io::ErrorKind::InvalidData,
			format!("frame of {} bytes exceeds {MAX_FRAME}", bytes.len()),
		));
	}
	w.write_all(&(bytes.len() as u32).to_be_bytes())?;
	w.write_all(&bytes)?;
	w.flush()
}

/// `Ok(None)` on a clean end of stream before a frame starts.
pub fn read_frame<T: DeserializeOwned>(
	r: &mut impl Read,
) -> io::Result<Option<T>> {
	let mut len = [0u8; 4];
	match r.read_exact(&mut len) {
		Ok(()) => {}
		Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => {
			return Ok(None);
		}
		Err(err) => return Err(err),
	}
	let len = u32::from_be_bytes(len) as usize;
	if len > MAX_FRAME {
		return Err(io::Error::new(
			io::ErrorKind::InvalidData,
			format!("frame of {len} bytes exceeds {MAX_FRAME}"),
		));
	}
	let mut buf = vec![0u8; len];
	r.read_exact(&mut buf)?;
	serde_json::from_slice(&buf)
		.map(Some)
		.map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

pub fn valid_hex_oid(s: &str) -> bool {
	(4..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

pub fn valid_rev(s: &str) -> bool {
	if valid_hex_oid(s) || s == "HEAD" {
		return true;
	}
	if s.is_empty() || s.len() > MAX_REF_NAME_LEN {
		return false;
	}
	if s == "@" {
		return false;
	}
	if s.starts_with('-') || s.starts_with('/') {
		return false;
	}
	if s.ends_with('/') || s.ends_with('.') || s.ends_with(".lock") {
		return false;
	}
	if s.contains("..") || s.contains("//") || s.contains("@{") {
		return false;
	}
	for c in s.chars() {
		if matches!(c, ':' | '^' | '~' | '?' | '*' | '[' | '\\' | '\0')
			|| c.is_whitespace()
			|| c.is_control()
		{
			return false;
		}
	}
	for seg in s.split('/') {
		if seg.starts_with('.') || seg.ends_with(".lock") {
			return false;
		}
	}
	true
}

pub fn valid_rel_path(s: &str, allow_empty: bool) -> bool {
	if s.is_empty() {
		return allow_empty;
	}
	if s.contains('\0') || s.contains('\n') || s.contains('\r') {
		return false;
	}
	if s.starts_with('-') || s.starts_with(':') || s.starts_with('/') {
		return false;
	}
	for seg in s.split('/') {
		if seg.is_empty() || seg == "." || seg == ".." {
			return false;
		}
	}
	for comp in std::path::Path::new(s).components() {
		if !matches!(comp, std::path::Component::Normal(_)) {
			return false;
		}
	}
	true
}

pub fn valid_log_query(q: &LogQuery) -> bool {
	if q.text.len() > 1024 || q.text.contains('\0') {
		return false;
	}
	if let Some(author) = &q.author {
		if author.len() > 1024 || author.contains('\0') {
			return false;
		}
	}
	if let Some(since) = &q.since {
		if since.len() > 64 || since.contains('\0') {
			return false;
		}
	}
	if let Some(until) = &q.until {
		if until.len() > 64 || until.contains('\0') {
			return false;
		}
	}
	if q.paths.len() > 64 {
		return false;
	}
	for p in &q.paths {
		if !valid_rel_path(p, false) {
			return false;
		}
	}
	true
}

pub fn valid_tips(tips: &[String]) -> bool {
	tips.len() <= REMOTE_MAX_TIPS && tips.iter().all(|t| valid_hex_oid(t))
}

#[cfg(test)]
mod tests {
	use super::*;
	use snip_core::browser::TreeKind;
	use snip_core::workspace::ChangeCounts;

	#[test]
	fn frames_round_trip_and_end_cleanly() {
		let mut buf = Vec::new();
		let req = Request::ListDir {
			workspace: "w".into(),
			path: "src/多行".into(),
		};
		write_frame(&mut buf, &req).unwrap();
		write_frame(&mut buf, &Request::OpenWorkspace { path: "~".into() })
			.unwrap();
		let mut r = buf.as_slice();
		assert_eq!(read_frame::<Request>(&mut r).unwrap(), Some(req));
		assert_eq!(
			read_frame::<Request>(&mut r).unwrap(),
			Some(Request::OpenWorkspace { path: "~".into() })
		);
		assert_eq!(read_frame::<Request>(&mut r).unwrap(), None);
	}

	#[test]
	fn oversized_frame_header_is_refused_before_allocating() {
		let mut buf = ((MAX_FRAME + 1) as u32).to_be_bytes().to_vec();
		buf.extend_from_slice(b"{}");
		let err = read_frame::<Request>(&mut buf.as_slice()).unwrap_err();
		assert_eq!(err.kind(), io::ErrorKind::InvalidData);
	}

	#[test]
	fn truncated_frame_is_an_error_not_a_clean_end() {
		let mut buf = Vec::new();
		write_frame(&mut buf, &Request::OpenWorkspace { path: "~".into() })
			.unwrap();
		buf.pop();
		assert!(read_frame::<Request>(&mut buf.as_slice()).is_err());
	}

	fn round_trip_check<
		T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
	>(
		val: &T,
	) {
		let json = serde_json::to_vec(val).expect("serialize");
		let de: T = serde_json::from_slice(&json).expect("deserialize");
		assert_eq!(val, &de);

		let mut frame_buf = Vec::new();
		write_frame(&mut frame_buf, val).expect("write_frame");
		let mut r = frame_buf.as_slice();
		let frame_de: T = read_frame(&mut r).unwrap().unwrap();
		assert_eq!(val, &frame_de);
	}

	#[test]
	fn round_trip_new_requests_responses_and_types() {
		let requests = [
			Request::Hello {
				version: 1,
				name: "m".into(),
				max_version: Some(2),
			},
			Request::ScanRepos {
				workspace: "ws".into(),
				under: Some("sub".into()),
			},
			Request::GitView {
				workspace: "ws".into(),
				repo: "repo1".into(),
				profile: ReadProfile::Interactive,
				query: GitQuery::ChangeList,
			},
		];
		for req in &requests {
			round_trip_check(req);
		}

		let queries = [
			GitQuery::ChangeList,
			GitQuery::Refs,
			GitQuery::ResolveCommit { rev: "HEAD".into() },
			GitQuery::LogFromTips {
				tips: vec!["abcd".into()],
				skip: 0,
				limit: 10,
			},
			GitQuery::HistoryQuery {
				reference: Some("main".into()),
				query: LogQuery {
					text: "test".into(),
					regex: false,
					match_case: true,
					author: Some("audic".into()),
					since: None,
					until: None,
					paths: vec!["src/lib.rs".into()],
				},
				skip: 5,
				limit: 20,
			},
			GitQuery::CommitDetails {
				sha: "1234567890abcdef1234567890abcdef12345678".into(),
			},
			GitQuery::UserEmail,
			GitQuery::ChangedPaths {
				source: GitSource::Working,
			},
			GitQuery::Preview {
				source: GitSource::Staged,
				path: "a/b.rs".into(),
				change: Some(ChangeType::Modified),
			},
			GitQuery::ChangedFileText {
				source: GitSource::Commit("abcd".into()),
				path: "foo.txt".into(),
				max: 1024,
			},
			GitQuery::CommitDirectory {
				rev: "HEAD".into(),
				dir: "src".into(),
				limit: 100,
			},
			GitQuery::CommitBlob {
				rev: "HEAD".into(),
				path: "src/lib.rs".into(),
				max: 4096,
			},
		];
		for q in &queries {
			round_trip_check(q);
		}

		let replies = [
			GitReply::ChangeList(ChangeList {
				summary: Some(StatusSummary {
					head: Some("sha".into()),
					branch: Some("main".into()),
					changes: ChangeCounts::default(),
				}),
				rows: vec![],
				total: 0,
			}),
			GitReply::Refs(RefSnapshot {
				refs: vec![],
				head: Some("sha".into()),
				detached: false,
				shallow: vec![],
			}),
			GitReply::Commit("deadbeef".into()),
			GitReply::Log {
				commits: vec![CommitSummary {
					sha: "sha1".into(),
					parents: vec![],
					author_name: "a".into(),
					author_email: "e".into(),
					author_date: "d".into(),
					subject: "s".into(),
				}],
				more: true,
			},
			GitReply::Details(CommitDetails {
				sha: "sha1".into(),
				parents: vec!["p1".into()],
				message: "msg".into(),
				author: "auth".into(),
				author_email: "mail".into(),
				author_date: "date".into(),
				committer: "c".into(),
				committer_email: "ce".into(),
				commit_date: "cd".into(),
				branches: vec!["main".into()],
				branches_more: false,
			}),
			GitReply::UserEmail(Some("user@test.local".into())),
			GitReply::UserEmail(None),
			GitReply::ChangedPaths(ChangedPathList {
				paths: vec![],
				gitlinks: vec![],
				total: 0,
			}),
			GitReply::Preview(GitPreview {
				content: Some("text".into()),
				patch: "patch".into(),
				patch_truncated: false,
			}),
			GitReply::FileText(Some("file content".into())),
			GitReply::FileText(None),
			GitReply::Directory {
				entries: vec![TreeEntry {
					path: "src".into(),
					name: "src".into(),
					kind: TreeKind::Tree,
				}],
				more: false,
			},
			GitReply::Blob(BlobText::Text("blob text".into())),
		];
		for r in &replies {
			round_trip_check(r);
		}

		let responses = [
			Response::Pending,
			Response::Repos(RepoScan {
				repos: vec![ScannedRepo {
					rel: "sub".into(),
					utf8: true,
					name: "sub".into(),
					kind: FoundKind::Main,
					summary: Ok(StatusSummary {
						head: None,
						branch: None,
						changes: ChangeCounts::default(),
					}),
				}],
				errors: vec![("err_path".into(), "error message".into())],
				error_overflow: 0,
				depth_limited: vec!["depth_path".into()],
				depth_overflow: 0,
				status: ScanStatus::Complete,
			}),
			Response::Git(GitReply::Commit("abc".into())),
			Response::Error {
				code: ErrorCode::NotARepository,
				message: "not a git repo".into(),
			},
			Response::Error {
				code: ErrorCode::InvalidRevision,
				message: "invalid rev".into(),
			},
			Response::Error {
				code: ErrorCode::Timeout,
				message: "timed out".into(),
			},
			Response::Error {
				code: ErrorCode::Busy,
				message: "busy".into(),
			},
			Response::Error {
				code: ErrorCode::Cancelled,
				message: "cancelled".into(),
			},
			Response::Error {
				code: ErrorCode::OutsideShare,
				message: "outside share".into(),
			},
		];
		for res in &responses {
			round_trip_check(res);
		}
	}

	#[test]
	fn hello_unknown_extra_field_parses_with_none_max_version() {
		let json = r#"{"op":"hello","version":1,"name":"x","future":true}"#;
		let req: Request = serde_json::from_str(json).unwrap();
		match req {
			Request::Hello {
				version,
				name,
				max_version,
			} => {
				assert_eq!(version, 1);
				assert_eq!(name, "x");
				assert_eq!(max_version, None);
			}
			other => panic!("expected Hello, got {other:?}"),
		}
	}

	#[test]
	fn hello_reply_without_home_or_max_version_parses_as_none() {
		let json = r#"{"reply":"hello","version":1,"name":"w"}"#;
		let res: Response = serde_json::from_str(json).unwrap();
		match res {
			Response::Hello {
				version,
				name,
				home,
				max_version,
			} => {
				assert_eq!(version, 1);
				assert_eq!(name, "w");
				assert_eq!(home, None);
				assert_eq!(max_version, None);
			}
			other => panic!("expected Hello, got {other:?}"),
		}
	}

	#[test]
	fn new_hello_round_trips_with_some_max_version() {
		let hello = Request::Hello {
			version: PROTOCOL_VERSION,
			name: "master".into(),
			max_version: Some(2),
		};
		round_trip_check(&hello);

		let hello_reply = Response::Hello {
			version: PROTOCOL_VERSION,
			name: "worker".into(),
			home: Some("/home/u".into()),
			max_version: Some(2),
		};
		round_trip_check(&hello_reply);
	}

	#[test]
	fn needs_version_table() {
		let v2_requests = [
			Request::ScanRepos {
				workspace: "ws".into(),
				under: None,
			},
			Request::GitView {
				workspace: "ws".into(),
				repo: "".into(),
				profile: ReadProfile::Interactive,
				query: GitQuery::ChangeList,
			},
		];
		for req in &v2_requests {
			assert_eq!(req.needs_version(), GIT_VIEWS_VERSION);
		}

		let v1_requests = [
			Request::Hello {
				version: 1,
				name: "n".into(),
				max_version: None,
			},
			Request::OpenWorkspace { path: "~".into() },
			Request::ListDir {
				workspace: "w".into(),
				path: "p".into(),
			},
			Request::Stat {
				workspace: "w".into(),
				path: "p".into(),
			},
			Request::Read {
				workspace: "w".into(),
				path: "p".into(),
			},
			Request::Write {
				workspace: "w".into(),
				path: "p".into(),
				content: "c".into(),
			},
			Request::Rename {
				workspace: "w".into(),
				from: "a".into(),
				to: "b".into(),
			},
		];
		for req in &v1_requests {
			assert_eq!(req.needs_version(), 1);
		}
	}

	#[test]
	fn valid_rev_table() {
		let hex40 = "a".repeat(40);
		let hex64 = "f".repeat(64);
		let accept = [
			"HEAD",
			"main",
			"feature/x",
			"v1.2.3",
			"abcd",
			&hex40,
			&hex64,
			"release-1",
		];
		for r in accept {
			assert!(valid_rev(r), "expected valid_rev({r}) == true");
		}

		let too_long = "a".repeat(MAX_REF_NAME_LEN + 1);
		let reject = [
			":/x",
			"HEAD@{0}",
			"@{upstream}",
			"@",
			"HEAD:a",
			"a..b",
			"../../x",
			"-x",
			"--output=/tmp/pwned",
			"/x",
			"x/",
			"x.",
			"x.lock",
			"a//b",
			"a/.b",
			"a/b.lock/c",
			"a b",
			"a^",
			"a~1",
			"a?",
			"a*",
			"a[",
			"a\\b",
			"a\nb",
			"a\0b",
			"",
			&too_long,
		];
		for r in reject {
			assert!(!valid_rev(r), "expected valid_rev({r}) == false");
		}
	}

	#[test]
	fn valid_rel_path_table() {
		let accept = ["a", "a/b.rs", "src/多行"];
		for p in accept {
			assert!(
				valid_rel_path(p, false),
				"expected valid_rel_path({p}, false) == true"
			);
		}

		assert!(valid_rel_path("", true));
		assert!(!valid_rel_path("", false));

		let reject = [
			"../x", "a/../b", "./a", "/abs", "-flag", ":/x", "a//b", "a\nb",
			"a\0b",
		];
		for p in reject {
			assert!(
				!valid_rel_path(p, false),
				"expected valid_rel_path({p}, false) == false"
			);
		}
	}

	#[test]
	fn valid_log_query_table() {
		let valid_q = LogQuery {
			text: "fix".into(),
			regex: false,
			match_case: true,
			author: Some("alice".into()),
			since: Some("2026-01-01".into()),
			until: Some("2026-12-31".into()),
			paths: vec!["src/lib.rs".into()],
		};
		assert!(valid_log_query(&valid_q));

		// Text too long or containing NUL
		let mut bad_text = valid_q.clone();
		bad_text.text = "a".repeat(1025);
		assert!(!valid_log_query(&bad_text));
		bad_text.text = "a\0b".into();
		assert!(!valid_log_query(&bad_text));

		// Author too long or containing NUL
		let mut bad_author = valid_q.clone();
		bad_author.author = Some("a".repeat(1025));
		assert!(!valid_log_query(&bad_author));
		bad_author.author = Some("a\0b".into());
		assert!(!valid_log_query(&bad_author));

		// Since / until too long or containing NUL
		let mut bad_since = valid_q.clone();
		bad_since.since = Some("a".repeat(65));
		assert!(!valid_log_query(&bad_since));
		bad_since.since = Some("2026\0".into());
		assert!(!valid_log_query(&bad_since));

		// Too many paths
		let mut bad_paths = valid_q.clone();
		bad_paths.paths = (0..65).map(|i| format!("path_{i}")).collect();
		assert!(!valid_log_query(&bad_paths));

		// Invalid path segment
		let mut bad_path_val = valid_q.clone();
		bad_path_val.paths = vec!["../secret".into()];
		assert!(!valid_log_query(&bad_path_val));
	}

	#[test]
	fn valid_tips_table() {
		let tips = vec!["abcd".into(), "1234567890abcdef".into()];
		assert!(valid_tips(&tips));

		let bad_hex = vec!["not_hex".into()];
		assert!(!valid_tips(&bad_hex));

		let short_hex = vec!["abc".into()]; // < 4 chars
		assert!(!valid_tips(&short_hex));

		let too_many: Vec<String> =
			(0..=REMOTE_MAX_TIPS).map(|_| "abcd".into()).collect();
		assert!(!valid_tips(&too_many));
	}
}
