//! Generation counters that cannot be compared with the wrong lane (#230).
//!
//! A read that outlives a gesture (a Log page, a preview, a Project folder)
//! leaves with the generation of its lane and lands only while that lane is
//! still at it. The counters used to be bare `u64`s, so a landing could be
//! compared with another lane's counter and it still compiled. A [`Gen<L>`]
//! is the counter of lane `L`, and a [`Stamp<L>`] is a value of it: only the
//! lane's own counter accepts the stamp, and the lane's marker type is
//! what the compiler tells them apart by.
//!
//! The counter wraps (`wrapping_add`): a stamp that comes back after 2^64
//! bumps is not a case anyone lives long enough to see.

use std::fmt;
use std::marker::PhantomData;

/// The counter of one lane: bumped whenever what is in flight for the lane
/// stops being wanted.
pub(crate) struct Gen<L> {
	n: u64,
	_lane: PhantomData<fn() -> L>,
}

/// A value a lane's counter had. A read keeps the stamp it left with and
/// asks the counter whether it is still current.
#[must_use]
pub(crate) struct Stamp<L> {
	n: u64,
	_lane: PhantomData<fn() -> L>,
}

impl<L> Gen<L> {
	/// A counter that has not been bumped yet.
	pub(crate) const fn new() -> Self {
		Self::starting_at(0)
	}

	/// A counter that already stands at `n`: for a lane whose first value is
	/// not zero (a workspace's generation is its tab's).
	pub(crate) const fn starting_at(n: u64) -> Self {
		Self {
			n,
			_lane: PhantomData,
		}
	}

	/// Moves the lane on: every stamp taken so far is stale. Returns the new
	/// one.
	pub(crate) fn bump(&mut self) -> Stamp<L> {
		self.n = self.n.wrapping_add(1);
		self.current()
	}

	/// [`Self::bump`] for a caller that does not keep the stamp: whatever was
	/// in flight is dropped.
	pub(crate) fn invalidate(&mut self) {
		self.n = self.n.wrapping_add(1);
	}

	/// Whether the lane ever moved from its starting point (a lane that
	/// starts at zero: something was begun).
	pub(crate) fn has_moved(&self) -> bool {
		self.n != 0
	}

	/// The stamp of now.
	pub(crate) fn current(&self) -> Stamp<L> {
		Stamp {
			n: self.n,
			_lane: PhantomData,
		}
	}

	/// Whether `stamp` is of now: nothing moved the lane since.
	pub(crate) fn is_current(&self, stamp: Stamp<L>) -> bool {
		stamp.n == self.n
	}

	/// The count, for a test that reads it back.
	#[cfg(test)]
	pub(crate) fn raw_for_tests(&self) -> u64 {
		self.n
	}
}

impl<L> Stamp<L> {
	/// The count, for a test that builds the stamp before it.
	#[cfg(test)]
	pub(crate) fn raw_for_tests(self) -> u64 {
		self.n
	}

	/// A stamp with a chosen value, for a test that builds a landing by hand.
	#[cfg(test)]
	pub(crate) fn for_tests(n: u64) -> Self {
		Self {
			n,
			_lane: PhantomData,
		}
	}
}

impl<L> Default for Gen<L> {
	fn default() -> Self {
		Self::new()
	}
}

// The derives would ask for `L: Clone`, and so on: a lane is only a marker.
impl<L> Clone for Stamp<L> {
	fn clone(&self) -> Self {
		*self
	}
}

impl<L> Copy for Stamp<L> {}

impl<L> PartialEq for Stamp<L> {
	fn eq(&self, other: &Self) -> bool {
		self.n == other.n
	}
}

impl<L> Eq for Stamp<L> {}

impl<L> fmt::Debug for Gen<L> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "Gen({})", self.n)
	}
}

/// A stamp prints as its number: the E2E lines carry it (`generation=2`).
impl<L> fmt::Display for Stamp<L> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "{}", self.n)
	}
}

impl<L> fmt::Debug for Stamp<L> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "Stamp({})", self.n)
	}
}

