//! Faults a worker injects into its own answers, for tests (#191, #230).
//! Nothing arms one outside tests: no environment switch, and
//! `snip serve --stdio` never calls the entry points.

use std::collections::HashMap;
use std::time::Duration;

use crate::proto::{ErrorCode, Request};

/// What a [`Request`] is, without its payload: what a [`FaultRule`] matches on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RequestKind {
	Hello,
	OpenWorkspace,
	ListDir,
	Stat,
	Read,
	ReplayRead,
	Write,
	Rename,
	ScanRepos,
	GitView,
	Export,
	ExportCommits,
	ExportChanges,
	ImportPlan,
	PrepareImport,
	ApplyPrepared,
	ImportApply,
	ReplayPlan,
	ReplayApply,
	PrepareReplay,
	ApplyPreparedReplay,
	Chunk,
	FrameChunk,
	FrameJoin,
}

/// Every [`RequestKind`], in declaration order.
pub const ALL_REQUEST_KINDS: [RequestKind; 24] = [
	RequestKind::Hello,
	RequestKind::OpenWorkspace,
	RequestKind::ListDir,
	RequestKind::Stat,
	RequestKind::Read,
	RequestKind::ReplayRead,
	RequestKind::Write,
	RequestKind::Rename,
	RequestKind::ScanRepos,
	RequestKind::GitView,
	RequestKind::Export,
	RequestKind::ExportCommits,
	RequestKind::ExportChanges,
	RequestKind::ImportPlan,
	RequestKind::PrepareImport,
	RequestKind::ApplyPrepared,
	RequestKind::ImportApply,
	RequestKind::ReplayPlan,
	RequestKind::ReplayApply,
	RequestKind::PrepareReplay,
	RequestKind::ApplyPreparedReplay,
	RequestKind::Chunk,
	RequestKind::FrameChunk,
	RequestKind::FrameJoin,
];

impl Request {
	/// The request's kind. An exhaustive match: a new request must decide.
	pub fn kind(&self) -> RequestKind {
		match self {
			Self::Hello { .. } => RequestKind::Hello,
			Self::OpenWorkspace { .. } => RequestKind::OpenWorkspace,
			Self::ListDir { .. } => RequestKind::ListDir,
			Self::Stat { .. } => RequestKind::Stat,
			Self::Read { .. } => RequestKind::Read,
			Self::ReplayRead { .. } => RequestKind::ReplayRead,
			Self::Write { .. } => RequestKind::Write,
			Self::Rename { .. } => RequestKind::Rename,
			Self::ScanRepos { .. } => RequestKind::ScanRepos,
			Self::GitView { .. } => RequestKind::GitView,
			Self::Export { .. } => RequestKind::Export,
			Self::ExportCommits { .. } => RequestKind::ExportCommits,
			Self::ExportChanges { .. } => RequestKind::ExportChanges,
			Self::ImportPlan { .. } => RequestKind::ImportPlan,
			Self::PrepareImport { .. } => RequestKind::PrepareImport,
			Self::ApplyPrepared { .. } => RequestKind::ApplyPrepared,
			Self::ImportApply { .. } => RequestKind::ImportApply,
			Self::ReplayPlan { .. } => RequestKind::ReplayPlan,
			Self::ReplayApply { .. } => RequestKind::ReplayApply,
			Self::PrepareReplay { .. } => RequestKind::PrepareReplay,
			Self::ApplyPreparedReplay { .. } => {
				RequestKind::ApplyPreparedReplay
			}
			Self::Chunk { .. } => RequestKind::Chunk,
			Self::FrameChunk { .. } => RequestKind::FrameChunk,
			Self::FrameJoin => RequestKind::FrameJoin,
		}
	}

