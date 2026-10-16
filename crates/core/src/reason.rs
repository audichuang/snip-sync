//! Why something failed, as data.
//!
//! A [`Reason`] is what a front end words in its own language; the thiserror
//! `Display` of the error types stays the CLI's text and the wire `message`.
//! Core is the lowest crate every producer (gitsrc, transfer, commits,
//! gitview), the wire (snip-remote) and the desktop can all see, so the type
//! lives here and `snip_remote` re-exports it.
//!
//! [`ErrorCode`] is closed: new meaning goes into [`Reason`], never into a
//! new code.

use serde::{Deserialize, Deserializer, Serialize};

use crate::gitsrc::StartReason;

/// The worker's coarse error classification on the wire. Closed: it is never
/// extended again, because an old peer would not know a new code.
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
	/// The paste destination changed since the preview; the message is
	/// the local paste's wording.
	Stale,
	/// Two entries of a paste land on one file.
	Collision,
}

/// What changed under a paste or a copy after its preview. `rel` is relative
/// to the destination (the source root for a source change).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "r", rename_all = "snake_case")]
pub enum StaleChange {
	Created {
		rel: String,
	},
	Deleted {
		rel: String,
	},
	Modified {
		rel: String,
	},
	/// HEAD, branch or index moved.
	Repository,
	/// The preview itself is no longer valid: replay capture or eligibility,
	/// another machine, a prepared token.
	Preview,
}

/// Why something failed, as data. One variant per distinct user sentence.
/// Never `Display`ed: the CLI keeps each error's thiserror text, and the
/// desktop words a `Reason` in one place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "r", rename_all = "snake_case")]
pub enum Reason {
	// worker / transport (master side)
	/// The worker gave only a code (an old worker, or the code says it all).
	Refused {
		code: ErrorCode,
	},
	Start {
		start: Option<StartReason>,
		detail: String,
	},
	Disconnected {
		detail: String,
	},
	Protocol,
	WorkerTooOld {
		worker: String,
		have: u32,
		need: u32,
	},
	TimedOut,
	Cancelled,
	// git
	GitMissing,
	GitFailed {
		stderr: String,
	},
	GitBusy,
	RepoBusy,
	WorkerNeedsRestart,
	TooLarge,
	PatchTooLarge,
	/// The repository is too large to verify it stays inside the share.
	VerifyLimit,
	NotARepository {
		path: String,
	},
	InsideParentRepo {
		path: String,
	},
	InvalidRevision {
		rev: String,
	},
	Shallow {
		sha: String,
	},
	OutsideShare,
	/// An invariant break the user cannot cause.
	Internal {
		detail: String,
	},
	// filesystem
	Io {
		detail: String,
	},
	NotFound {
		path: String,
	},
	PermissionDenied {
		path: String,
	},
	// copy / paste / replay engine
	EmptySelection,
	UnsafePath {
		path: String,
	},
	NotRegularFile {
		path: String,
	},
	OutsideRoot {
		path: String,
	},
	Collision {
		path: String,
	},
	/// A file selected as both Staged and Working.
	StagedAndWorking {
		path: String,
	},
	/// `roots` are the colliding roots, kept locally and emptied on the wire.
	AmbiguousRoots {
		basename: String,
		roots: Vec<String>,
	},
	/// `path` is the file that did not fit (a wire path relative to its
	/// root); `None` when the wrapper overhead alone is over the limit.
	PayloadTooLarge {
		limit: u64,
		actual: u64,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		path: Option<String>,
	},
	SourceChanged {
		change: StaleChange,
	},
	DestinationChanged {
		change: StaleChange,
	},
	CrossRepoCommits,
	/// `at` is the commit that breaks the chain and `first_parent` its first
	/// parent, if any. Both are shas.
	Discontinuous {
		base: String,
		tip: String,
		at: String,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		first_parent: Option<String>,
	},
	NotEnoughCommits {
		requested: u64,
		available: u64,
	},
	NotCommitPayload,
	InvalidPayload {
		detail: String,
	},
	/// `path` is relative to the repository; `scope` is the opened folder,
	/// emptied on the wire.
	OutsideScope {
		path: String,
		scope: String,
	},
	Unverifiable {
		path: String,
	},
	// workspace scan
	/// A declared submodule with no `.git` at its path: not initialized, or
	/// deinited.
	SubmoduleNotCheckedOut,
	/// A declared submodule whose path could not be examined; `detail` is
	/// the OS's text, emptied on the wire.
	SubmoduleUnreadable {
		detail: String,
	},
}

