//! Persistent stores for remote-node pairing.
//!
//! Master and worker share files (`remote-workers.json` and
//! `remote-trusted-masters.json`) that can be edited concurrently by the
//! desktop UI and the CLI. Mutations lock a sidecar file (`<name>.lock`) and
//! save through a unique temporary file before renaming over the target.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::{PairedWorker, TrustedMaster, TRUSTED_FILE, WORKERS_FILE};

/// A JSON list persisted with exclusive sidecar locking and atomic save.
struct ListFile<'a, T> {
	path: &'a Path,
	_marker: PhantomData<fn() -> T>,
}

impl<'a, T: Serialize + DeserializeOwned> ListFile<'a, T> {
	fn new(path: &'a Path) -> Self {
		Self {
			path,
			_marker: PhantomData,
		}
	}

	fn load(&self) -> Vec<T> {
		crate::load_json(self.path)
	}

	fn lock_path(&self) -> PathBuf {
		let file_name = self
			.path
			.file_name()
			.and_then(|s| s.to_str())
			.unwrap_or("store");
		self.path.with_file_name(format!("{file_name}.lock"))
	}

	fn with_lock<R>(&self, f: impl FnOnce() -> io::Result<R>) -> io::Result<R> {
		if let Some(parent) = self.path.parent() {
			if !parent.as_os_str().is_empty() {
				fs::create_dir_all(parent)?;
			}
		}
		let lock_path = self.lock_path();
		let lock_file = OpenOptions::new()
			.read(true)
			.write(true)
			.create(true)
			.truncate(false)
			.open(&lock_path)?;
		lock_file.lock()?;
		struct Guard<'b>(&'b File);
		impl Drop for Guard<'_> {
			fn drop(&mut self) {
				let _ = self.0.unlock();
			}
		}
		let _guard = Guard(&lock_file);
		f()
	}

	fn mutate<R>(
		&self,
		f: impl FnOnce(&mut Vec<T>) -> io::Result<(bool, R)>,
	) -> io::Result<R> {
		self.with_lock(|| {
			let mut items = self.load();
			let (should_save, res) = f(&mut items)?;
			if should_save {
				crate::save_json(self.path, &items)?;
			}
			Ok(res)
		})
	}
}

/// Master's persistent list of paired workers.
#[derive(Debug, Clone)]
pub struct WorkerStore {
	pub path: PathBuf,
}

impl WorkerStore {
	pub fn new(path: PathBuf) -> Self {
		Self { path }
	}

	pub fn in_config_dir(dir: &Path) -> Self {
		Self::new(dir.join(WORKERS_FILE))
	}

	pub fn path(&self) -> &Path {
		&self.path
	}

	pub fn load(&self) -> Vec<PairedWorker> {
		ListFile::<PairedWorker>::new(&self.path).load()
	}

	/// Adds or updates a worker. Existing entries with the same fingerprint
	/// or the same network address are removed, and the new worker is placed
	/// at the front.
	pub fn add(&self, worker: PairedWorker) -> io::Result<()> {
		ListFile::<PairedWorker>::new(&self.path).mutate(|items| {
			items.retain(|w| {
				w.fingerprint != worker.fingerprint && w.addr != worker.addr
			});
			items.insert(0, worker);
			Ok((true, ()))
		})
	}

	/// Removes the worker with the given fingerprint. Returns the removed
	/// worker, or `None` if it was not found (in which case the file is not
	/// rewritten).
	pub fn forget(
		&self,
		fingerprint: &str,
	) -> io::Result<Option<PairedWorker>> {
		ListFile::<PairedWorker>::new(&self.path).mutate(|items| {
			if let Some(pos) =
				items.iter().position(|w| w.fingerprint == fingerprint)
			{
				let removed = items.remove(pos);
				Ok((true, Some(removed)))
			} else {
				Ok((false, None))
			}
		})
	}

	/// Loads a fresh list and finds a worker by position, name, or address.
	pub fn find(&self, query: &str) -> Result<PairedWorker, String> {
		let workers = self.load();
		let idx = Self::find_in(&workers, query)?;
		Ok(workers[idx].clone())
	}

