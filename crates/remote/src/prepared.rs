//! Bounded, one-use file payload retention. Never stores an executable plan.

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::proto::{ErrorCode, PasteMapping};

/// The worker's refusal for a token it does not hold (expired, already
/// consumed, or issued by another worker). Nothing was written, so the
/// client may send the full text once through `ImportApply`.
pub(crate) const TOKEN_GONE: &str = "prepared paste expired, was consumed, or belongs to another worker; preview again";

/// A refusal from before the first write of an Apply: lock acquisition,
/// freshness re-validation and re-planning. Busy and Timeout come only
/// from there, so the payload stays retained and the preview can apply
/// again. Stale, Io and the rest are not retryable here.
pub(crate) fn refused_before_write(code: ErrorCode) -> bool {
	matches!(code, ErrorCode::Busy | ErrorCode::Timeout)
}

const MAX_BYTES: usize = 128 * 1024 * 1024;
const MAX_ENTRIES: usize = 8;
const TTL: Duration = Duration::from_secs(5 * 60);

pub(crate) struct CachedImport {
	pub text: String,
	pub mapping: PasteMapping,
	pub workspace: PathBuf,
	pub destination: PathBuf,
	pub digest: String,
	/// A commit replay keeps the worker's own captured preview, which holds
	/// the parsed payload, and leaves `text` empty. None for file pastes.
	pub replay: Option<Box<snip_core::transfer::CommitReplayPreview>>,
}

impl CachedImport {
	fn bytes(&self) -> usize {
		// Include routing/path buffers, vector capacity and conservative
		// per-entry/table/lease overhead, not just the source text length.
		self.mapping.prefixes.iter().fold(
			size_of::<Self>()
				+ 4 * size_of::<Stored>()
				+ 256 + self.text.capacity()
				+ self.workspace.capacity()
				+ self.destination.capacity()
				+ self.digest.capacity()
				+ self.mapping.header_format.capacity()
				+ self.mapping.prefixes.capacity()
					* size_of::<(String, String)>()
				+ self.replay.as_ref().map_or(0, |p| {
					size_of::<snip_core::transfer::CommitReplayPreview>()
						+ p.retained_heap_bytes()
				}),
			|sum, (prefix, folder)| {
				sum.saturating_add(prefix.capacity())
					.saturating_add(folder.capacity())
			},
		)
	}
}

struct Stored {
	token: String,
	expires: Duration,
	bytes: usize,
	payload: CachedImport,
}

struct State {
	entries: Vec<Stored>,
	/// Includes payloads removed from `entries` while Apply still owns them.
	bytes: usize,
	count: usize,
	max_bytes: usize,
	max_entries: usize,
	ttl: Duration,
	started: Instant,
	offset: Duration,
	next: u64,
}

impl State {
	fn now(&self) -> Duration {
		self.started.elapsed().saturating_add(self.offset)
	}
	fn prune(&mut self) {
		let now = self.now();
		let mut i = 0;
		while i < self.entries.len() {
			if self.entries[i].expires <= now {
				let entry = self.entries.swap_remove(i);
				self.bytes -= entry.bytes;
				self.count -= 1;
			} else {
				i += 1;
			}
		}
		self.entries.shrink_to_fit();
	}
}

pub(crate) struct PreparedCache {
	shared: Arc<Mutex<State>>,
	nonce: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CacheError {
	Missing,
	Scope,
	Digest,
}

/// Owns the payload until the original import_apply returns. Its charge is
/// released after its buffers, including on errors or an unwinding job.
pub(crate) struct PayloadLease {
	payload: Option<CachedImport>,
	token: String,
	expires: Duration,
	bytes: usize,
	shared: Arc<Mutex<State>>,
}

impl PayloadLease {
	pub fn payload(&self) -> &CachedImport {
		self.payload.as_ref().unwrap()
	}

