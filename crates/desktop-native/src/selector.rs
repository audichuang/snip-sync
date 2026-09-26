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

fn matches(hay: &str, q: &str) -> bool {
	q.is_empty() || hay.to_lowercase().contains(&q.to_lowercase())
}

impl WorkbenchModel {
	pub fn selector_items(&self, q: &str) -> Vec<SelectorItem> {
		match self.popover {
			Some(Popover::Repo) => self
				.repos
				.iter()
				.enumerate()
				.filter(|(_, r)| matches(&r.name, q))
				.map(|(i, r)| {
					let duplicate = self
						.repos
						.iter()
						.filter(|other| other.name == r.name)
						.count() > 1;
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
							crate::i18n::t("repo_error_short", self.locale)
								.to_string(),
							true,
						),
					};
					let detail = if duplicate {
						format!("{detail}\n{}", r.root.display())
					} else {
						detail
					};
					SelectorItem {
						id: if duplicate {
							format!("pick-repo:{i}:{}", r.name)
						} else {
							format!("pick-repo:{}", r.name)
						},
						label: r.name.clone(),
						detail,
						error,
						group: None,
						pick: Pick::Repo(i),
					}
				})
				.collect(),
			Some(Popover::Ref) => {
				let mut out = Vec::new();
				if matches(crate::i18n::t("refs_all", self.locale), q) {
					out.push(SelectorItem {
						id: "pick-ref:all".into(),
						label: crate::i18n::t("refs_all", self.locale).into(),
						detail: String::new(),
						error: false,
						group: None,
						pick: Pick::Ref(None),
					});
				}
				if self.head_sha.is_some() && matches("HEAD", q) {
					out.push(SelectorItem {
						id: "pick-ref:HEAD".into(),
						label: "HEAD".into(),
						detail: String::new(),
						error: false,
						group: None,
						pick: Pick::Ref(Some("HEAD".into())),
					});
				}
				for (group, prefix) in [
					("refs_local", "refs/heads/"),
					("refs_remote", "refs/remotes/"),
					("refs_tags", "refs/tags/"),
				] {
					for r in
						self.refs.iter().filter(|r| r.name.starts_with(prefix))
					{
						let short = &r.name[prefix.len()..];
						if !matches(short, q) {
							continue;
						}
						out.push(SelectorItem {
							id: format!("pick-ref:{}", r.name),
							label: short.to_string(),
							detail: r
								.sha
								.get(..7)
								.unwrap_or(&r.sha)
								.to_string(),
							error: false,
							group: Some(group),
							pick: Pick::Ref(Some(r.name.clone())),
						});
					}
				}
				out
			}
			None => Vec::new(),
		}
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
		let n = self.selector_items("").len();
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
		let items = self.selector_items(&q);
		match ev {
			InputEvent::Changed => {
				self.popover_cursor = 0;
				app_log!("[APP:SELECTOR_FILTER: items={}]", items.len());
			}
			InputEvent::Down => {
				self.popover_cursor = (self.popover_cursor + 1)
					.min(items.len().saturating_sub(1));
			}
			InputEvent::Up => {
				self.popover_cursor = self.popover_cursor.saturating_sub(1)
			}
			InputEvent::Submit => {
				if let Some(item) = items.get(self.popover_cursor) {
					self.choose(item.pick.clone(), cx);
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
