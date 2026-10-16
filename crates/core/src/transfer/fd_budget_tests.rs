//! #241: what an Apply holds open must never leave its own rows, or the
//! taking back of the empty files they did not fill, without a
//! descriptor; and a file it could not take back is reported, never left
//! behind in silence. On Linux an Apply pins the objects it makes (see
//! `Made`); a process with a low limit on open files once had every write
//! and every take-back fail with `EMFILE`, leaving hundreds of empty files
//! that the next Apply then skipped as existing.

use super::*;

fn tmp() -> (tempfile::TempDir, PathBuf) {
	let dir = tempfile::tempdir().unwrap();
	let path = dunce::canonicalize(dir.path()).unwrap();
	(dir, path)
}

/// A plan pasting `text` into the existing folder `ws`.
fn plan(ws: &Path, text: &str) -> TransferImportPlan {
	let placed = PlacedRoot::resolve(None, ws).unwrap();
	plan_import_with(
		text,
		"",
		&[placed.anchor().path().to_path_buf()],
		&ImportMapping::with_primary(placed),
		&RunOptions::default(),
	)
	.unwrap()
}

/// Installs the fail point for this thread until dropped.
struct FailGuard;

impl Drop for FailGuard {
	fn drop(&mut self) {
		restore::FAIL_POINT.with(|slot| *slot.borrow_mut() = None);
	}
}

fn fail_at(f: impl Fn(&str, &Path) -> io::Result<()> + 'static) -> FailGuard {
	restore::FAIL_POINT.with(|slot| *slot.borrow_mut() = Some(Box::new(f)));
	FailGuard
}

#[cfg(unix)]
const FILES: usize = 300;

/// 300 new files in 30 new folders: 330 objects for an Apply to make.
#[cfg(unix)]
fn many() -> String {
	let mut text = String::new();
	for i in 0..FILES {
		text.push_str(&format!("// file: d{}/f{i}.txt\nC{i}\n", i % 30));
	}
	text
}

#[cfg(target_os = "linux")]
fn open_descriptors() -> usize {
	fs::read_dir("/proc/self/fd").unwrap().count()
}

/// Pastes 300 new files with the soft limit on open files at `limit`:
/// every one is written, none is left empty, and nothing the Apply opened
/// is still open after it.
#[cfg(unix)]
fn pastes_many_files_under(limit: u64) {
	use nix::sys::resource::{getrlimit, setrlimit, Resource};

	let (_tmp, ws) = tmp();
	let plan = plan(&ws, &many());
	// One test per process under nextest, so the limit stays here.
	let (_, hard) = getrlimit(Resource::RLIMIT_NOFILE).unwrap();
	setrlimit(Resource::RLIMIT_NOFILE, limit.min(hard), hard).unwrap();
	#[cfg(target_os = "linux")]
	let before = open_descriptors();
	let result = plan.apply(&RestoreSelection::default()).unwrap();
	#[cfg(target_os = "linux")]
	assert_eq!(open_descriptors(), before, "a handle outlived the Apply");
	assert!(
		result.errors.is_empty(),
		"{:?}",
		&result.errors[..5.min(result.errors.len())]
	);
	assert_eq!(result.created_count, FILES);
	for i in 0..FILES {
		let path = ws.join(format!("d{}/f{i}.txt", i % 30));
		assert_eq!(
			fs::read_to_string(&path).unwrap(),
			format!("C{i}"),
			"{}",
			path.display()
		);
	}
}

#[cfg(unix)]
#[test]
fn many_new_files_are_written_with_64_descriptors() {
	pastes_many_files_under(64);
}

#[cfg(unix)]
#[test]
fn many_new_files_are_written_with_256_descriptors() {
	pastes_many_files_under(256);
}

#[cfg(unix)]
#[test]
fn many_new_files_are_written_with_1024_descriptors() {
	pastes_many_files_under(1024);
}

