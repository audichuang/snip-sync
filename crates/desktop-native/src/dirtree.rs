//! IntelliJ "Group By > Directory" over a slice of path-carrying rows,
//! shared by the Changes tool window, the Git Log details and the paste
//! preview. Only open directories' children are visited.

/// A row the directory tree can place: its root-relative path.
pub(crate) trait TreePath {
	fn tree_path(&self) -> &str;
}

/// Receives the rows in screen order.
pub(crate) trait TreeSink {
	/// A directory row. `path` is its full path from the root, `name` what the
	/// row shows (a single-child chain joined), `members` the indices of every
	/// file under it. Returns whether its children follow.
	fn dir(
		&mut self,
		path: &str,
		name: &str,
		members: &[usize],
		depth: usize,
	) -> bool;
	fn file(&mut self, ix: usize, depth: usize);
}

/// Builds the rows of a directory tree from the file rows alone: no Git
/// reads, and only open directories' children get rows.
pub(crate) struct DirTree<'a, T> {
	pub(crate) files: &'a [T],
}

impl<T: TreePath> DirTree<'_, T> {
	/// Path of file `i`. An untracked directory comes as `dir/` and is a
	/// leaf named by its last component.
	pub(crate) fn path(&self, i: usize) -> &str {
		self.files[i].tree_path().trim_end_matches('/')
	}

	/// First directory of file `i` from byte `start` on, `None` for a leaf.
	pub(crate) fn head(&self, i: usize, start: usize) -> Option<&str> {
		self.path(i)[start..].split_once('/').map(|(d, _)| d)
	}

	/// Rows of `entries`, files whose paths share the same `start`-byte
	/// prefix: directories first, then files, each by name ignoring case.
	pub(crate) fn push(
		&self,
		sink: &mut impl TreeSink,
		entries: &mut [usize],
		start: usize,
		depth: usize,
	) {
		use crate::ui::natural_cmp;
		let key = |i: usize| match self.head(i, start) {
			Some(dir) => (false, dir),
			None => (true, &self.path(i)[start..]),
		};
		entries.sort_by(|&a, &b| {
			let ((la, na), (lb, nb)) = (key(a), key(b));
			la.cmp(&lb).then_with(|| natural_cmp(na, nb))
		});
		let mut k = 0;
		while let Some(dir) = entries.get(k).and_then(|&i| self.head(i, start))
		{
			let end = k + entries[k..]
				.iter()
				.take_while(|&&i| self.head(i, start) == Some(dir))
				.count();
			let first = entries[k];
			// A directory whose only child is a directory shows as one row.
			let mut name_end = start + dir.len();
			while let Some(sub) = self.head(first, name_end + 1) {
				if !entries[k..end]
					.iter()
					.all(|&i| self.head(i, name_end + 1) == Some(sub))
				{
					break;
				}
				name_end += 1 + sub.len();
			}
			let path = &self.path(first)[..name_end];
			if sink.dir(path, &path[start..], &entries[k..end], depth) {
				self.push(sink, &mut entries[k..end], name_end + 1, depth + 1);
			}
			k = end;
		}
		for &file_idx in &entries[k..] {
			sink.file(file_idx, depth);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	impl TreePath for &str {
		fn tree_path(&self) -> &str {
			self
		}
	}

	#[derive(Default)]
	struct Record {
		rows: Vec<String>,
		closed: Vec<&'static str>,
		dirs: Vec<(String, String, usize, usize)>,
	}

	impl TreeSink for Record {
		fn dir(
			&mut self,
			path: &str,
			name: &str,
			members: &[usize],
			depth: usize,
		) -> bool {
			self.dirs.push((
				path.to_string(),
				name.to_string(),
				members.len(),
				depth,
			));
			self.rows.push(format!("D {name}"));
			!self.closed.contains(&path)
		}

		fn file(&mut self, ix: usize, depth: usize) {
			self.rows.push(format!("F {ix} d{depth}"));
		}
	}

	#[test]
	fn sink_receives_members_and_respects_expanded() {
		let files = ["a/b/x", "a/b/y", "a/c", "z"];
		let tree = DirTree { files: &files };
		let run = |closed: Vec<&'static str>| {
			let mut sink = Record {
				closed,
				..Default::default()
			};
			let mut entries: Vec<usize> = (0..files.len()).collect();
			tree.push(&mut sink, &mut entries, 0, 0);
			sink
		};
		let open = run(Vec::new());
		assert_eq!(
			open.dirs,
			[
				("a".to_string(), "a".to_string(), 3, 0),
				("a/b".to_string(), "b".to_string(), 2, 1),
			]
		);
		assert_eq!(
			open.rows,
			["D a", "D b", "F 0 d2", "F 1 d2", "F 2 d1", "F 3 d0"]
		);
		let closed = run(vec!["a/b"]);
		assert_eq!(closed.rows, ["D a", "D b", "F 2 d1", "F 3 d0"]);
		let shut = run(vec!["a"]);
		assert_eq!(shut.rows, ["D a", "F 3 d0"]);
		assert_eq!(shut.dirs.len(), 1, "a closed directory is not walked");
	}
}
