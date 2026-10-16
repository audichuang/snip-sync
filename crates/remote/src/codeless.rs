//! The frames that carry a [`Reason`] with no [`ErrorCode`] beside it.
//!
//! `Response::Error` has a code, so [`Reason::beside_code`] may drop the
//! reasons the code already says. A replay failure, a restore failure of an
//! import and a scan row have none: dropping their reason leaves the master
//! a generic refusal. Each of those frames gets its reason from exactly one
//! function here, and `codeless_frames_keep_every_reason` walks them all
//! over every [`Reason`] shape, so a function that used `beside_code` (or
//! any rule that loses a variant) fails that test.

use snip_core::commits::ReplayResult;
use snip_core::reason::Reason;
use snip_core::restore::RestoreExecutionResult;

/// The reason of a scan row (`ScannedRepo.reason`, `RepoScan.error_reasons`).
pub(crate) fn scan_reason(reason: Reason) -> Reason {
	reason.scrubbed()
}

/// A restore result of `Response::Imported`: each failure's reason loses the
/// OS text and the machine's paths.
pub(crate) fn restore_result(
	mut result: RestoreExecutionResult,
) -> RestoreExecutionResult {
	for f in &mut result.failures {
		f.reason = f.reason.take().map(scan_reason);
	}
	result
}

/// A replay result of `Response::Replayed`: the failure's reason names no
/// path of this machine.
pub(crate) fn replay_result(mut result: ReplayResult) -> ReplayResult {
	if let Some(failure) = &mut result.failure {
		failure.reason = failure.reason.take().map(scan_reason);
	}
	result
}

#[cfg(test)]
mod tests {
	use std::mem::discriminant;

	use snip_core::commits::ReplayFailure;
	use snip_core::reason::test_support::samples;
	use snip_core::restore::RestoreFailure;

	use super::*;

	/// Every frame kind without a code, against every reason shape: the
	/// variant survives. Add a new codeless frame here when it is added to
	/// the protocol.
	#[test]
	fn codeless_frames_keep_every_reason() {
		for r in samples() {
			let kept = discriminant(&r);

			assert_eq!(discriminant(&scan_reason(r.clone())), kept, "{r:?}");

			let restore = RestoreExecutionResult {
				failures: vec![RestoreFailure {
					path: "a".into(),
					reason: Some(r.clone()),
				}],
				..Default::default()
			};
			let wired = restore_result(restore).failures.remove(0).reason;
			assert_eq!(wired.as_ref().map(discriminant), Some(kept), "{r:?}");

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
			let wired = replay_result(replay).failure.unwrap().reason;
			assert_eq!(wired.as_ref().map(discriminant), Some(kept), "{r:?}");
		}
	}
}
