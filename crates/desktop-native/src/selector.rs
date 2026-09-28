//! Header repo / ref selectors: a filterable popover driven by the IME text
//! input, with Up/Down/Enter/Escape. Picking a ref filters the Git Log; it
//! never checks anything out.

use gpui::{Context, Window};

use crate::text_input::InputEvent;
use crate::{Popover, WorkbenchModel};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pick {
	Repo(usize),
	Ref(Option<String>),
}

#[derive(Clone, Debug)]
pub struct SelectorItem {
	pub id: String,
	pub label: String,
	pub detail: String,
	pub error: bool,
	pub group: Option<&'static str>,
	pub pick: Pick,
}

// Candidates borrow model data. Counting/filtering never formats owned rows.
#[derive(Clone, Copy)]
pub(crate) enum SelectorCandidate<'a> {
	Repo {
		index: usize,
		repos: &'a [crate::RepoEntry],
		locale: crate::i18n::Locale,
	},
	Ref {
		name: Option<&'a str>,
		label: &'a str,
		detail: &'a str,
		group: Option<&'static str>,
	},
}

#[cfg(test)]
thread_local! {
	static MATERIALIZED_ITEMS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl SelectorCandidate<'_> {
	fn label(&self) -> &str {
		match self {
			Self::Repo { index, repos, .. } => &repos[*index].name,
			Self::Ref { label, .. } => label,
		}
	}

	/// Section of the ref popup (`refs_local` / `refs_remote` / `refs_tags`).
	pub(crate) fn group(&self) -> Option<&'static str> {
		match self {
			Self::Repo { .. } => None,
			Self::Ref { group, .. } => *group,
		}
	}

	fn pick(self) -> Pick {
		match self {
			Self::Repo { index, .. } => Pick::Repo(index),
			Self::Ref { name, .. } => Pick::Ref(name.map(str::to_owned)),
		}
	}

	fn into_item(self) -> SelectorItem {
		#[cfg(test)]
		MATERIALIZED_ITEMS.with(|count| count.set(count.get() + 1));
		let pick = self.pick();
		match self {
			Self::Repo {
				index,
				repos,
				locale,
			} => {
				let r = &repos[index];
				// ponytail: scan duplicate names only for visible rows; index if this becomes hot.
				let duplicate =
					repos.iter().filter(|other| other.name == r.name).count()
						> 1;
				let (detail, error) = match &r.summary {
					Ok(s) => (
						format!(
							"{} +{} ~{} ?{}{}",
							s.branch.as_deref().unwrap_or(""),
							s.changes.staged,
							s.changes.unstaged,
							s.changes.untracked,
							if s.changes.conflicted > 0 {
								format!(" !{}", s.changes.conflicted)
							} else {
								String::new()
							}
						),
						false,
					),
					Err(_) => (
						crate::i18n::t("repo_error_short", locale).to_string(),
						true,
					),
				};
				SelectorItem {
					id: if duplicate {
						format!("pick-repo:{index}:{}", r.name)
					} else {
						format!("pick-repo:{}", r.name)
					},
					label: r.name.clone(),
					detail: if duplicate {
						format!("{detail}\n{}", r.root.display())
					} else {
						detail
					},
					error,
					group: None,
					pick,
				}
			}
			Self::Ref {
				name,
				label,
				detail,
				group,
			} => SelectorItem {
				id: format!("pick-ref:{}", name.unwrap_or("all")),
				label: label.to_owned(),
				detail: detail.to_owned(),
				error: false,
				group,
				pick,
			},
		}
	}
}

fn candidates<'a>(
	popover: Option<Popover>,
	repos: &'a [crate::RepoEntry],
	refs: &'a [snip_core::browser::GitReference],
	has_head: bool,
	locale: crate::i18n::Locale,
	q: &str,
) -> impl Iterator<Item = SelectorCandidate<'a>> {
	let query = q.to_lowercase();
	let repos = if popover == Some(Popover::Repo) {
		repos
	} else {
		&[]
	};
	let refs = if popover == Some(Popover::Ref) {
		refs
	} else {
		&[]
	};
	let repo_rows =
		(0..repos.len()).map(move |index| SelectorCandidate::Repo {
			index,
			repos,
			locale,
		});
	let special_refs = [
		SelectorCandidate::Ref {
			name: None,
			label: crate::i18n::t("refs_all", locale),
			detail: "",
			group: None,
		},
		SelectorCandidate::Ref {
			name: Some("HEAD"),
			label: "HEAD",
			detail: "",
			group: None,
		},
	]
	.into_iter()
	.enumerate()
	.filter(move |(index, _)| {
		popover == Some(Popover::Ref) && (*index == 0 || has_head)
	})
	.map(|(_, row)| row);
	let ref_rows = [
		("refs_local", "refs/heads/"),
		("refs_remote", "refs/remotes/"),
		("refs_tags", "refs/tags/"),
	]
	.into_iter()
	.flat_map(move |(group, prefix)| {
		refs.iter().filter_map(move |r| {
			let label = r.name.strip_prefix(prefix)?;
			Some(SelectorCandidate::Ref {
				name: Some(&r.name),
				label,
				detail: r.sha.get(..7).unwrap_or(&r.sha),
				group: Some(group),
			})
		})
	});
	repo_rows
		.chain(special_refs)
		.chain(ref_rows)
		.filter(move |row| {
			query.is_empty() || row.label().to_lowercase().contains(&query)
		})
}

