//! The frames that carry a [`Reason`] with no [`ErrorCode`] beside it.
//!
//! `Response::Error` has a code, so [`Reason::beside_code`] may drop the
//! reasons the code already says. A replay failure, a restore failure of an
//! import and a scan row have none: dropping their reason leaves the master
//! a generic refusal. [`Codeless`] is the type of such a payload, and only
//! [`Codeless::new`] makes one: each payload type scrubs its reasons in one
//! destructuring that names every field, so a field added later cannot be
//! skipped (it does not compile until it is placed here), and a frame
//! cannot be built around a payload that did not pass through it.

use serde::{Deserialize, Serialize};
use snip_core::commits::{ReplayFailure, ReplayResult};
use snip_core::reason::Reason;
use snip_core::restore::{RestoreExecutionResult, RestoreFailure};

use crate::proto::{RepoScan, ScannedRepo};

/// A frame payload with no `ErrorCode` beside its reasons, built only here:
/// every reason in it went through the payload's scrub. Serialized as the
/// bare payload, so the wire is unchanged; `Deserialize` is the master's way
/// in.
///
/// ```compile_fail
/// let _ = snip_remote::Response::Imported(
///     snip_core::restore::RestoreExecutionResult::default(),
/// );
/// ```
/// ```
/// fn read(r: snip_remote::Response) -> Option<snip_core::restore::RestoreExecutionResult> {
///     match r {
///         snip_remote::Response::Imported(c) => Some(c.into_inner()),
///         _ => None,
///     }
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Codeless<T>(T);

impl<T: CarriesReasons> Codeless<T> {
	/// `payload` with every reason in it scrubbed.
	pub(crate) fn new(payload: T) -> Self {
		Self(payload.scrub_reasons())
	}
}

impl<T> Codeless<T> {
	pub fn into_inner(self) -> T {
		self.0
	}
}

impl<T> std::ops::Deref for Codeless<T> {
	type Target = T;

	fn deref(&self) -> &T {
		&self.0
	}
}

mod sealed {
	pub trait Sealed {}
}

/// A payload that holds [`Reason`]s and is sent without a code. Sealed: the
/// three payloads of the protocol implement it, here.
pub trait CarriesReasons: sealed::Sealed + Sized {
	fn scrub_reasons(self) -> Self;
}

/// One reason of a codeless frame: it loses the OS text and the machine's
/// paths, and keeps its variant.
fn scan_reason(reason: Reason) -> Reason {
	reason.scrubbed()
}

impl sealed::Sealed for RepoScan {}
impl CarriesReasons for RepoScan {
	fn scrub_reasons(self) -> Self {
		let RepoScan {
			repos,
			errors,
			error_reasons,
			error_overflow,
			depth_limited,
			depth_overflow,
			status,
		} = self;
		RepoScan {
			repos: repos
				.into_iter()
				.map(|repo| {
					let ScannedRepo {
						rel,
						utf8,
						name,
						kind,
						summary,
						reason,
					} = repo;
					ScannedRepo {
						rel,
						utf8,
						name,
						kind,
						summary,
						reason: reason.map(scan_reason),
					}
				})
				.collect(),
			errors,
			error_reasons: error_reasons
				.into_iter()
				.map(|reason| reason.map(scan_reason))
				.collect(),
			error_overflow,
			depth_limited,
			depth_overflow,
			status,
		}
	}
}

impl sealed::Sealed for RestoreExecutionResult {}
impl CarriesReasons for RestoreExecutionResult {
	fn scrub_reasons(self) -> Self {
		let RestoreExecutionResult {
			created_count,
			overwritten_count,
			skipped_existing_count,
			planned_skipped_count,
			deleted_count,
			errors,
			failures,
			created_dirs,
		} = self;
		RestoreExecutionResult {
			created_count,
			overwritten_count,
			skipped_existing_count,
			planned_skipped_count,
			deleted_count,
			errors,
			created_dirs,
			failures: failures
				.into_iter()
				.map(|failure| {
					let RestoreFailure { path, reason } = failure;
					RestoreFailure {
						path,
						reason: reason.map(scan_reason),
					}
				})
				.collect(),
		}
	}
}

impl sealed::Sealed for ReplayResult {}
impl CarriesReasons for ReplayResult {
	fn scrub_reasons(self) -> Self {
		let ReplayResult { created, failure } = self;
		ReplayResult {
			created,
			failure: failure.map(|failure| {
				let ReplayFailure {
					index,
					message,
					error,
					layout_conflict,
					conflict_path,
					reason,
				} = failure;
				ReplayFailure {
					index,
					message,
					error,
					layout_conflict,
					conflict_path,
					reason: reason.map(scan_reason),
				}
			}),
		}
	}
}