	/// The workspace the request names; none for those that name none.
	/// An exhaustive match: a new request must decide.
	pub fn workspace(&self) -> Option<&str> {
		match self {
			Self::Hello { .. }
			| Self::OpenWorkspace { .. }
			| Self::Chunk { .. }
			| Self::FrameChunk { .. }
			| Self::FrameJoin => None,
			Self::ListDir { workspace, .. }
			| Self::Stat { workspace, .. }
			| Self::Read { workspace, .. }
			| Self::ReplayRead { workspace, .. }
			| Self::Write { workspace, .. }
			| Self::Rename { workspace, .. }
			| Self::ScanRepos { workspace, .. }
			| Self::GitView { workspace, .. }
			| Self::Export { workspace, .. }
			| Self::ExportCommits { workspace, .. }
			| Self::ExportChanges { workspace, .. }
			| Self::ImportPlan { workspace, .. }
			| Self::PrepareImport { workspace, .. }
			| Self::ApplyPrepared { workspace, .. }
			| Self::ImportApply { workspace, .. }
			| Self::ReplayPlan { workspace, .. }
			| Self::ReplayApply { workspace, .. }
			| Self::PrepareReplay { workspace, .. }
			| Self::ApplyPreparedReplay { workspace, .. } => Some(workspace),
		}
	}
}

/// How the worker serves a [`RequestKind`]: where a [`Fault`] can act on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
	/// Answered before dispatch.
	Hello,
	/// `State::handle` does the work, under heartbeats (`run_inline`):
	/// OpenWorkspace, ListDir, Stat, Read.
	Inline,
	/// Refused at once, with no work behind it: Write, Rename.
	RefusedAtOnce,
	/// Served as a job (`run_job`, `run_job_with`, `serve_copy`).
	Job,
	/// A piece of a request, never dispatched alone.
	Framing,
}

impl RequestKind {
	/// How the worker serves this kind. An exhaustive match: a new request
	/// must decide.
	pub fn class(self) -> Class {
		match self {
			Self::Hello => Class::Hello,
			Self::OpenWorkspace | Self::ListDir | Self::Stat | Self::Read => {
				Class::Inline
			}
			Self::Write | Self::Rename => Class::RefusedAtOnce,
			Self::ReplayRead
			| Self::ScanRepos
			| Self::GitView
			| Self::Export
			| Self::ExportCommits
			| Self::ExportChanges
			| Self::ImportPlan
			| Self::PrepareImport
			| Self::ApplyPrepared
			| Self::ImportApply
			| Self::ReplayPlan
			| Self::ReplayApply
			| Self::PrepareReplay
			| Self::ApplyPreparedReplay => Class::Job,
			Self::Chunk | Self::FrameChunk | Self::FrameJoin => Class::Framing,
		}
	}

	/// Whether the request can write to the workspace (a master never sends
	/// it twice). `ReplayApply` and `ApplyPreparedReplay` write only when not
	/// `check_only`: a test sends them with `check_only: false`.
	pub fn may_write(self) -> bool {
		match self {
			Self::ApplyPrepared
			| Self::ImportApply
			| Self::ReplayApply
			| Self::ApplyPreparedReplay => true,
			Self::Hello
			| Self::OpenWorkspace
			| Self::ListDir
			| Self::Stat
			| Self::Read
			| Self::ReplayRead
			| Self::Write
			| Self::Rename
			| Self::ScanRepos
			| Self::GitView
			| Self::Export
			| Self::ExportCommits
			| Self::ExportChanges
			| Self::ImportPlan
			| Self::PrepareImport
			| Self::ReplayPlan
			| Self::PrepareReplay
			| Self::Chunk
			| Self::FrameChunk
			| Self::FrameJoin => false,
		}
	}
}

/// What a worker does to one answer. The layer a variant acts on is named
/// on it; a heartbeat is never touched, only the answer is.
#[derive(Clone, Debug, PartialEq)]
pub enum Fault {
	/// The work behind the request takes this much longer. It runs where the
	/// work runs, so heartbeats cover it as they cover slow work (protocol 2
	/// and up).
	Delay(Duration),
	/// Each unit of work behind the request takes this much longer: a page
	/// of a folder listing, or the one call of a request that is a single
	/// call. Like a slow filesystem call it is not interrupted by a cancel,
	/// so a test sees what the work does once its deadline has passed.
	PageDelay(Duration),
	/// Dispatch layer: answers with this refusal (`"injected fault"`, no
	/// reason, as an older worker would) and does no work.
	Refuse(ErrorCode),
	/// Dispatch layer: hangs up without doing any work.
	HangUp,
	/// Dispatch layer: writes a frame that is not JSON, then hangs up.
	Garbage,
	/// Dispatch layer: sends one heartbeat, then hangs up.
	HeartbeatThenHangUp,
	/// Dispatch layer: waits this long, silent, then serves the request as
	/// usual.
	Stall(Duration),
	/// Reply layer: does the work, drops the whole answer, then hangs up. A
	/// write's outcome is unknown to the master.
	RunThenHangUp,
	/// Reply layer: writes the first half of the answer's bytes, then hangs
	/// up.
	TruncatedReply,
	/// Reply layer: writes the answer whole, `chunk` bytes at a time, with a
	/// real pause between pieces.
	Throttle { chunk: usize, pause: Duration },
}

