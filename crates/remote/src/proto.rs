//! Wire format: one JSON value per frame, each frame a big-endian `u32`
//! length followed by that many bytes. Frames are capped on both sides, so
//! a peer can never make the other allocate more than [`MAX_FRAME`].

use std::io::{self, Read, Write};

use serde::{de::DeserializeOwned, Deserialize, Serialize};

/// Bumped on any change a peer of the previous version would misread.
pub const PROTOCOL_VERSION: u32 = 1;

/// Largest frame either side sends or accepts. A preview is at most 1 MiB
/// of text, and JSON escaping can grow it several times.
pub const MAX_FRAME: usize = 8 * 1024 * 1024;

/// Entries one `ListDir` reply carries at most; the rest is reported as
/// `truncated`.
pub const MAX_DIR_ENTRIES: usize = 1000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
	/// First frame of every connection.
	Hello {
		version: u32,
		name: String,
	},
	/// Turns an unknown master into a trusted one. `proof` is
	/// [`crate::tls::pairing_proof`] in hex.
	Pair {
		name: String,
		proof: String,
	},
	ListWorkspaces,
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
	Git {
		workspace: String,
		args: Vec<String>,
	},
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Response {
	Hello {
		version: u32,
		name: String,
		/// Whether this connection's certificate is already trusted.
		paired: bool,
	},
	Paired {
		name: String,
	},
	Workspaces {
		items: Vec<RemoteWorkspace>,
	},
	Dir {
		entries: Vec<DirEntry>,
		truncated: bool,
	},
	Stat(Stat),
	/// `content` is `None` for a binary or non-UTF-8 file, as local preview.
	Text {
		content: Option<String>,
	},
	Error {
		code: ErrorCode,
		message: String,
	},
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteWorkspace {
	/// Stable for the same folder: derived from its real path.
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
	/// The request needs a paired master.
	NotPaired,
	/// No pairing code is open, or the proof did not match it.
	PairingRefused,
	/// Outside every shared workspace.
	Forbidden,
	NotFound,
	Unsupported,
	BadRequest,
	TooLarge,
	Io,
	VersionMismatch,
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

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn frames_round_trip_and_end_cleanly() {
		let mut buf = Vec::new();
		let req = Request::ListDir {
			workspace: "w".into(),
			path: "src/多行".into(),
		};
		write_frame(&mut buf, &req).unwrap();
		write_frame(&mut buf, &Request::ListWorkspaces).unwrap();
		let mut r = buf.as_slice();
		assert_eq!(read_frame::<Request>(&mut r).unwrap(), Some(req));
		assert_eq!(
			read_frame::<Request>(&mut r).unwrap(),
			Some(Request::ListWorkspaces)
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
		write_frame(&mut buf, &Request::ListWorkspaces).unwrap();
		buf.pop();
		assert!(read_frame::<Request>(&mut buf.as_slice()).is_err());
	}
}