	/// Resolves a worker query against an in-memory list: 1-based index,
	/// unique name, or unique address.
	pub fn find_in(
		workers: &[PairedWorker],
		key: &str,
	) -> Result<usize, String> {
		if let Ok(n) = key.parse::<usize>() {
			if (1..=workers.len()).contains(&n) {
				return Ok(n - 1);
			}
		}
		let hits: Vec<usize> = workers
			.iter()
			.enumerate()
			.filter(|(_, w)| w.name == key || w.addr == key)
			.map(|(i, _)| i)
			.collect();
		match hits.as_slice() {
			[one] => Ok(*one),
			[] => Err(format!(
				"no paired worker named {key}; see `snip remote workers`"
			)),
			_ => Err(format!("{key} names several workers; use its number")),
		}
	}
}

/// Worker's persistent list of trusted masters.
#[derive(Debug, Clone)]
pub struct TrustedMasterStore {
	pub path: PathBuf,
}

impl TrustedMasterStore {
	pub fn new(path: PathBuf) -> Self {
		Self { path }
	}

	pub fn in_config_dir(dir: &Path) -> Self {
		Self::new(dir.join(TRUSTED_FILE))
	}

	pub fn path(&self) -> &Path {
		&self.path
	}

	pub fn load(&self) -> Vec<TrustedMaster> {
		ListFile::<TrustedMaster>::new(&self.path).load()
	}