/// An error row of a scan: the English `message` the CLI prints, and the
/// `reason` a front end words. The message is never parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
	pub message: String,
	pub reason: Reason,
}

impl Failure {
	pub fn new(message: impl Into<String>, reason: Reason) -> Self {
		Self {
			message: message.into(),
			reason,
		}
	}

	/// An I/O error, kept as its text and classified by kind.
	pub fn io(e: &std::io::Error, path: &str) -> Self {
		Self::new(e.to_string(), io_reason(e, path))
	}
}

impl std::fmt::Display for Failure {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(&self.message)
	}
}

/// A wire field: missing, null, or not understood (a newer peer's variant)
/// reads as `None`, and the frame still parses. Plain `#[serde(default)]`
/// would fail the whole frame on a future variant.
pub fn lenient<'de, D: Deserializer<'de>>(
	d: D,
) -> Result<Option<Reason>, D::Error> {
	let v = Option::<serde_json::Value>::deserialize(d)?;
	Ok(v.and_then(|v| serde_json::from_value(v).ok()))
}

/// [`lenient`] for a list: each element is read on its own, so one reason a
/// newer peer added costs that element, not the frame. A list that is not
/// a list reads as empty.
pub fn lenient_vec<'de, D: Deserializer<'de>>(
	d: D,
) -> Result<Vec<Option<Reason>>, D::Error> {
	Ok(match Option::<serde_json::Value>::deserialize(d)? {
		Some(serde_json::Value::Array(items)) => items
			.into_iter()
			.map(|v| serde_json::from_value(v).ok())
			.collect(),
		_ => Vec::new(),
	})
}

/// The reason an I/O error stands for. `path` is echoed for the kinds that
/// name one; pass `""` when there is none.
pub fn io_reason(e: &std::io::Error, path: &str) -> Reason {
	use std::io::ErrorKind;
	match e.kind() {
		ErrorKind::NotFound => Reason::NotFound { path: path.into() },
		ErrorKind::PermissionDenied => {
			Reason::PermissionDenied { path: path.into() }
		}
		ErrorKind::TimedOut => Reason::TimedOut,
		ErrorKind::FileTooLarge => Reason::TooLarge,
		_ => Reason::Io {
			detail: e.to_string(),
		},
	}
}

/// A path that names a place under some root rather than on the machine:
/// not absolute, no drive, no parent step.
fn wire_relative(p: &str) -> bool {
	let b = p.as_bytes();
	!p.is_empty()
		&& !p.starts_with(['/', '\\'])
		&& !(b.len() > 1 && b[1] == b':')
		&& !p.split(['/', '\\']).any(|c| c == "..")
}

impl Reason {
	/// What a worker may send. An exhaustive match, so each new variant
	/// decides its own wire policy. `None` where the [`ErrorCode`] already
	/// says it. Otherwise `Some`, with every free-text or possibly-absolute
	/// field emptied (path, scope, roots, detail, stderr); only repo-relative
	/// names, basenames, shas, revs, worker names and numbers survive.
	pub fn for_wire(self) -> Option<Reason> {
		match self {
			Self::Refused { .. }
			| Self::Start { .. }
			| Self::Disconnected { .. }
			| Self::Protocol
			| Self::WorkerTooOld { .. }
			| Self::TimedOut
			| Self::Cancelled
			| Self::NotARepository { .. }
			| Self::InsideParentRepo { .. }
			| Self::InvalidRevision { .. }
			| Self::OutsideShare
			| Self::Io { .. }
			| Self::NotFound { .. }
			| Self::PermissionDenied { .. } => None,
			Self::GitFailed { .. }
			| Self::Internal { .. }
			| Self::InvalidPayload { .. }
			| Self::UnsafePath { .. }
			| Self::NotRegularFile { .. }
			| Self::OutsideRoot { .. }
			| Self::Collision { .. }
			| Self::StagedAndWorking { .. }
			| Self::PayloadTooLarge { .. }
			| Self::Unverifiable { .. }
			| Self::AmbiguousRoots { .. }
			| Self::OutsideScope { .. }
			| Self::SourceChanged { .. }
			| Self::DestinationChanged { .. }
			| Self::GitBusy
			| Self::RepoBusy
			| Self::WorkerNeedsRestart
			| Self::TooLarge
			| Self::PatchTooLarge
			| Self::VerifyLimit
			| Self::Shallow { .. }
			| Self::GitMissing
			| Self::EmptySelection
			| Self::CrossRepoCommits
			| Self::Discontinuous { .. }
			| Self::NotEnoughCommits { .. }
			| Self::NotCommitPayload
			| Self::SubmoduleNotCheckedOut
			| Self::SubmoduleUnreadable { .. } => Some(self.scrubbed()),
		}
	}

