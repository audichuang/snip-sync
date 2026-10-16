//! The `io::Error`s the remote layer raises itself, typed (#233).
//!
//! `RemoteError::Io` carries any `io::Error`, and a front end may show only
//! what the OS said. A sentence this layer wrote ("the worker closed the
//! connection", "frame of N bytes exceeds M") is ours: it is raised as an
//! [`IoFault`], so [`RemoteError::reason`](crate::RemoteError::reason) words
//! it by its variant, and `Display` and `kind()` stay what the CLI and the
//! retry checks have always seen.

use std::io;

use snip_core::reason::Reason;

/// An `io::Error` the remote layer raises itself. `kind()` and `Display` are
/// exactly what the code raised before the type existed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum IoFault {
	/// The stream ended between frames: the worker closed it.
	WorkerClosed,
	/// A frame arrived that is over [`crate::proto::MAX_FRAME`].
	IncomingFrameTooLarge { len: usize, max: usize },
	/// A frame to send that is over [`crate::proto::MAX_FRAME`].
	OutgoingFrameTooLarge { len: usize, max: usize },
	/// A request to send that is over [`crate::proto::JOINED_MAX`].
	OutgoingRequestTooLarge { len: usize, max: usize },
	/// A frame that is not JSON of the expected shape. `text` is serde's own
	/// message: for the CLI and the log, never for a screen.
	BadFrame { text: String },
}

impl IoFault {
	pub(crate) fn kind(&self) -> io::ErrorKind {
		match self {
			Self::WorkerClosed => io::ErrorKind::UnexpectedEof,
			Self::IncomingFrameTooLarge { .. }
			| Self::OutgoingFrameTooLarge { .. }
			| Self::OutgoingRequestTooLarge { .. }
			| Self::BadFrame { .. } => io::ErrorKind::InvalidData,
		}
	}

	/// What this fault stands for, as data. An exhaustive match: a new fault
	/// must decide.
	pub(crate) fn reason(&self) -> Reason {
		match self {
			Self::WorkerClosed => Reason::WorkerClosed,
			Self::IncomingFrameTooLarge { .. } | Self::BadFrame { .. } => {
				Reason::Protocol
			}
			Self::OutgoingFrameTooLarge { len, max }
			| Self::OutgoingRequestTooLarge { len, max } => Reason::PayloadTooLarge {
				limit: *max as u64,
				actual: *len as u64,
				path: None,
			},
		}
	}

	/// The fault inside `e`, if the remote layer raised it.
	pub(crate) fn of(e: &io::Error) -> Option<&IoFault> {
		e.get_ref()?.downcast_ref()
	}
}

impl std::fmt::Display for IoFault {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::WorkerClosed => {
				f.write_str("the worker closed the connection")
			}
			Self::IncomingFrameTooLarge { len, max }
			| Self::OutgoingFrameTooLarge { len, max } => {
				write!(f, "frame of {len} bytes exceeds {max}")
			}
			Self::OutgoingRequestTooLarge { len, max } => {
				write!(f, "request of {len} bytes exceeds {max}")
			}
			Self::BadFrame { text } => f.write_str(text),
		}
	}
}

impl std::error::Error for IoFault {}

impl From<IoFault> for io::Error {
	fn from(fault: IoFault) -> Self {
		io::Error::new(fault.kind(), fault)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::RemoteError;

	/// Every fault, by an exhaustive match: a new one does not compile until
	/// the table below lists it.
	fn fault_index(f: &IoFault) -> usize {
		match f {
			IoFault::WorkerClosed => 0,
			IoFault::IncomingFrameTooLarge { .. } => 1,
			IoFault::OutgoingFrameTooLarge { .. } => 2,
			IoFault::OutgoingRequestTooLarge { .. } => 3,
			IoFault::BadFrame { .. } => 4,
		}
	}

	/// Each fault keeps the kind and the `Display` text the code raised
	/// before the type existed (the CLI prints it, the retry checks read
	/// the kind), and words itself by its variant.
	#[test]
	fn every_io_fault_keeps_its_kind_and_display() {
		let rows = [
			(
				IoFault::WorkerClosed,
				"the worker closed the connection",
				io::ErrorKind::UnexpectedEof,
				Reason::WorkerClosed,
			),
			(
				IoFault::IncomingFrameTooLarge { len: 9, max: 8 },
				"frame of 9 bytes exceeds 8",
				io::ErrorKind::InvalidData,
				Reason::Protocol,
			),
			(
				IoFault::OutgoingFrameTooLarge { len: 9, max: 8 },
				"frame of 9 bytes exceeds 8",
				io::ErrorKind::InvalidData,
				Reason::PayloadTooLarge {
					limit: 8,
					actual: 9,
					path: None,
				},
			),
			(
				IoFault::OutgoingRequestTooLarge { len: 9, max: 8 },
				"request of 9 bytes exceeds 8",
				io::ErrorKind::InvalidData,
				Reason::PayloadTooLarge {
					limit: 8,
					actual: 9,
					path: None,
				},
			),
			(
				IoFault::BadFrame {
					text: "expected value at line 1 column 1".into(),
				},
				"expected value at line 1 column 1",
				io::ErrorKind::InvalidData,
				Reason::Protocol,
			),
		];
		let seen: std::collections::BTreeSet<_> =
			rows.iter().map(|(f, ..)| fault_index(f)).collect();
		assert_eq!(seen.len(), 5, "a fault has no row");
		for (fault, display, kind, reason) in rows {
			let e = io::Error::from(fault.clone());
			assert_eq!(e.to_string(), display);
			assert_eq!(e.kind(), kind);
			assert_eq!(IoFault::of(&e), Some(&fault));
			assert_eq!(RemoteError::Io(e).reason(), reason, "{display}");
		}
		assert_eq!(IoFault::of(&io::Error::other("x")), None);
	}
}