	/// Puts the payload back under its token, unexpired, for a refusal
	/// that wrote nothing: the same preview may apply again. Its charge
	/// stays in place. Past the expiry it is released as a plain drop.
	pub fn restore(mut self) {
		{
			let mut state =
				self.shared.lock().unwrap_or_else(PoisonError::into_inner);
			if state.now() < self.expires {
				if let Some(payload) = self.payload.take() {
					state.entries.push(Stored {
						token: std::mem::take(&mut self.token),
						expires: self.expires,
						bytes: self.bytes,
						payload,
					});
				}
			}
		}
	}
}

impl Drop for PayloadLease {
	fn drop(&mut self) {
		// Restored leases have no payload left and keep their charge.
		let Some(payload) = self.payload.take() else {
			return;
		};
		drop(payload);
		let mut state =
			self.shared.lock().unwrap_or_else(PoisonError::into_inner);
		state.bytes -= self.bytes;
		state.count -= 1;
	}
}

impl PreparedCache {
	pub fn new() -> Self {
		// Separate randomized hashes make tokens independent across workers,
		// even if their first prepared payload is identical after a reconnect.
		let nonce = format!(
			"{:016x}{:016x}",
			RandomState::new().hash_one(0u8),
			RandomState::new().hash_one(1u8)
		);
		Self {
			nonce,
			shared: Arc::new(Mutex::new(State {
				entries: Vec::new(),
				bytes: 0,
				count: 0,
				max_bytes: MAX_BYTES,
				max_entries: MAX_ENTRIES,
				ttl: TTL,
				started: Instant::now(),
				offset: Duration::ZERO,
				next: 0,
			})),
		}
	}

	pub fn insert(&self, payload: CachedImport) -> Option<String> {
		let mut state =
			self.shared.lock().unwrap_or_else(PoisonError::into_inner);
		state.prune();
		let bytes = payload.bytes().saturating_add(self.nonce.len() + 17);
		if state.bytes.saturating_add(bytes) > state.max_bytes
			|| state.count >= state.max_entries
		{
			return None;
		}
		let token = format!("{}-{:016x}", self.nonce, state.next);
		state.next = state.next.checked_add(1)?;
		let expires = state.now().saturating_add(state.ttl);
		state.bytes = state.bytes.saturating_add(bytes);
		state.count += 1;
		state.entries.push(Stored {
			token: token.clone(),
			expires,
			bytes,
			payload,
		});
		Some(token)
	}

	pub fn take(
		&self,
		token: &str,
		workspace: &Path,
		destination: &Path,
		digest: &str,
	) -> Result<PayloadLease, CacheError> {
		let mut state =
			self.shared.lock().unwrap_or_else(PoisonError::into_inner);
		state.prune();
		let index = state
			.entries
			.iter()
			.position(|entry| entry.token == token)
			.ok_or(CacheError::Missing)?;
		let entry = state.entries.swap_remove(index);
		state.entries.shrink_to_fit();
		drop(state);
		let lease = PayloadLease {
			payload: Some(entry.payload),
			token: entry.token,
			expires: entry.expires,
			bytes: entry.bytes,
			shared: self.shared.clone(),
		};
		let payload = lease.payload();
		if payload.workspace != workspace || payload.destination != destination
		{
			return Err(CacheError::Scope);
		}
		if payload.digest != digest {
			return Err(CacheError::Digest);
		}
		Ok(lease)
	}

	pub fn set_limits_for_tests(
		&self,
		bytes: usize,
		entries: usize,
		ttl: Duration,
	) {
		let mut state =
			self.shared.lock().unwrap_or_else(PoisonError::into_inner);
		while let Some(entry) = state.entries.pop() {
			state.bytes -= entry.bytes;
			state.count -= 1;
		}
		state.entries.shrink_to_fit();
		state.max_bytes = bytes;
		state.max_entries = entries.min(MAX_ENTRIES);
		state.ttl = ttl;
	}

	pub fn expire_for_tests(&self) {
		let mut state =
			self.shared.lock().unwrap_or_else(PoisonError::into_inner);
		state.offset = state
			.offset
			.saturating_add(state.ttl)
			.saturating_add(Duration::from_secs(1));
		state.prune();
	}