impl Fault {
	/// The reply-layer writer this fault asks for, if it is one.
	pub(crate) fn reply_writer(&self) -> Option<FaultWriter> {
		match self {
			Self::RunThenHangUp => Some(FaultWriter::Discard),
			Self::TruncatedReply => Some(FaultWriter::Truncate),
			Self::Throttle { chunk, pause } => Some(FaultWriter::Throttle {
				chunk: (*chunk).max(1),
				pause: *pause,
			}),
			Self::Delay(_)
			| Self::PageDelay(_)
			| Self::Refuse(_)
			| Self::HangUp
			| Self::Garbage
			| Self::HeartbeatThenHangUp
			| Self::Stall(_) => None,
		}
	}

	/// Whether a Hello can meet this fault: it is answered before dispatch,
	/// so only the ways a start fails apply.
	fn applies_to_hello(&self) -> bool {
		match self {
			Self::Refuse(_) | Self::HangUp | Self::Garbage => true,
			Self::Delay(_)
			| Self::PageDelay(_)
			| Self::HeartbeatThenHangUp
			| Self::Stall(_)
			| Self::RunThenHangUp
			| Self::TruncatedReply
			| Self::Throttle { .. } => false,
		}
	}
}

/// How a reply-layer [`Fault`] writes the bytes of an answer already
/// rendered. It needs no knowledge of the frames inside.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FaultWriter {
	/// Writes nothing.
	Discard,
	/// Writes the first half.
	Truncate,
	/// Writes all of it, `chunk` bytes at a time, pausing between pieces.
	Throttle { chunk: usize, pause: Duration },
}

impl FaultWriter {
	/// Whether the connection ends after this writer ran.
	pub(crate) fn hangs_up(self) -> bool {
		match self {
			Self::Discard | Self::Truncate => true,
			Self::Throttle { .. } => false,
		}
	}

	pub(crate) fn write(
		self,
		out: &mut impl std::io::Write,
		bytes: &[u8],
	) -> std::io::Result<()> {
		match self {
			Self::Discard => Ok(()),
			Self::Truncate => {
				out.write_all(&bytes[..bytes.len() / 2])?;
				out.flush()
			}
			Self::Throttle { chunk, pause } => {
				let mut pieces = bytes.chunks(chunk.max(1)).peekable();
				while let Some(piece) = pieces.next() {
					out.write_all(piece)?;
					out.flush()?;
					if pieces.peek().is_some() {
						std::thread::sleep(pause);
					}
				}
				Ok(())
			}
		}
	}
}

/// Which requests a [`FaultRule`] applies to.
#[derive(Clone, Debug)]
pub enum Match {
	Kind(RequestKind),
	/// Every dispatched request. A Hello is not dispatched: name it with
	/// `Kind(RequestKind::Hello)`.
	Any,
}

/// A fault and the requests it falls on: the ones that match `on` (and
/// `workspace`, when set), after `skip` of them went by, for `times` of them.
#[derive(Clone, Debug)]
pub struct FaultRule {
	pub on: Match,
	pub workspace: Option<String>,
	pub skip: usize,
	pub times: usize,
	pub fault: Fault,
}

struct Armed {
	rule: FaultRule,
	/// Requests this rule has matched so far.
	matched: usize,
}

#[derive(Default)]
pub(crate) struct FaultBook {
	rules: Vec<Armed>,
	seen: HashMap<RequestKind, usize>,
	/// Requests that name a workspace, counted by that workspace too.
	seen_in: HashMap<(RequestKind, String), usize>,
	/// Workspaces whose first successful open is recorded, and the order it
	/// happened in. Only workspaces a test named: storage stays bounded.
	watched: HashMap<String, Option<usize>>,
}