fn items_for_range<'a>(
	rows: impl Iterator<Item = SelectorCandidate<'a>> + 'a,
	range: std::ops::Range<usize>,
) -> impl Iterator<Item = (usize, SelectorItem)> + 'a {
	rows.enumerate()
		.skip(range.start)
		.take(range.len())
		.map(|(index, row)| (index, row.into_item()))
}

impl WorkbenchModel {
	pub(crate) fn selector_candidates(
		&self,
		q: &str,
	) -> impl Iterator<Item = SelectorCandidate<'_>> {
		candidates(
			self.popover,
			&self.repos,
			&self.refs,
			// The merged log's HEAD filter picks every repository's HEAD.
			self.head_sha.is_some() || self.log_is_merged(),
			self.locale,
			q,
		)
	}

	pub(crate) fn selector_items(
		&self,
		q: &str,
		range: std::ops::Range<usize>,
	) -> impl Iterator<Item = (usize, SelectorItem)> + '_ {
		items_for_range(self.selector_candidates(q), range)
	}

	pub fn open_popover(
		&mut self,
		kind: Popover,
		window: &mut Window,
		cx: &mut Context<Self>,
	) {
		if self.popover == Some(kind) {
			self.close_popover(cx);
			return;
		}
		self.popover = Some(kind);
		self.popover_cursor = 0;
		self.selector_input.update(cx, |i, cx| i.set_text("", cx));
		window.focus(&self.selector_input.read(cx).handle());
		let n = self.selector_candidates("").count();
		app_log!("[APP:SELECTOR_OPEN: {:?} items={}]", kind, n);
		cx.notify();
	}

	pub fn close_popover(&mut self, cx: &mut Context<Self>) {
		if self.popover.take().is_some() {
			self.pending_focus = Some(self.focus_handle.clone());
			app_log!("[APP:SELECTOR_CLOSED]");
			cx.notify();
		}
	}

	pub fn selector_event(&mut self, ev: InputEvent, cx: &mut Context<Self>) {
		if self.popover.is_none() {
			return;
		}
		let q = self.selector_input.read(cx).text().to_string();
		match ev {
			InputEvent::Changed => {
				self.popover_cursor = 0;
				app_log!(
					"[APP:SELECTOR_FILTER: items={}]",
					self.selector_candidates(&q).count()
				);
			}
			InputEvent::Down => {
				self.popover_cursor = (self.popover_cursor + 1).min(
					self.selector_candidates(&q).count().saturating_sub(1),
				);
			}
			InputEvent::Up => {
				self.popover_cursor = self.popover_cursor.saturating_sub(1)
			}
			InputEvent::Submit => {
				let pick = self
					.selector_candidates(&q)
					.nth(self.popover_cursor)
					.map(SelectorCandidate::pick);
				if let Some(pick) = pick {
					self.choose(pick, cx);
				}
				return;
			}
			InputEvent::SubmitPrev => {}
			InputEvent::Dismiss => {
				self.close_popover(cx);
				return;
			}
		}
		cx.notify();
	}

	pub fn choose(&mut self, pick: Pick, cx: &mut Context<Self>) {
		self.close_popover(cx);
		match pick {
			Pick::Repo(i) => {
				app_log!(
					"[APP:SELECTOR_CHOSE: repo={}]",
					self.repos.get(i).map(|r| r.name.as_str()).unwrap_or("?")
				);
				self.select_repo(i, cx);
			}
			Pick::Ref(r) => {
				app_log!(
					"[APP:SELECTOR_CHOSE: ref={}]",
					r.as_deref().unwrap_or("all")
				);
				self.filter_by_ref(r, cx);
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::i18n::Locale;
	use snip_core::browser::GitReference;

	fn reference(name: &str) -> GitReference {
		GitReference {
			name: name.into(),
			sha: "1234567890abcdef".into(),
		}
	}

	#[test]
	fn late_range_materializes_only_requested_rows_and_submit_keeps_exact_ref()
	{
		let mut refs: Vec<_> = (0..5_000)
			.map(|i| reference(&format!("refs/heads/branch-{i:04}")))
			.collect();
		refs.push(reference("refs/heads/Feature/計畫"));
		let rows =
			|| candidates(Some(Popover::Ref), &[], &refs, true, Locale::En, "");
		MATERIALIZED_ITEMS.with(|count| count.set(0));
		assert_eq!(rows().count(), 5_003);
		assert_eq!(
			rows().nth(5_002).unwrap().pick(),
			Pick::Ref(Some("refs/heads/Feature/計畫".into()))
		);
		MATERIALIZED_ITEMS.with(|count| assert_eq!(count.get(), 0));

		let late: Vec<_> = items_for_range(rows(), 4_998..5_003).collect();
		MATERIALIZED_ITEMS.with(|count| assert_eq!(count.get(), 5));
		assert_eq!(
			late.iter()
				.map(|(i, row)| (*i, row.label.as_str()))
				.collect::<Vec<_>>(),
			vec![
				(4_998, "branch-4996"),
				(4_999, "branch-4997"),
				(5_000, "branch-4998"),
				(5_001, "branch-4999"),
				(5_002, "Feature/計畫"),
			]
		);
		assert_eq!(
			late[4].1.pick,
			Pick::Ref(Some("refs/heads/Feature/計畫".into()))
		);
		assert_eq!(late[4].1.id, "pick-ref:refs/heads/Feature/計畫");
		assert_eq!(items_for_range(rows(), 8_000..8_010).count(), 0);
		MATERIALIZED_ITEMS.with(|count| assert_eq!(count.get(), 5));
	}

	#[test]
	fn ref_filter_preserves_groups_order_head_and_unicode() {
		let refs = [
			reference("refs/tags/v1"),
			reference("refs/remotes/origin/main"),
			reference("refs/heads/Beta"),
			reference("refs/heads/Alpha計畫"),
			reference("refs/notes/private"),
		];
		let rows =
			|| candidates(Some(Popover::Ref), &[], &refs, true, Locale::En, "");
		let items: Vec<_> = items_for_range(rows(), 0..10)
			.map(|(_, item)| item)
			.collect();
		assert_eq!(
			items
				.iter()
				.map(|item| item.id.as_str())
				.collect::<Vec<_>>(),
			vec![
				"pick-ref:all",
				"pick-ref:HEAD",
				"pick-ref:refs/heads/Beta",
				"pick-ref:refs/heads/Alpha計畫",
				"pick-ref:refs/remotes/origin/main",
				"pick-ref:refs/tags/v1"
			]
		);
		assert_eq!(
			items.iter().map(|item| item.group).collect::<Vec<_>>(),
			vec![
				None,
				None,
				Some("refs_local"),
				Some("refs_local"),
				Some("refs_remote"),
				Some("refs_tags")
			]
		);
		assert_eq!(items[2].detail, "1234567");
		let filtered: Vec<_> = candidates(
			Some(Popover::Ref),
			&[],
			&refs,
			false,
			Locale::ZhTw,
			"ALPHA計畫",
		)
		.map(SelectorCandidate::pick)
		.collect();
		assert_eq!(
			filtered,
			vec![Pick::Ref(Some("refs/heads/Alpha計畫".into()))]
		);
		assert_eq!(
			candidates(
				Some(Popover::Ref),
				&[],
				&refs,
				false,
				Locale::En,
				"HEAD"
			)
			.count(),
			0
		);
		assert_eq!(
			candidates(None, &[], &refs, true, Locale::En, "").count(),
			0
		);
	}

	#[test]
	fn duplicate_repo_ids_and_labels_stay_exact_without_formatting_for_count() {
		let repo = |name: &str, root: &str| crate::RepoEntry {
			root: root.into(),
			name: name.into(),
			kind: crate::RepoEntryKind::Main,
			identity: None,
			summary: Err("unavailable".into()),
		};
		let repos = [
			repo("billing", "/west/billing"),
			repo("billing", "/east/billing"),
			repo("Ledger計畫", "/ledger"),
		];
		let rows = || {
			candidates(
				Some(Popover::Repo),
				&repos,
				&[],
				false,
				Locale::En,
				"BILLING",
			)
		};
		MATERIALIZED_ITEMS.with(|count| count.set(0));
		assert_eq!(rows().count(), 2);
		assert_eq!(rows().nth(1).unwrap().pick(), Pick::Repo(1));
		MATERIALIZED_ITEMS.with(|count| assert_eq!(count.get(), 0));
		let items: Vec<_> = items_for_range(rows(), 0..2)
			.map(|(_, item)| item)
			.collect();
		assert_eq!(
			items
				.iter()
				.map(|item| item.id.as_str())
				.collect::<Vec<_>>(),
			vec!["pick-repo:0:billing", "pick-repo:1:billing"]
		);
		assert_eq!(items[0].label, "billing");
		assert_eq!(
			items[0].detail,
			format!(
				"{}\n/west/billing",
				crate::i18n::t("repo_error_short", Locale::En)
			)
		);
		assert_eq!(
			items[1].detail,
			format!(
				"{}\n/east/billing",
				crate::i18n::t("repo_error_short", Locale::En)
			)
		);
		assert!(items.iter().all(|item| item.error));
		let (_, unique) = items_for_range(
			candidates(
				Some(Popover::Repo),
				&repos,
				&[],
				false,
				Locale::En,
				"ledger計畫",
			),
			0..1,
		)
		.next()
		.unwrap();
		assert_eq!(unique.id, "pick-repo:Ledger計畫");
		assert_eq!(unique.pick, Pick::Repo(2));
	}
}
