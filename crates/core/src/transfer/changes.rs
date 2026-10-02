use crate::gitrun::RunOptions;
use crate::gitsrc::{self, Git, GitSource};
use crate::transfer::{CanonicalRootId, ExportItem, SourceKind, TransferError};

/// Changes discovered in a Git repository for transfer planning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedItems {
	pub items: Vec<ExportItem>,
	/// Entries dropped because their path name is not UTF-8 (cannot go in a header).
	pub skipped_non_utf8: usize,
	/// Listed paths whose new side is a submodule commit (gitlink): not file content, not in `items`.
	pub gitlinks: Vec<String>,
}

/// Lists changed files in `gitsrc` order, converting them to transfer [`ExportItem`]s.
pub fn changed_items(
	root: &CanonicalRootId,
	git: &Git,
	source: &GitSource,
	opts: &RunOptions,
) -> Result<ChangedItems, TransferError> {
	let (changes, skipped_non_utf8) =
		gitsrc::list_changes_with(git, source, opts)?;
	let kind = match source {
		GitSource::Working => SourceKind::Working,
		GitSource::Staged => SourceKind::Staged,
		GitSource::Commit(rev) => {
			let sha = git.resolve_commit_with(rev, opts)?;
			SourceKind::Commit { rev: sha }
		}
		GitSource::Range(a, b) => {
			let base = git.resolve_commit_with(a, opts)?;
			let tip = git.resolve_commit_with(b, opts)?;
			SourceKind::Range { base, tip }
		}
	};

	let mut items = Vec::new();
	let mut gitlinks = Vec::new();

	for change in changes {
		if change.gitlink {
			gitlinks.push(change.path);
		} else {
			items.push(ExportItem {
				root: root.clone(),
				relative_path: change.path,
				source: kind.clone(),
				change_type: Some(change.change_type),
			});
		}
	}

	Ok(ChangedItems {
		items,
		skipped_non_utf8,
		gitlinks,
	})
}