impl FaultBook {
	pub(crate) fn add(&mut self, rule: FaultRule) {
		if matches!(rule.on, Match::Kind(RequestKind::Hello)) {
			assert!(
				rule.fault.applies_to_hello(),
				"a Hello is answered before dispatch: only Refuse, HangUp and Garbage apply to it, not {:?}",
				rule.fault
			);
		}
		self.rules.push(Armed { rule, matched: 0 });
	}

	/// Drops every rule. The counts of requests seen stay.
	pub(crate) fn clear(&mut self) {
		self.rules.clear();
	}

	/// Counts `request` and returns the fault of the first rule that matches
	/// it and has turns left: a match first spends `skip`, then `times`. A
	/// match that is still skipping lets the search go on to later rules.
	pub(crate) fn take(&mut self, request: &Request) -> Option<Fault> {
		let kind = request.kind();
		*self.seen.entry(kind).or_default() += 1;
		if let Some(ws) = request.workspace() {
			*self.seen_in.entry((kind, ws.to_string())).or_default() += 1;
		}
		for armed in &mut self.rules {
			let rule = &armed.rule;
			let on = match rule.on {
				Match::Kind(k) => k == kind,
				Match::Any => kind != RequestKind::Hello,
			};
			let workspace = match &rule.workspace {
				Some(w) => request.workspace() == Some(w.as_str()),
				None => true,
			};
			if !on || !workspace {
				continue;
			}
			armed.matched += 1;
			let turn = armed.matched;
			if turn > rule.skip && turn - rule.skip <= rule.times {
				return Some(rule.fault.clone());
			}
		}
		None
	}

	/// Requests of `kind` that [`Self::take`] has counted.
	pub(crate) fn seen(&self, kind: RequestKind) -> usize {
		self.seen.get(&kind).copied().unwrap_or(0)
	}

	/// Requests of `kind` naming `workspace` that [`Self::take`] has counted.
	pub(crate) fn seen_in(&self, kind: RequestKind, workspace: &str) -> usize {
		self.seen_in
			.get(&(kind, workspace.to_string()))
			.copied()
			.unwrap_or(0)
	}

	/// Starts the count of `kind` in `workspace` over. [`Self::clear`] leaves
	/// it alone.
	pub(crate) fn reset_seen_in(&mut self, kind: RequestKind, workspace: &str) {
		self.seen_in.remove(&(kind, workspace.to_string()));
	}

	/// Drops the rules that name exactly `kind` in `workspace`.
	pub(crate) fn remove_rules(&mut self, kind: RequestKind, workspace: &str) {
		self.rules.retain(|armed| {
			let rule = &armed.rule;
			!(matches!(rule.on, Match::Kind(k) if k == kind)
				&& rule.workspace.as_deref() == Some(workspace))
		});
	}

	/// Records the first successful open of `workspace` from now on.
	pub(crate) fn watch(&mut self, workspace: &str) {
		self.watched.insert(workspace.to_string(), None);
	}

	/// A successful open of `workspace`: kept (with `order()`) only the
	/// first time, and only for a watched workspace.
	pub(crate) fn note_open(
		&mut self,
		workspace: &str,
		order: impl FnOnce() -> usize,
	) {
		if let Some(slot) = self.watched.get_mut(workspace) {
			slot.get_or_insert_with(order);
		}
	}