	/// Adds a trusted master, replacing any previous entry with the same
	/// fingerprint.
	pub fn add(&self, master: TrustedMaster) -> io::Result<()> {
		ListFile::<TrustedMaster>::new(&self.path).mutate(|items| {
			if let Some(pos) = items
				.iter()
				.position(|m| m.fingerprint == master.fingerprint)
			{
				items[pos] = master;
			} else {
				items.push(master);
			}
			Ok((true, ()))
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::time::{Duration, Instant};

	fn w(name: &str, addr: &str, fp: &str) -> PairedWorker {
		PairedWorker {
			name: name.into(),
			addr: addr.into(),
			fingerprint: fp.into(),
		}
	}

	fn timeout_secs(base: u64) -> u64 {
		let scale = std::env::var("SNIP_E2E_TIMEOUT_SCALE")
			.ok()
			.and_then(|v| v.parse::<u64>().ok())
			.unwrap_or(1)
			.max(1);
		base * scale
	}

	#[test]
	fn add_forget_find_basics() {
		let tmp = tempfile::tempdir().unwrap();
		let store = WorkerStore::in_config_dir(tmp.path());
		assert_eq!(store.load(), Vec::new());

		// add newest first
		store.add(w("w1", "1.1.1.1:1", "fp1")).unwrap();
		store.add(w("w2", "2.2.2.2:2", "fp2")).unwrap();
		let list = store.load();
		assert_eq!(list.len(), 2);
		assert_eq!(list[0].name, "w2");
		assert_eq!(list[1].name, "w1");

		// dedupe by addr: replaces w1 with same addr and places at front
		store.add(w("w1_new", "1.1.1.1:1", "fp1_alt")).unwrap();
		let list = store.load();
		assert_eq!(list.len(), 2);
		assert_eq!(list[0].name, "w1_new");
		assert_eq!(list[1].name, "w2");

		// dedupe by fingerprint: replaces w2 with same fp and places at front
		store.add(w("w2_new", "2.2.2.2:99", "fp2")).unwrap();
		let list = store.load();
		assert_eq!(list.len(), 2);
		assert_eq!(list[0].name, "w2_new");
		assert_eq!(list[1].name, "w1_new");

		// find via store
		let found = store.find("1").unwrap();
		assert_eq!(found.name, "w2_new");
		let found = store.find("w1_new").unwrap();
		assert_eq!(found.fingerprint, "fp1_alt");
		let found = store.find("2.2.2.2:99").unwrap();
		assert_eq!(found.name, "w2_new");

		// forget unknown fingerprint returns None and does not modify file
		let mtime_before =
			fs::metadata(store.path()).unwrap().modified().unwrap();
		let non_existent = store.forget("non_existent_fp").unwrap();
		assert!(non_existent.is_none());
		let mtime_after =
			fs::metadata(store.path()).unwrap().modified().unwrap();
		assert_eq!(mtime_before, mtime_after);

		// forget existing fingerprint returns removed worker
		let removed = store.forget("fp2").unwrap();
		assert_eq!(removed.unwrap().name, "w2_new");
		let list = store.load();
		assert_eq!(list.len(), 1);
		assert_eq!(list[0].name, "w1_new");
	}

	#[test]
	fn workers_are_found_by_number_name_or_address() {
		let ws = [
			w("ubuntu", "100.1.1.1", "fp-u"),
			w("win", "100.2.2.2", "fp-w"),
		];
		assert_eq!(WorkerStore::find_in(&ws, "2"), Ok(1));
		assert_eq!(WorkerStore::find_in(&ws, "ubuntu"), Ok(0));
		assert_eq!(WorkerStore::find_in(&ws, "100.2.2.2"), Ok(1));
		assert_eq!(
			WorkerStore::find_in(&ws, "3"),
			Err("no paired worker named 3; see `snip remote workers`".into())
		);
		assert_eq!(
			WorkerStore::find_in(&ws, "mac"),
			Err("no paired worker named mac; see `snip remote workers`".into())
		);
		let dup = [w("same", "a", "fp1"), w("same", "b", "fp2")];
		assert_eq!(
			WorkerStore::find_in(&dup, "same"),
			Err("same names several workers; use its number".into())
		);
	}

	#[test]
	fn stale_instance_concurrent_mutations() {
		let tmp = tempfile::tempdir().unwrap();
		let file = tmp.path().join("remote-workers.json");
		let store_a = WorkerStore::new(file.clone());
		let store_b = WorkerStore::new(file.clone());

		store_a.add(w("y", "10.0.0.2:1", "fp_y")).unwrap();
		store_a.add(w("z", "10.0.0.3:1", "fp_z")).unwrap();

		// A adds x
		store_a.add(w("x", "10.0.0.1:1", "fp_x")).unwrap();

		// B (which never loaded x in memory) forgets y by fingerprint
		let removed = store_b.forget("fp_y").unwrap();
		assert_eq!(removed.unwrap().name, "y");

		// Both x and z are still present
		let list = store_b.load();
		assert_eq!(list.len(), 2);
		assert!(list.iter().any(|w| w.fingerprint == "fp_x"));
		assert!(list.iter().any(|w| w.fingerprint == "fp_z"));
		assert!(!list.iter().any(|w| w.fingerprint == "fp_y"));

		// Forgetting by fingerprint removes the right worker even after reordering
		store_a.add(w("top", "10.0.0.99:1", "fp_top")).unwrap();
		let removed = store_b.forget("fp_z").unwrap();
		assert_eq!(removed.unwrap().name, "z");
		let list = store_a.load();
		assert_eq!(list.len(), 2);
		assert_eq!(list[0].name, "top");
		assert_eq!(list[1].name, "x");
	}

	#[test]
	fn two_processes_interleaving_child_worker() {
		let Ok(file) = std::env::var("SNIP_STORE_CHILD_FILE") else {
			return;
		};
		let tag = std::env::var("SNIP_STORE_CHILD_TAG")
			.unwrap_or_else(|_| "x".into());
		let store = WorkerStore::new(PathBuf::from(file));
		for i in 0..40 {
			let fp = format!("fp_{tag}_{i:04}");
			store
				.add(w(
					&format!("{tag}-worker-{i}"),
					&format!(
						"127.0.0.1:{}",
						20000 + i + if tag == "b" { 100 } else { 0 }
					),
					&fp,
				))
				.expect("child add failed");
		}
	}

	#[test]
	fn two_processes_interleaving_add_without_losing_an_entry() {
		let tmp = tempfile::tempdir().unwrap();
		let file_path = tmp.path().join("remote-workers.json");
		let exe = std::env::current_exe().unwrap();

		let mut child_a = std::process::Command::new(&exe)
			.args([
				"--exact",
				"store::tests::two_processes_interleaving_child_worker",
				"--nocapture",
				"--test-threads=1",
			])
			.env("SNIP_STORE_CHILD_FILE", &file_path)
			.env("SNIP_STORE_CHILD_TAG", "a")
			.spawn()
			.expect("failed to spawn child a");

		let mut child_b = std::process::Command::new(&exe)
			.args([
				"--exact",
				"store::tests::two_processes_interleaving_child_worker",
				"--nocapture",
				"--test-threads=1",
			])
			.env("SNIP_STORE_CHILD_FILE", &file_path)
			.env("SNIP_STORE_CHILD_TAG", "b")
			.spawn()
			.expect("failed to spawn child b");

		let timeout = Duration::from_secs(timeout_secs(60));
		let start = Instant::now();
		let mut a_done = None;
		let mut b_done = None;

		while start.elapsed() < timeout {
			if a_done.is_none() {
				a_done =
					child_a.try_wait().expect("failed to try_wait child a");
			}
			if b_done.is_none() {
				b_done =
					child_b.try_wait().expect("failed to try_wait child b");
			}
			if a_done.is_some() && b_done.is_some() {
				break;
			}
			std::thread::sleep(Duration::from_millis(50));
		}

		if a_done.is_none() || b_done.is_none() {
			let _ = child_a.kill();
			let _ = child_b.kill();
			let _ = child_a.wait();
			let _ = child_b.wait();
			panic!("two-process add timed out after {timeout:?}");
		}

		let status_a = child_a.wait().expect("failed to wait child a");
		let status_b = child_b.wait().expect("failed to wait child b");
		assert!(status_a.success(), "child a failed");
		assert!(status_b.success(), "child b failed");

		let store = WorkerStore::new(file_path);
		let workers = store.load();
		assert_eq!(workers.len(), 80);
		let fps: std::collections::HashSet<_> =
			workers.iter().map(|w| &w.fingerprint).collect();
		assert_eq!(fps.len(), 80);
	}

	#[test]
	fn add_with_unwritable_location_returns_err() {
		let tmp = tempfile::tempdir().unwrap();
		let regular_file = tmp.path().join("a_file");
		fs::write(&regular_file, "blocking").unwrap();
		let bad_path = regular_file.join("remote-workers.json");
		let store = WorkerStore::new(bad_path);
		let res = store.add(w("fail", "127.0.0.1:9", "fp-fail"));
		assert!(
			res.is_err(),
			"expected error for unwritable path, got {res:?}"
		);
	}

	#[test]
	fn unique_temp_names_leave_no_tmp_files() {
		let tmp = tempfile::tempdir().unwrap();
		let store = WorkerStore::in_config_dir(tmp.path());
		for i in 0..50 {
			store
				.add(w(
					&format!("w{i}"),
					&format!("10.0.0.1:{i}"),
					&format!("fp{i}"),
				))
				.unwrap();
		}
		for entry in fs::read_dir(tmp.path()).unwrap() {
			let name =
				entry.unwrap().file_name().to_string_lossy().into_owned();
			assert!(
				!name.ends_with(".tmp"),
				"found leftover temp file: {name}"
			);
		}
	}

	#[test]
	fn trusted_master_store_basics() {
		let tmp = tempfile::tempdir().unwrap();
		let store = TrustedMasterStore::in_config_dir(tmp.path());
		assert_eq!(store.load(), Vec::new());

		let m1 = TrustedMaster {
			name: "master1".into(),
			fingerprint: "fp1".into(),
		};
		let m2 = TrustedMaster {
			name: "master2".into(),
			fingerprint: "fp2".into(),
		};
		store.add(m1).unwrap();
		store.add(m2).unwrap();
		assert_eq!(store.load().len(), 2);

		// Dedupe by fingerprint: replaces m1
		let m1_updated = TrustedMaster {
			name: "master1_renamed".into(),
			fingerprint: "fp1".into(),
		};
		store.add(m1_updated).unwrap();
		let list = store.load();
		assert_eq!(list.len(), 2);
		assert_eq!(list[0].name, "master1_renamed");
		assert_eq!(list[1].name, "master2");
	}
}
