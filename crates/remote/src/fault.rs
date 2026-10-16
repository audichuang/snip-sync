//! Faults a worker injects into its own answers, for tests (#191, #230).
//! Nothing arms one outside tests: no environment switch, and
//! `snip serve --stdio` never calls the entry points.

use std::collections::HashMap;
use std::time::Duration;

use crate::proto::Request;

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

/// What a worker does to one answer.
#[derive(Clone, Debug, PartialEq)]
pub enum Fault {
	/// The work behind the request takes this much longer. It runs where the
	/// work runs, so heartbeats cover it as they cover slow work (protocol 2
	/// and up).
	Delay(Duration),
}

/// Which requests a [`FaultRule`] applies to.
#[derive(Clone, Debug)]
pub enum Match {
	Kind(RequestKind),
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
}

impl FaultBook {
	pub(crate) fn add(&mut self, rule: FaultRule) {
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
		for armed in &mut self.rules {
			let rule = &armed.rule;
			let on = match rule.on {
				Match::Kind(k) => k == kind,
				Match::Any => true,
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
}