	/// Watched workspaces that were opened, in the order of their first
	/// open.
	pub(crate) fn opened(&self) -> Vec<String> {
		let mut opened: Vec<_> = self
			.watched
			.iter()
			.filter_map(|(ws, order)| order.map(|o| (o, ws.clone())))
			.collect();
		opened.sort();
		opened.into_iter().map(|(_, ws)| ws).collect()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn read(workspace: &str) -> Request {
		Request::Read {
			workspace: workspace.into(),
			path: "a.txt".into(),
		}
	}

	fn delay() -> Fault {
		Fault::Delay(Duration::from_millis(5))
	}

	#[test]
	fn fault_rules_spend_skip_then_times_and_match_workspace() {
		let mut book = FaultBook::default();
		book.add(FaultRule {
			on: Match::Any,
			workspace: None,
			skip: 1,
			times: 2,
			fault: delay(),
		});
		assert_eq!(book.take(&read("w")), None);
		assert_eq!(book.take(&read("w")), Some(delay()));
		assert_eq!(book.take(&read("w")), Some(delay()));
		assert_eq!(book.take(&read("w")), None);
		assert_eq!(book.seen(RequestKind::Read), 4);
		assert_eq!(book.seen(RequestKind::Stat), 0);

		book.clear();
		book.add(FaultRule {
			on: Match::Kind(RequestKind::Read),
			workspace: Some("w".into()),
			skip: 0,
			times: 5,
			fault: delay(),
		});
		// Another workspace, and another kind, do not match.
		assert_eq!(book.take(&read("other")), None);
		assert_eq!(
			book.take(&Request::Stat {
				workspace: "w".into(),
				path: "a.txt".into(),
			}),
			None
		);
		assert_eq!(book.take(&read("w")), Some(delay()));
		// The counts survive a clear and keep counting.
		assert_eq!(book.seen(RequestKind::Read), 6);
	}

	#[test]
	fn every_request_kind_is_listed_once() {
		assert_eq!(ALL_REQUEST_KINDS.len(), 24);
		let distinct: std::collections::HashSet<_> =
			ALL_REQUEST_KINDS.iter().collect();
		assert_eq!(distinct.len(), ALL_REQUEST_KINDS.len());
		for request in [
			read("w"),
			Request::FrameJoin,
			Request::Hello {
				version: 1,
				name: "t".into(),
				max_version: None,
			},
		] {
			assert!(ALL_REQUEST_KINDS.contains(&request.kind()));
		}
		assert_eq!(read("w").workspace(), Some("w"));
		assert_eq!(Request::FrameJoin.workspace(), None);
	}

	#[test]
	fn every_request_kind_has_a_class() {
		let count = |c: Class| {
			ALL_REQUEST_KINDS
				.into_iter()
				.filter(|k| k.class() == c)
				.count()
		};
		assert_eq!(count(Class::Hello), 1);
		assert_eq!(count(Class::Inline), 4);
		assert_eq!(count(Class::RefusedAtOnce), 2);
		assert_eq!(count(Class::Job), 14);
		assert_eq!(count(Class::Framing), 3);
		let writers: Vec<_> = ALL_REQUEST_KINDS
			.into_iter()
			.filter(|k| k.may_write())
			.collect();
		assert_eq!(
			writers,
			[
				RequestKind::ApplyPrepared,
				RequestKind::ImportApply,
				RequestKind::ReplayApply,
				RequestKind::ApplyPreparedReplay,
			]
		);
		// A writer is a job: only a job can have written something.
		assert!(writers.iter().all(|k| k.class() == Class::Job));
	}

	#[test]
	fn fault_writer_discards_halves_or_paces_the_reply() {
		let bytes: Vec<u8> = (0..100u8).collect();
		let mut out = Vec::new();
		FaultWriter::Discard.write(&mut out, &bytes).unwrap();
		assert!(out.is_empty());
		FaultWriter::Truncate.write(&mut out, &bytes).unwrap();
		assert_eq!(out, bytes[..50]);

		/// Records each `write_all` as one piece.
		#[derive(Default)]
		struct Pieces(Vec<Vec<u8>>);
		impl std::io::Write for Pieces {
			fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
				self.0.push(buf.to_vec());
				Ok(buf.len())
			}
			fn flush(&mut self) -> std::io::Result<()> {
				Ok(())
			}
		}
		let mut pieces = Pieces::default();
		let started = std::time::Instant::now();
		FaultWriter::Throttle {
			chunk: 30,
			pause: Duration::from_millis(20),
		}
		.write(&mut pieces, &bytes)
		.unwrap();
		assert_eq!(pieces.0.len(), 100usize.div_ceil(30));
		assert_eq!(pieces.0.concat(), bytes);
		assert!(
			started.elapsed() >= Duration::from_millis(60),
			"three pauses between four pieces"
		);
		assert!(FaultWriter::Discard.hangs_up());
		assert!(FaultWriter::Truncate.hangs_up());
		assert!(!FaultWriter::Throttle {
			chunk: 1,
			pause: Duration::ZERO
		}
		.hangs_up());
	}