	pub fn usage_for_tests(&self) -> (usize, usize) {
		let mut state =
			self.shared.lock().unwrap_or_else(PoisonError::into_inner);
		state.prune();
		(state.bytes, state.count)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	fn payload() -> CachedImport {
		CachedImport {
			text: "// FILE: a\nx\n".into(),
			mapping: PasteMapping::default(),
			workspace: "/workspace".into(),
			destination: "/workspace/dest".into(),
			digest: "digest".into(),
			replay: None,
		}
	}
	#[test]
	fn cache_admission_counts_in_use_payloads_and_routing_metadata() {
		let cache = PreparedCache::new();
		cache.set_limits_for_tests(128 * 1024, 1, TTL);
		let token = cache.insert(payload()).unwrap();
		let charged = cache.usage_for_tests();
		assert!(charged.0 > payload().text.len());
		let lease = cache
			.take(
				&token,
				Path::new("/workspace"),
				Path::new("/workspace/dest"),
				"digest",
			)
			.unwrap();
		assert_eq!(
			cache.usage_for_tests(),
			charged,
			"consuming a token must retain its allocation charge"
		);
		assert!(
			cache.insert(payload()).is_none(),
			"an in-use entry occupies its cache slot"
		);
		cache.expire_for_tests();
		assert_eq!(
			cache.usage_for_tests(),
			charged,
			"expiry cannot free an executing payload"
		);
		cache.set_limits_for_tests(charged.0, 8, TTL);
		assert!(
			cache.insert(payload()).is_none(),
			"in-use bytes count even when an entry slot is free"
		);
		drop(lease);
		assert_eq!(cache.usage_for_tests(), (0, 0));
		assert!(cache.insert(payload()).is_some());
		cache.expire_for_tests();
		cache.set_limits_for_tests(32, 8, TTL);
		assert!(
			cache.insert(payload()).is_none(),
			"metadata alone exceeds this byte budget"
		);
	}
	#[test]
	fn tokens_expire_are_single_use_and_are_scoped_to_one_worker() {
		let cache = PreparedCache::new();
		let token = cache.insert(payload()).unwrap();
		assert!(matches!(
			PreparedCache::new().take(
				&token,
				Path::new("/workspace"),
				Path::new("/workspace/dest"),
				"digest"
			),
			Err(CacheError::Missing)
		));
		assert!(matches!(
			cache.take(
				&token,
				Path::new("/other"),
				Path::new("/workspace/dest"),
				"digest"
			),
			Err(CacheError::Scope)
		));
		assert!(matches!(
			cache.take(
				&token,
				Path::new("/workspace"),
				Path::new("/workspace/dest"),
				"digest"
			),
			Err(CacheError::Missing)
		));
		let token = cache.insert(payload()).unwrap();
		cache.expire_for_tests();
		assert!(matches!(
			cache.take(
				&token,
				Path::new("/workspace"),
				Path::new("/workspace/dest"),
				"digest"
			),
			Err(CacheError::Missing)
		));
		assert_eq!(cache.usage_for_tests(), (0, 0));
	}

	#[test]
	fn a_restored_payload_applies_again_under_its_token_until_it_expires() {
		let cache = PreparedCache::new();
		let token = cache.insert(payload()).unwrap();
		let charged = cache.usage_for_tests();
		let lease = cache
			.take(
				&token,
				Path::new("/workspace"),
				Path::new("/workspace/dest"),
				"digest",
			)
			.unwrap();
		assert_eq!(lease.payload().text, payload().text);
		lease.restore();
		assert_eq!(
			cache.usage_for_tests(),
			charged,
			"a restored payload keeps its charge"
		);
		let again = cache
			.take(
				&token,
				Path::new("/workspace"),
				Path::new("/workspace/dest"),
				"digest",
			)
			.unwrap();
		drop(again);
		assert_eq!(cache.usage_for_tests(), (0, 0));

		let token = cache.insert(payload()).unwrap();
		let lease = cache
			.take(
				&token,
				Path::new("/workspace"),
				Path::new("/workspace/dest"),
				"digest",
			)
			.unwrap();
		cache.expire_for_tests();
		lease.restore();
		assert_eq!(
			cache.usage_for_tests(),
			(0, 0),
			"an expired payload is released, not restored"
		);
		assert!(matches!(
			cache.take(
				&token,
				Path::new("/workspace"),
				Path::new("/workspace/dest"),
				"digest"
			),
			Err(CacheError::Missing)
		));
	}
}