#[cfg(test)]
mod tests {
	use std::mem::discriminant;

	use snip_core::reason::test_support::samples;

	use super::*;
	use crate::proto::Response;

	/// The payload a codeless `Response` carries, by an exhaustive match: a
	/// new variant of the protocol does not compile until it is decided
	/// whether it carries reasons without a code.
	fn codeless_payload(r: &Response) -> Option<&'static str> {
		match r {
			Response::Repos(_) => Some("repos"),
			Response::Imported(_) => Some("imported"),
			Response::Replayed(_) => Some("replayed"),
			Response::Hello { .. }
			| Response::Workspace(_)
			| Response::Dir { .. }
			| Response::Stat(_)
			| Response::Text { .. }
			| Response::Pending
			| Response::Git(_)
			| Response::Copied(_)
			| Response::CommitsCopied(_)
			| Response::Chunk { .. }
			| Response::Joined
			| Response::ImportPlanned(_)
			| Response::ImportPrepared(_)
			| Response::ReplayPlanned(_)
			| Response::ReplayPrepared(_)
			| Response::ReplayRead(_)
			| Response::Fresh
			| Response::Error { .. } => None,
		}
	}

	/// Every frame kind without a code, against every reason shape: the
	/// variant survives the scrub, and no text of the machine does.
	#[test]
	fn codeless_frames_keep_every_reason() {
		for r in samples() {
			let kept = discriminant(&r);
			let secret = |wire: &Response| {
				let json = serde_json::to_string(wire).unwrap();
				assert!(!json.contains("secret"), "{r:?}: {json}");
				let back: Response = serde_json::from_str(&json).unwrap();
				back
			};

			let scan = RepoScan {
				repos: vec![ScannedRepo {
					rel: "a".into(),
					utf8: true,
					name: "a".into(),
					kind: snip_core::gitview::FoundKind::Main,
					summary: Err("x".into()),
					reason: Some(r.clone()),
				}],
				errors: vec![("a".into(), "x".into())],
				error_reasons: vec![Some(r.clone())],
				error_overflow: 0,
				depth_limited: Vec::new(),
				depth_overflow: 0,
				status: snip_core::workspace::ScanStatus::Complete,
			};
			let Response::Repos(back) =
				secret(&Response::Repos(Codeless::new(scan)))
			else {
				panic!("a scan came back as another reply");
			};
			assert_eq!(
				back.repos[0].reason.as_ref().map(discriminant),
				Some(kept),
				"{r:?}"
			);
			assert_eq!(
				back.error_reasons[0].as_ref().map(discriminant),
				Some(kept),
				"{r:?}"
			);

			let restore = RestoreExecutionResult {
				failures: vec![RestoreFailure {
					path: "a".into(),
					reason: Some(r.clone()),
				}],
				..Default::default()
			};
			let Response::Imported(back) =
				secret(&Response::Imported(Codeless::new(restore)))
			else {
				panic!("an import came back as another reply");
			};
			assert_eq!(
				back.failures[0].reason.as_ref().map(discriminant),
				Some(kept),
				"{r:?}"
			);

			let replay = ReplayResult {
				created: Vec::new(),
				failure: Some(ReplayFailure {
					index: 0,
					message: String::new(),
					error: String::new(),
					layout_conflict: None,
					conflict_path: None,
					reason: Some(r.clone()),
				}),
			};
			let Response::Replayed(back) =
				secret(&Response::Replayed(Codeless::new(replay)))
			else {
				panic!("a replay came back as another reply");
			};
			assert_eq!(
				back.failure
					.as_ref()
					.unwrap()
					.reason
					.as_ref()
					.map(discriminant),
				Some(kept),
				"{r:?}"
			);
		}
	}

	#[test]
	fn every_response_variant_is_classified() {
		let carriers = [
			Response::Repos(Codeless::new(RepoScan {
				repos: Vec::new(),
				errors: Vec::new(),
				error_reasons: Vec::new(),
				error_overflow: 0,
				depth_limited: Vec::new(),
				depth_overflow: 0,
				status: snip_core::workspace::ScanStatus::Complete,
			})),
			Response::Imported(Codeless::new(Default::default())),
			Response::Replayed(Codeless::new(Default::default())),
		];
		let names: Vec<_> =
			carriers.iter().map(|r| codeless_payload(r)).collect();
		assert_eq!(names, [Some("repos"), Some("imported"), Some("replayed")]);
		assert_eq!(codeless_payload(&Response::Pending), None);
	}
}