	#[test]
	fn a_reply_layer_fault_touches_the_answer_and_never_a_heartbeat() {
		use crate::proto::{write_frame, Response};
		let mut whole = Vec::new();
		write_frame(&mut whole, &Response::Pending).unwrap();
		let heartbeat = whole.clone();
		crate::jobs::write_response(&mut whole, &Response::Pending).unwrap();
		let reply = whole[heartbeat.len()..].to_vec();

		for (fault, want_reply, hangs_up) in [
			(Fault::RunThenHangUp, Vec::new(), true),
			(
				Fault::TruncatedReply,
				reply[..reply.len() / 2].to_vec(),
				true,
			),
			(
				Fault::Throttle {
					chunk: 3,
					pause: Duration::ZERO,
				},
				reply.clone(),
				false,
			),
		] {
			let mut out = Vec::new();
			let mut answer =
				crate::jobs::Answer::new(&mut out, Some(fault.clone()));
			answer.heartbeat().unwrap();
			assert!(!answer.hang_up());
			answer.reply(&Response::Pending).unwrap();
			assert_eq!(answer.hang_up(), hangs_up, "{fault:?}");
			let mut want = heartbeat.clone();
			want.extend(want_reply);
			assert_eq!(out, want, "{fault:?}");
		}
		// Dispatch-layer faults leave the reply alone.
		let mut out = Vec::new();
		let mut answer = crate::jobs::Answer::new(
			&mut out,
			Some(Fault::Stall(Duration::ZERO)),
		);
		answer.reply(&Response::Pending).unwrap();
		assert!(!answer.hang_up());
		assert_eq!(out, reply);
	}

	#[test]
	fn seen_in_counts_by_workspace_and_a_scan_rule_set_replaces_the_last() {
		let scan = |ws: &str| Request::ScanRepos {
			workspace: ws.into(),
			under: None,
		};
		let mut book = FaultBook::default();
		book.add(FaultRule {
			on: Match::Kind(RequestKind::ScanRepos),
			workspace: Some("w".into()),
			skip: 0,
			times: 1,
			fault: Fault::HangUp,
		});
		book.add(FaultRule {
			on: Match::Kind(RequestKind::ScanRepos),
			workspace: Some("w".into()),
			skip: 0,
			times: usize::MAX,
			fault: Fault::Refuse(ErrorCode::Busy),
		});
		assert_eq!(book.take(&scan("w")), Some(Fault::HangUp));
		assert_eq!(book.take(&scan("w")), Some(Fault::Refuse(ErrorCode::Busy)));
		assert_eq!(book.take(&scan("x")), None);
		assert_eq!(book.seen_in(RequestKind::ScanRepos, "w"), 2);
		assert_eq!(book.seen_in(RequestKind::ScanRepos, "x"), 1);
		assert_eq!(book.seen(RequestKind::ScanRepos), 3);
		book.remove_rules(RequestKind::ScanRepos, "w");
		book.reset_seen_in(RequestKind::ScanRepos, "w");
		assert_eq!(book.take(&scan("w")), None);
		assert_eq!(book.seen_in(RequestKind::ScanRepos, "w"), 1);

		book.watch("w");
		book.note_open("other", || 0);
		book.note_open("w", || 7);
		book.note_open("w", || 9);
		assert_eq!(book.opened(), ["w"]);
	}

	#[test]
	fn a_hello_takes_only_the_faults_that_make_a_start_fail() {
		let hello = Request::Hello {
			version: 1,
			name: "t".into(),
			max_version: None,
		};
		let mut book = FaultBook::default();
		// Any never matches a hello: it is not dispatched.
		book.add(FaultRule {
			on: Match::Any,
			workspace: None,
			skip: 0,
			times: 1,
			fault: Fault::HangUp,
		});
		assert_eq!(book.take(&hello), None);
		book.add(FaultRule {
			on: Match::Kind(RequestKind::Hello),
			workspace: None,
			skip: 0,
			times: 1,
			fault: Fault::Garbage,
		});
		assert_eq!(book.take(&hello), Some(Fault::Garbage));
		assert_eq!(book.seen(RequestKind::Hello), 2);
		let result = std::panic::catch_unwind(|| {
			FaultBook::default().add(FaultRule {
				on: Match::Kind(RequestKind::Hello),
				workspace: None,
				skip: 0,
				times: 1,
				fault: Fault::Throttle {
					chunk: 1,
					pause: Duration::ZERO,
				},
			});
		});
		assert!(result.is_err(), "a Throttle on a Hello means nothing");
	}
}