/// A row whose write failed leaves its empty file to be taken back; when
/// that fails too, the file stays and the result says so, as a failure of
/// its own the CLI prints and exits non-zero on.
#[test]
fn an_empty_file_that_cannot_be_taken_back_is_reported() {
	let (_tmp, ws) = tmp();
	let plan = plan(&ws, "// file: a.txt\nA\n// file: b.txt\nB\n");
	let a = ws.join("a.txt");
	let target = a.clone();
	let _guard = fail_at(move |step, path| {
		if path == target && (step == "write" || step == "release") {
			return Err(io::Error::other(format!("injected {step}")));
		}
		Ok(())
	});
	let result = plan.apply(&RestoreSelection::default()).unwrap();
	assert_eq!(result.created_count, 1, "{result:?}");
	assert!(a.exists(), "the take-back failed: the empty file is there");
	assert_eq!(fs::read_to_string(&a).unwrap(), "");
	let left: Vec<&String> = result
		.errors
		.iter()
		.filter(|line| {
			line.starts_with("a.txt: ")
				&& line.contains("could not be taken back")
		})
		.collect();
	assert_eq!(left.len(), 1, "{:?}", result.errors);
	assert!(left[0].contains("injected release"), "{left:?}");
	assert_eq!(result.errors.len(), result.failures.len());
	assert_eq!(result.errors.len(), 2, "{:?}", result.errors);
}

/// An Apply refused after it made a file takes the file back; when that
/// fails, the error it returns names the file left behind, besides why
/// it was refused.
#[test]
fn a_refused_apply_reports_the_empty_file_it_could_not_take_back() {
	let (_tmp, ws) = tmp();
	let plan = plan(&ws, "// file: a.txt\nA\n// file: b.txt\nB\n");
	let a = ws.join("a.txt");
	let b = ws.join("b.txt");
	let (left, refused) = (a.clone(), b.clone());
	let _guard = fail_at(move |step, path| {
		if (step == "release" && path == left)
			|| (step == "reserve" && path == refused)
		{
			return Err(io::Error::other(format!("injected {step}")));
		}
		Ok(())
	});
	let err = plan.apply(&RestoreSelection::default()).unwrap_err();
	assert!(a.exists(), "the take-back failed: the empty file is there");
	assert!(!b.exists());
	let text = err.to_string();
	assert!(text.contains("injected reserve"), "{text}");
	assert!(text.contains(&a.display().to_string()), "{text}");
	assert!(text.contains("injected release"), "{text}");
}

/// Two Applies running at once share one ceiling on pins: the second one
/// is never handed the room the first has not used yet. Each once read
/// its own budget, and three Applies under a limit of 256 held 253 pins
/// between them, with no descriptor left for their rows.
#[cfg(target_os = "linux")]
#[test]
fn applies_running_together_share_one_ceiling_on_pins() {
	use std::sync::atomic::Ordering;
	let mut first = Made::default();
	let mut second = Made::default();
	let ceiling = first.ceiling.max(second.ceiling);
	assert!(ceiling > 0, "nothing could be pinned at all");
	let mut taken = 0;
	for _ in 0..PIN_CAP * 2 {
		taken += usize::from(first.take_pin());
		taken += usize::from(second.take_pin());
	}
	assert_eq!(
		taken, ceiling,
		"first {} second {}",
		first.pinned, second.pinned
	);
	assert_eq!(PINNED.load(Ordering::Relaxed), ceiling);
	drop(first);
	drop(second);
	assert_eq!(PINNED.load(Ordering::Relaxed), 0);
}

/// Three Applies pasting 300 new files each at once, with the soft limit
/// on open files at 256: every file is written and none is left empty.
#[cfg(target_os = "linux")]
#[test]
fn three_applies_at_once_are_written_with_256_descriptors() {
	use nix::sys::resource::{getrlimit, setrlimit, Resource};

	let roots: Vec<_> = (0..3).map(|_| tmp()).collect();
	let plans: Vec<_> = roots.iter().map(|(_, ws)| plan(ws, &many())).collect();
	let (_, hard) = getrlimit(Resource::RLIMIT_NOFILE).unwrap();
	setrlimit(Resource::RLIMIT_NOFILE, 256.min(hard), hard).unwrap();
	let start = std::sync::Barrier::new(plans.len());
	let results: Vec<_> = std::thread::scope(|scope| {
		let running: Vec<_> = plans
			.iter()
			.map(|plan| {
				let start = &start;
				scope.spawn(move || {
					start.wait();
					plan.apply(&RestoreSelection::default())
				})
			})
			.collect();
		running.into_iter().map(|t| t.join().unwrap()).collect()
	});
	for ((_, ws), result) in roots.iter().zip(results) {
		let result = result.unwrap();
		assert!(
			result.errors.is_empty(),
			"{:?}",
			&result.errors[..5.min(result.errors.len())]
		);
		assert_eq!(result.created_count, FILES);
		for i in 0..FILES {
			let path = ws.join(format!("d{}/f{i}.txt", i % 30));
			assert_eq!(fs::read_to_string(&path).unwrap(), format!("C{i}"));
		}
	}
}