	/// This reason with every free-text or possibly-absolute field emptied
	/// (path, scope, roots, detail, stderr), for a peer that must not learn
	/// the machine's layout. Unlike [`for_wire`](Self::for_wire) it keeps
	/// the variant whatever the code says: a scan row has no code. Only
	/// repo-relative names, basenames, shas, revs, worker names and numbers
	/// survive. An exhaustive match.
	pub fn scrubbed(self) -> Reason {
		match self {
			Self::Start { start, .. } => Self::Start {
				start,
				detail: String::new(),
			},
			Self::Disconnected { .. } => Self::Disconnected {
				detail: String::new(),
			},
			Self::NotARepository { .. } => Self::NotARepository {
				path: String::new(),
			},
			Self::InsideParentRepo { .. } => Self::InsideParentRepo {
				path: String::new(),
			},
			Self::GitFailed { .. } => Self::GitFailed {
				stderr: String::new(),
			},
			Self::Internal { .. } => Self::Internal {
				detail: String::new(),
			},
			Self::InvalidPayload { .. } => Self::InvalidPayload {
				detail: String::new(),
			},
			Self::Io { .. } => Self::Io {
				detail: String::new(),
			},
			Self::SubmoduleUnreadable { .. } => Self::SubmoduleUnreadable {
				detail: String::new(),
			},
			Self::NotFound { .. } => Self::NotFound {
				path: String::new(),
			},
			Self::PermissionDenied { .. } => Self::PermissionDenied {
				path: String::new(),
			},
			Self::UnsafePath { .. } => Self::UnsafePath {
				path: String::new(),
			},
			Self::NotRegularFile { .. } => Self::NotRegularFile {
				path: String::new(),
			},
			Self::OutsideRoot { .. } => Self::OutsideRoot {
				path: String::new(),
			},
			Self::Collision { .. } => Self::Collision {
				path: String::new(),
			},
			Self::StagedAndWorking { path } => Self::StagedAndWorking {
				path: if wire_relative(&path) {
					path
				} else {
					String::new()
				},
			},
			Self::PayloadTooLarge {
				limit,
				actual,
				path,
			} => Self::PayloadTooLarge {
				limit,
				actual,
				path: path.filter(|p| wire_relative(p)),
			},
			Self::Unverifiable { .. } => Self::Unverifiable {
				path: String::new(),
			},
			Self::AmbiguousRoots { basename, .. } => Self::AmbiguousRoots {
				basename,
				roots: Vec::new(),
			},
			Self::OutsideScope { path, .. } => Self::OutsideScope {
				path,
				scope: String::new(),
			},
			// `rel` is relative to the root, so it stays.
			Self::SourceChanged { .. } | Self::DestinationChanged { .. } => {
				self
			}
			Self::Refused { .. }
			| Self::Protocol
			| Self::WorkerTooOld { .. }
			| Self::TimedOut
			| Self::Cancelled
			| Self::GitMissing
			| Self::GitBusy
			| Self::RepoBusy
			| Self::WorkerNeedsRestart
			| Self::TooLarge
			| Self::PatchTooLarge
			| Self::VerifyLimit
			| Self::InvalidRevision { .. }
			| Self::Shallow { .. }
			| Self::OutsideShare
			| Self::EmptySelection
			| Self::CrossRepoCommits
			| Self::Discontinuous { .. }
			| Self::NotEnoughCommits { .. }
			| Self::NotCommitPayload
			| Self::SubmoduleNotCheckedOut => self,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Every variant gets a distinct index, by an exhaustive match: a new
	/// variant does not compile until it has one, and `samples` must then
	/// cover it (`samples_cover_every_variant`).
	fn reason_index(r: &Reason) -> usize {
		match r {
			Reason::Refused { .. } => 0,
			Reason::Start { .. } => 1,
			Reason::Disconnected { .. } => 2,
			Reason::Protocol => 3,
			Reason::WorkerTooOld { .. } => 4,
			Reason::TimedOut => 5,
			Reason::Cancelled => 6,
			Reason::GitMissing => 7,
			Reason::GitFailed { .. } => 8,
			Reason::GitBusy => 9,
			Reason::RepoBusy => 10,
			Reason::WorkerNeedsRestart => 11,
			Reason::TooLarge => 12,
			Reason::PatchTooLarge => 13,
			Reason::VerifyLimit => 14,
			Reason::NotARepository { .. } => 15,
			Reason::InsideParentRepo { .. } => 16,
			Reason::InvalidRevision { .. } => 17,
			Reason::Shallow { .. } => 18,
			Reason::OutsideShare => 19,
			Reason::Internal { .. } => 20,
			Reason::Io { .. } => 21,
			Reason::NotFound { .. } => 22,
			Reason::PermissionDenied { .. } => 23,
			Reason::EmptySelection => 24,
			Reason::UnsafePath { .. } => 25,
			Reason::NotRegularFile { .. } => 26,
			Reason::OutsideRoot { .. } => 27,
			Reason::Collision { .. } => 28,
			Reason::AmbiguousRoots { .. } => 29,
			Reason::PayloadTooLarge { .. } => 30,
			Reason::StagedAndWorking { .. } => 40,
			Reason::SourceChanged { .. } => 31,
			Reason::DestinationChanged { .. } => 32,
			Reason::CrossRepoCommits => 33,
			Reason::Discontinuous { .. } => 34,
			Reason::NotEnoughCommits { .. } => 35,
			Reason::NotCommitPayload => 36,
			Reason::InvalidPayload { .. } => 37,
			Reason::OutsideScope { .. } => 38,
			Reason::Unverifiable { .. } => 39,
			Reason::SubmoduleNotCheckedOut => 41,
			Reason::SubmoduleUnreadable { .. } => 42,
		}
	}

	/// Fields that must not reach the wire carry "/abs/secret"; the ones
	/// that may carry "ok".
	fn samples() -> Vec<Reason> {
		let s = || "/abs/secret".to_string();
		let ok = || "ok".to_string();
		let mut v = vec![
			Reason::Refused {
				code: ErrorCode::Io,
			},
			Reason::Start {
				start: Some(StartReason::KeyLogin),
				detail: s(),
			},
			Reason::Disconnected { detail: s() },
			Reason::Protocol,
			Reason::WorkerTooOld {
				worker: ok(),
				have: 1,
				need: 2,
			},
			Reason::TimedOut,
			Reason::Cancelled,
			Reason::GitMissing,
			Reason::GitFailed { stderr: s() },
			Reason::GitBusy,
			Reason::RepoBusy,
			Reason::WorkerNeedsRestart,
			Reason::TooLarge,
			Reason::PatchTooLarge,
			Reason::VerifyLimit,
			Reason::NotARepository { path: s() },
			Reason::InsideParentRepo { path: s() },
			Reason::InvalidRevision { rev: ok() },
			Reason::Shallow { sha: ok() },
			Reason::OutsideShare,
			Reason::Internal { detail: s() },
			Reason::Io { detail: s() },
			Reason::NotFound { path: s() },
			Reason::PermissionDenied { path: s() },
			Reason::EmptySelection,
			Reason::UnsafePath { path: s() },
			Reason::NotRegularFile { path: s() },
			Reason::OutsideRoot { path: s() },
			Reason::Collision { path: s() },
			Reason::AmbiguousRoots {
				basename: ok(),
				roots: vec![s()],
			},
			Reason::PayloadTooLarge {
				limit: 1,
				actual: 2,
				path: Some(ok()),
			},
			Reason::StagedAndWorking { path: ok() },
			Reason::SourceChanged {
				change: StaleChange::Modified { rel: ok() },
			},
			Reason::DestinationChanged {
				change: StaleChange::Created { rel: ok() },
			},
			Reason::CrossRepoCommits,
			Reason::Discontinuous {
				base: ok(),
				tip: ok(),
				at: ok(),
				first_parent: Some(ok()),
			},
			Reason::NotEnoughCommits {
				requested: 3,
				available: 2,
			},
			Reason::NotCommitPayload,
			Reason::InvalidPayload { detail: s() },
			Reason::OutsideScope {
				path: ok(),
				scope: s(),
			},
			Reason::Unverifiable { path: s() },
			Reason::SubmoduleNotCheckedOut,
			Reason::SubmoduleUnreadable { detail: s() },
		];
		// A second sample for the variants whose nested type has its own
		// shapes.
		v.push(Reason::Start {
			start: None,
			detail: String::new(),
		});
		for path in [Some(s()), Some(r"C:\abs\secret".to_string()), None] {
			v.push(Reason::PayloadTooLarge {
				limit: 1,
				actual: 2,
				path,
			});
		}
		v.push(Reason::StagedAndWorking { path: s() });
		for change in [
			StaleChange::Deleted { rel: ok() },
			StaleChange::Repository,
			StaleChange::Preview,
		] {
			v.push(Reason::DestinationChanged { change });
		}
		v
	}

	#[test]
	fn samples_cover_every_variant() {
		let mut seen: Vec<usize> = samples().iter().map(reason_index).collect();
		seen.sort_unstable();
		seen.dedup();
		let max = *seen.last().unwrap();
		assert_eq!(
			seen,
			(0..=max).collect::<Vec<_>>(),
			"a Reason variant has no sample"
		);
	}

	#[test]
	fn every_sample_round_trips_strictly() {
		for r in samples() {
			let json = serde_json::to_string(&r).unwrap();
			let back: Reason = serde_json::from_str(&json).unwrap();
			assert_eq!(back, r, "{json}");
		}
		// Every start cause survives its own spelling.
		for start in [
			StartReason::NoWorker,
			StartReason::KeyLogin,
			StartReason::NoAnswer,
			StartReason::Closed,
		] {
			let r = Reason::Start {
				start: Some(start),
				detail: "d".into(),
			};
			let back: Reason =
				serde_json::from_str(&serde_json::to_string(&r).unwrap())
					.unwrap();
			assert_eq!(back, r);
		}
	}

	#[test]
	fn error_code_keeps_its_wire_spelling() {
		assert_eq!(
			serde_json::to_string(&ErrorCode::OutsideShare).unwrap(),
			"\"outside_share\""
		);
		assert_eq!(
			serde_json::to_string(&ErrorCode::NotARepository).unwrap(),
			"\"not_a_repository\""
		);
	}

	#[derive(Deserialize)]
	struct Frame {
		#[serde(default, deserialize_with = "lenient")]
		reason: Option<Reason>,
	}

	fn lenient_of(json: &str) -> Option<Reason> {
		serde_json::from_str::<Frame>(json)
			.unwrap_or_else(|e| panic!("frame must parse: {e}: {json}"))
			.reason
	}

	#[test]
	fn lenient_reads_what_it_understands_and_nothing_else_as_none() {
		assert_eq!(lenient_of("{}"), None);
		assert_eq!(lenient_of(r#"{"reason":null}"#), None);
		assert_eq!(
			lenient_of(r#"{"reason":{"r":"git_busy"}}"#),
			Some(Reason::GitBusy)
		);
		// A newer peer's variant.
		assert_eq!(lenient_of(r#"{"reason":{"r":"from_the_future"}}"#), None);
		// A known variant with an unknown nested start cause.
		assert_eq!(
			lenient_of(
				r#"{"reason":{"r":"start","start":"from_the_future","detail":""}}"#
			),
			None
		);
		// Not even an object.
		assert_eq!(lenient_of(r#"{"reason":7}"#), None);
	}

	#[test]
	fn for_wire_leaks_no_free_text() {
		for r in samples() {
			let idx = reason_index(&r);
			let wire = r.clone().for_wire();
			let json = serde_json::to_string(&wire).unwrap();
			assert!(
				!json.contains("secret"),
				"variant {idx} sends free text: {json}"
			);
			if let Some(w) = wire {
				// Idempotent: a scrubbed reason scrubs to itself.
				assert_eq!(w.clone().for_wire(), Some(w), "variant {idx}");
			}
		}
	}

	#[test]
	fn for_wire_keeps_the_relative_names_the_ui_echoes() {
		let w = Reason::OutsideScope {
			path: "src/a.rs".into(),
			scope: "/abs/secret".into(),
		}
		.for_wire();
		assert_eq!(
			w,
			Some(Reason::OutsideScope {
				path: "src/a.rs".into(),
				scope: String::new()
			})
		);
		let w = Reason::DestinationChanged {
			change: StaleChange::Modified { rel: "a/b".into() },
		}
		.for_wire();
		assert_eq!(
			w,
			Some(Reason::DestinationChanged {
				change: StaleChange::Modified { rel: "a/b".into() }
			})
		);
		// The file that did not fit is named when it is relative, and the
		// numbers and shas always stay.
		let big = |path: Option<&str>| Reason::PayloadTooLarge {
			limit: 1,
			actual: 2,
			path: path.map(String::from),
		};
		assert_eq!(
			big(Some("repo/a b.txt")).for_wire(),
			Some(big(Some("repo/a b.txt")))
		);
		assert_eq!(big(Some("/abs/a.txt")).for_wire(), Some(big(None)));
		assert_eq!(big(Some("../a.txt")).for_wire(), Some(big(None)));
		assert_eq!(big(None).for_wire(), Some(big(None)));
		let chain = Reason::Discontinuous {
			base: "b".into(),
			tip: "t".into(),
			at: "a".into(),
			first_parent: Some("p".into()),
		};
		assert_eq!(chain.clone().for_wire(), Some(chain));
		assert_eq!(
			Reason::StagedAndWorking {
				path: "f.txt".into()
			}
			.for_wire(),
			Some(Reason::StagedAndWorking {
				path: "f.txt".into()
			})
		);
		// The code already says these.
		for r in [
			Reason::TimedOut,
			Reason::Cancelled,
			Reason::OutsideShare,
			Reason::Refused {
				code: ErrorCode::Stale,
			},
			Reason::NotFound { path: "x".into() },
		] {
			assert_eq!(r.for_wire(), None);
		}
	}

	#[test]
	fn scrubbed_leaks_no_free_text_and_for_wire_is_scrubbed_or_nothing() {
		for r in samples() {
			let idx = reason_index(&r);
			let scrubbed = r.clone().scrubbed();
			let json = serde_json::to_string(&scrubbed).unwrap();
			assert!(
				!json.contains("secret"),
				"variant {idx} keeps free text: {json}"
			);
			assert_eq!(reason_index(&scrubbed), idx, "the variant is kept");
			assert_eq!(scrubbed.clone().scrubbed(), scrubbed, "idempotent");
			match r.for_wire() {
				None => {}
				Some(w) => assert_eq!(w, scrubbed, "variant {idx}"),
			}
		}
	}

	#[derive(Deserialize)]
	struct Rows {
		#[serde(default, deserialize_with = "lenient_vec")]
		reasons: Vec<Option<Reason>>,
	}

	#[test]
	fn lenient_vec_reads_each_element_on_its_own() {
		let rows = |json: &str| -> Vec<Option<Reason>> {
			serde_json::from_str::<Rows>(json)
				.unwrap_or_else(|e| panic!("frame must parse: {e}: {json}"))
				.reasons
		};
		assert_eq!(rows("{}"), vec![]);
		assert_eq!(rows(r#"{"reasons":null}"#), vec![]);
		assert_eq!(rows(r#"{"reasons":7}"#), vec![]);
		assert_eq!(
			rows(
				r#"{"reasons":[{"r":"git_busy"},null,{"r":"from_the_future"},{"r":"timed_out"}]}"#
			),
			vec![Some(Reason::GitBusy), None, None, Some(Reason::TimedOut)]
		);
	}

	#[test]
	fn io_reason_classifies_by_kind_never_by_text() {
		use std::io::{Error, ErrorKind};
		assert_eq!(
			io_reason(&Error::from(ErrorKind::NotFound), "a"),
			Reason::NotFound { path: "a".into() }
		);
		assert_eq!(
			io_reason(&Error::from(ErrorKind::PermissionDenied), "a"),
			Reason::PermissionDenied { path: "a".into() }
		);
		assert_eq!(
			io_reason(&Error::from(ErrorKind::TimedOut), "a"),
			Reason::TimedOut
		);
		// Text that merely sounds like another kind stays Io.
		assert_eq!(
			io_reason(&Error::other("exceeds 1 MiB"), ""),
			Reason::Io {
				detail: "exceeds 1 MiB".into()
			}
		);
		assert_eq!(
			io_reason(&Error::new(ErrorKind::FileTooLarge, "x"), ""),
			Reason::TooLarge
		);
	}
}
