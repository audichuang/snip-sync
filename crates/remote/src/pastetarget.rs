//! Where a paste plans and writes. Both front ends plan, check and write
//! through `PasteTarget`: a local target runs the core paste engine on
//! this machine, a remote one asks the workspace's worker, which runs the
//! same engine on its own disk (`crate::pasteserve`).
//!
//! Each keeps its own freshness check. A local Apply writes the plan it
//! kept, re-checked under the repository write lock. A worker keeps no
//! state: its Apply plans again and refuses as stale whatever differs from
//! the preview.

use std::path::{Path, PathBuf};

use snip_core::restore::{
	suggest_restore_base, FsProbe, RestoreBaseSuggestion,
};
use snip_core::transfer::{CanonicalRootId, ImportMapping};

/// The restore base `--adjust-paths` would apply, probed under `primary`.
pub(crate) fn suggest(
	primary: &Path,
	paths: &[String],
	text: &str,
) -> Option<RestoreBaseSuggestion> {
	suggest_restore_base(
		&primary.to_string_lossy(),
		paths,
		&FsProbe,
		snip_core::format::extract_source_root(text).as_deref(),
	)
}

/// The routing every file paste plans with: `snip paste`, the app and a
/// worker. Entries land in `primary`, or under it adjusted by
/// `restore_base`. Entries under `prefix/` land in that prefix's folder.
/// Returns the mapping and the destination roots `plan_import_with` needs:
/// `primary` first, then each folder once. The prefixes are read in order,
/// so a caller that validates and resolves each one meets its errors in
/// order.
pub(crate) fn import_mapping<E>(
	primary: PathBuf,
	primary_id: CanonicalRootId,
	restore_base: Option<&RestoreBaseSuggestion>,
	prefixes: impl IntoIterator<Item = Result<(String, PathBuf), E>>,
	root_id: impl Fn(&Path) -> Result<CanonicalRootId, E>,
) -> Result<(ImportMapping, Vec<PathBuf>), E> {
	let mut mapping = match restore_base {
		Some(s) => ImportMapping::from_restore_base(s, primary_id),
		None => ImportMapping::with_primary(primary_id),
	};
	let mut roots = vec![primary];
	for prefix in prefixes {
		let (prefix, folder) = prefix?;
		let id = root_id(&folder)?;
		if !roots.contains(&folder) {
			roots.push(folder);
		}
		mapping.map_prefix(prefix, id);
	}
	Ok((mapping, roots))
}

#[cfg(test)]
mod tests {
	use std::cell::RefCell;

	use snip_core::restore::RestoreBase;

	use super::*;

	fn id(dir: &Path) -> CanonicalRootId {
		CanonicalRootId::new(dir).unwrap()
	}

	#[test]
	fn import_mapping_puts_the_destination_first_and_each_folder_once() {
		let p = tempfile::tempdir().unwrap();
		let a = tempfile::tempdir().unwrap();
		let b = tempfile::tempdir().unwrap();
		let (p, a, b) = (p.path(), a.path(), b.path());
		let (mapping, roots) = import_mapping(
			p.to_path_buf(),
			id(p),
			None,
			[
				Ok(("x".to_string(), a.to_path_buf())),
				Ok(("y".into(), b.to_path_buf())),
				Ok(("z".into(), a.to_path_buf())),
			],
			|f: &Path| CanonicalRootId::new(f),
		)
		.unwrap();
		assert_eq!(
			roots,
			vec![p.to_path_buf(), a.to_path_buf(), b.to_path_buf()]
		);
		assert!(mapping.prefix_destinations.contains_key("x"));
		assert!(mapping.prefix_destinations.contains_key("y"));
		assert!(mapping.prefix_destinations.contains_key("z"));
		assert_eq!(mapping.primary_destination, Some(id(p)));
		assert!(mapping.primary_prefix.is_none());
	}

	#[test]
	fn import_mapping_follows_the_restore_base() {
		let p = tempfile::tempdir().unwrap();
		let p = p.path();
		let add = RestoreBaseSuggestion {
			base: RestoreBase::Add {
				prefix: "src".into(),
			},
			label: String::new(),
			matched: 1,
			total: 1,
		};
		let (mapping, _) = import_mapping(
			p.to_path_buf(),
			id(p),
			Some(&add),
			std::iter::empty::<Result<(String, PathBuf), std::io::Error>>(),
			|f: &Path| CanonicalRootId::new(f),
		)
		.unwrap();
		assert_eq!(mapping.primary_prefix, Some("src".into()));

		let strip = RestoreBaseSuggestion {
			base: RestoreBase::Strip {
				segment: "proj".into(),
			},
			label: String::new(),
			matched: 1,
			total: 1,
		};
		let (mapping, _) = import_mapping(
			p.to_path_buf(),
			id(p),
			Some(&strip),
			std::iter::empty::<Result<(String, PathBuf), std::io::Error>>(),
			|f: &Path| CanonicalRootId::new(f),
		)
		.unwrap();
		assert_eq!(mapping.prefix_destinations["proj"], id(p));
	}

	#[test]
	fn import_mapping_stops_at_the_first_failing_prefix() {
		let p = tempfile::tempdir().unwrap();
		let a = tempfile::tempdir().unwrap();
		let b = tempfile::tempdir().unwrap();
		let (p, a, b) = (p.path(), a.path(), b.path());
		let seen: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
		let result = import_mapping(
			p.to_path_buf(),
			id(p),
			None,
			[
				Ok(("x".to_string(), a.to_path_buf())),
				Err("boom"),
				Ok(("y".into(), b.to_path_buf())),
			],
			|f: &Path| -> Result<CanonicalRootId, &'static str> {
				seen.borrow_mut().push(f.to_path_buf());
				Ok(id(f))
			},
		);
		match result {
			Err(e) => assert_eq!(e, "boom"),
			Ok(_) => panic!("the failing prefix must stop the mapping"),
		}
		assert_eq!(*seen.borrow(), vec![a.to_path_buf()]);
	}
}