/// The lanes. Each is only a name the compiler tells the counters apart by.
macro_rules! lanes {
	($($(#[$doc:meta])* $name:ident),+ $(,)?) => {
		$($(#[$doc])* pub(crate) enum $name {})+
	};
}

lanes! {
	/// The open repo's read and everything keyed to it (`generation`).
	RepoLoadLane,
	/// The reader's file or diff read.
	PreviewLane,
	/// A page of the Log.
	HistoryLane,
	/// The Project tree.
	TreeLane,
	/// The tree's single worker.
	TreeWorkerLane,
	/// One tree read's epoch, kept by the tree itself.
	TreeEpochLane,
	/// A commit's details.
	DetailsLane,
	/// The Changes list.
	ChangesLane,
	/// The workspace scan.
	DiscoveryLane,
	/// A paste row's diff.
	PasteDiffLane,
	/// A remote workspace's open.
	RemoteOpenLane,
	/// A host's folder listing in the workspace menu.
	BrowseLane,
	/// The retry timer of a restoring remote tab.
	RestoreTimerLane,
	/// The workspace the tab shows: the whole drain.
	WorkspaceLane,
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_stamp_is_current_until_its_lane_moves() {
		let mut g = Gen::<PreviewLane>::new();
		let first = g.bump();
		assert!(g.is_current(first));
		assert!(g.is_current(g.current()));
		let second = g.bump();
		assert!(!g.is_current(first));
		assert!(g.is_current(second));
		assert_eq!(g.raw_for_tests(), 2);
		let mut at = Gen::<PreviewLane>::starting_at(7);
		assert_eq!(at.bump(), Stamp::for_tests(8));
	}

	#[test]
	fn the_counter_wraps_instead_of_panicking() {
		let mut g = Gen::<TreeLane>::starting_at(u64::MAX);
		let stale = g.current();
		let wrapped = g.bump();
		assert_eq!(g.raw_for_tests(), 0);
		assert!(!g.is_current(stale));
		assert!(g.is_current(wrapped));
	}

	/// Compiles `source` as a crate that holds this very file as a module,
	/// and says whether it built. (A doctest cannot say it: the desktop is a
	/// binary crate, and `cargo test --doc` skips those.)
	fn compiles(source: &str) -> bool {
		let dir = tempfile::tempdir().unwrap();
		let main = dir.path().join("main.rs");
		let gen_rs =
			std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/gen.rs");
		std::fs::write(
			&main,
			format!(
				"#![allow(dead_code)]\n#[path = {gen_rs:?}]\nmod gen;\n{source}\nfn main() {{}}\n"
			),
		)
		.unwrap();
		let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
		let out = std::process::Command::new(rustc)
			.args([
				"--edition",
				"2021",
				"--crate-type",
				"bin",
				"--emit=metadata",
			])
			.arg("--out-dir")
			.arg(dir.path())
			.arg(&main)
			.output()
			.expect("rustc must run");
		out.status.success()
	}

	/// The point of the type: a stamp of one lane does not ask another lane's
	/// counter, so mixing them up is a compile error, not a stale read that
	/// lands (or a fresh one that is dropped).
	#[test]
	fn a_stamp_of_one_lane_does_not_typecheck_against_another() {
		let alike = "
			fn f(mut a: gen::Gen<gen::PreviewLane>) -> bool {
				let s = a.bump();
				a.is_current(s)
			}";
		assert!(compiles(alike), "the control did not compile");
		let mixed = "
			fn f(a: gen::Gen<gen::HistoryLane>, s: gen::Stamp<gen::PreviewLane>) -> bool {
				a.is_current(s)
			}";
		assert!(!compiles(mixed), "a stamp of another lane was accepted");
		let stored = "
			struct Landing { at: gen::Stamp<gen::TreeLane> }
			fn f(a: &gen::Gen<gen::PreviewLane>) -> Landing {
				Landing { at: a.current() }
			}";
		assert!(!compiles(stored), "a stamp was stored under another lane");
	}
}
