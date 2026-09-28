//! Editor area: tab strip, find bar and the reader.

use super::*;

impl WorkbenchModel {
	// ───────────────────────── editor ─────────────────────────

	/// Editor tab row. An empty `label` means nothing is open: the row stays
	/// (stable layout) but shows no tab, like IntelliJ's empty editor.
	/// Editor tab bar with one Islands pill: file-type icon, the name (italic
	/// while it is a preview tab) and a ✕ when the tab can be closed.
	pub(super) fn tab_strip(
		&self,
		label: String,
		ic: Icon,
		preview: bool,
		close: Option<fn(&mut Self, &mut Context<Self>)>,
		cx: &mut Context<Self>,
	) -> Div {
		let loc = self.locale;
		let log = &self.probes;
		div()
			.flex()
			.flex_row()
			.flex_shrink_0()
			.h(px(32.))
			.px(px(4.))
			.items_center()
			.border_b_1()
			.border_color(rgb(pal().divider))
			.on_mouse_down(MouseButton::Right, |_, w, cx| {
				w.dispatch_action(Box::new(crate::OpenTabMenu), cx)
			})
			.when(!label.is_empty(), |d| {
				d.child(
					div()
						.id("editor-tab")
						.relative()
						.flex()
						.flex_row()
						.items_center()
						.gap(px(6.))
						.min_w_0()
						.max_w(px(360.))
						.h(px(24.))
						.pl(px(8.))
						.pr(px(if close.is_some() { 4. } else { 10. }))
						// Islands selected tab: filled rounded pill.
						.rounded(px(6.))
						.bg(rgb(pal().range_bg))
						.text_size(px(UI_TEXT))
						.text_color(rgb(pal().text))
						.when(preview, |d| {
							d.tooltip(tip(t("tip_preview_tab", loc)))
								// Double-click keeps the tab, as in IntelliJ.
								.on_click(cx.listener(
									|this, ev: &gpui::ClickEvent, _, cx| {
										if ev.click_count() >= 2
											&& !this.reader.pinned
										{
											this.reader.pinned = true;
											app_log!("[APP:TAB_PINNED]");
											cx.notify();
										}
									},
								))
						})
						.child(icon(ic, 14.))
						.child(clip_text(label).when(preview, |d| d.italic()))
						.when_some(close, |d, close| {
							d.child(
								icon_button(
									"btn-tab-close",
									Icon::Close,
									t("tip_close_tab", loc),
									true,
									38,
								)
								.size(px(16.))
								.on_click(cx.listener(move |this, _, _, cx| {
									cx.stop_propagation();
									close(this, cx)
								}))
								.children(probe(log, "btn-tab-close")),
							)
						})
						.children(probe(log, "editor-tab")),
				)
			})
	}
	/// IntelliJ-style empty editor: the real shortcuts, centered and muted.
	pub(super) fn editor_empty_hints(&self) -> Div {
		let loc = self.locale;
		let ctrl = if cfg!(target_os = "macos") {
			"⌘"
		} else {
			"Ctrl+"
		};
		let hints: [(&str, String); 7] = [
			(t("project", loc), "Alt+1".into()),
			(t("changes", loc), "Alt+0".into()),
			(t("git_log", loc), "Alt+9".into()),
			(t("hint_switch_repo", loc), "Alt+Shift+R".into()),
			(t("hint_switch_ref", loc), "Alt+Shift+B".into()),
			(t("paste_tab", loc), format!("{ctrl}V")),
			(t("btn_refresh", loc), format!("{ctrl}R")),
		];
		div()
			.flex()
			.flex_col()
			.flex_1()
			.min_h_0()
			.items_center()
			.justify_center()
			.overflow_hidden()
			.child(
				div()
					.flex()
					.flex_col()
					.gap(px(10.))
					.text_size(px(UI_TEXT))
					.children(hints.into_iter().map(|(label, keys)| {
						div()
							.flex()
							.flex_row()
							.gap(px(10.))
							.child(
								div()
									.w(px(140.))
									.flex()
									.justify_end()
									.text_color(rgb(pal().text_muted))
									.child(label.to_string()),
							)
							.child(
								div().text_color(rgb(pal().link)).child(keys),
							)
					})),
			)
	}

	/// Breadcrumb text and source badge: says exactly which version is shown.
	pub(super) fn source_labels(&self) -> (String, String, String) {
		let loc = self.locale;
		// A commit from the log names its own repository.
		let repo = self
			.preview_root()
			.map(|root| self.log_repo_name(&root))
			.unwrap_or_default();
		let Some(p) = &self.preview else {
			let tab = match (&self.selected_commit, &self.compare) {
				(_, Some((a, b))) => format!("{}..{}", short(a), short(b)),
				(Some(s), None) => format!("commit {}", short(s)),
				_ => t("no_file", loc).to_string(),
			};
			return (tab, repo, String::new());
		};
		let path = p.path.clone().unwrap_or_default();
		let name = path.rsplit('/').next().unwrap_or(&path).to_string();
		let segs = path.replace('/', " › ");
		match &p.source {
			PreviewSource::WorkingFile => (
				name,
				format!("{repo} › {segs}"),
				t("src_working_file", loc).into(),
			),
			PreviewSource::WorkingChanges => (
				name,
				format!("{repo} › {segs}"),
				t("src_working_diff", loc).into(),
			),
			PreviewSource::StagedChanges => (
				name,
				format!("{repo} › {segs}"),
				t("src_staged_diff", loc).into(),
			),
			PreviewSource::UnstagedChanges => (
				name,
				format!("{repo} › {segs}"),
				t("src_unstaged_diff", loc).into(),
			),
			PreviewSource::CommitDiff { sha } => {
				let parent_note = match self
					.commits
					.iter()
					.find(|c| &c.sha == sha)
					.map(|c| c.parents.len())
				{
					Some(0) => t("src_vs_empty_tree", loc).to_string(),
					Some(n) if n > 1 => {
						tf("src_vs_first_parent_merge", loc, &[&n])
					}
					_ => t("src_vs_first_parent", loc).to_string(),
				};
				(
					format!("{name} @{}", short(sha)),
					format!("{repo} › commit {} › {segs}", short(sha)),
					format!("commit {} · {parent_note}", short(sha)),
				)
			}
			PreviewSource::CommitFile { sha } => (
				format!("{name} @{}", short(sha)),
				format!("{repo} › @{} › {segs}", short(sha)),
				tf("src_commit_file", loc, &[&short(sha)]),
			),
			PreviewSource::Compare { from, to } => (
				format!("{name} {}..{}", short(from), short(to)),
				format!("{repo} › {}..{} › {segs}", short(from), short(to)),
				tf("src_compare", loc, &[&short(from), &short(to)]),
			),
			PreviewSource::PasteItem => (name, segs, String::new()),
		}
	}

	/// IntelliJ's search bar: an inline field with Match Case / Regex
	/// toggles, match count, previous / next, go-to-line and close. Hidden
	/// until Ctrl+F or Ctrl+G opens it; Esc closes it.
	pub(super) fn find_bar(&self, cx: &mut Context<Self>) -> Stateful<Div> {
		let loc = self.locale;
		let log = &self.probes;
		let n_matches = self.reader.matches.len();
		let opts = self.reader.find_opts;
		let find_label = match self.reader.current {
			Some(c) if n_matches > 0 => {
				let more = if n_matches >= crate::reader::MAX_MATCHES {
					"+"
				} else {
					""
				};
				format!("{}/{}{}", c + 1, n_matches, more)
			}
			_ => "0/0".into(),
		};
		let toggle = |id: &'static str, ic: Icon, tip_key: &str, on: bool| {
			icon_button(id, ic, t(tip_key, loc), true, 34)
				.size(px(20.))
				.when(on, |d| {
					d.bg(rgb(pal().range_bg)).border_color(rgb(pal().accent))
				})
				.children(probe(log, id))
		};
		let field = div()
			.flex()
			.flex_row()
			.items_center()
			.gap(px(4.))
			.flex_1()
			.min_w(px(160.))
			.max_w(px(420.))
			.h(px(24.))
			.child(icon(Icon::Search, 14.))
			.child(
				div()
					.id("find-input")
					.relative()
					.flex_1()
					.min_w(px(60.))
					.child(self.find_input.clone())
					.children(probe(log, "find-input")),
			)
			.child(
				toggle(
					"btn-find-case",
					Icon::MatchCase,
					"tip_match_case",
					opts.match_case,
				)
				.on_click(cx.listener(|this, _, _, cx| {
					this.toggle_find_option(false, cx)
				})),
			)
			.child(
				toggle("btn-find-regex", Icon::Regex, "tip_regex", opts.regex)
					.on_click(cx.listener(|this, _, _, cx| {
						this.toggle_find_option(true, cx)
					})),
			);
		div()
			.id("find-bar")
			.relative()
			.flex()
			.flex_row()
			.items_center()
			.flex_shrink_0()
			.h(px(32.))
			.px(px(8.))
			.gap(px(6.))
			.border_b_1()
			.border_color(rgb(pal().divider))
			.bg(rgb(pal().panel_bg))
			.text_size(px(SMALL_TEXT))
			.child(field)
			.child(
				div()
					.id("find-count")
					.flex_shrink_0()
					.min_w(px(40.))
					.text_color(rgb(
						if self.reader.find_invalid
							|| (n_matches == 0
								&& !self.reader.find_query.is_empty())
						{
							pal().error
						} else {
							pal().text_muted
						},
					))
					.child(find_label),
			)
			.child(
				icon_button(
					"btn-find-prev",
					Icon::ArrowUp,
					t("tip_find_prev", loc),
					n_matches > 0,
					32,
				)
				.when(n_matches > 0, |b| {
					b.on_click(
						cx.listener(|this, _, _, cx| this.find_step(false, cx)),
					)
				})
				.children(probe(log, "btn-find-prev")),
			)
			.child(
				icon_button(
					"btn-find-next",
					Icon::ArrowDown,
					t("tip_find_next", loc),
					n_matches > 0,
					33,
				)
				.when(n_matches > 0, |b| {
					b.on_click(
						cx.listener(|this, _, _, cx| this.find_step(true, cx)),
					)
				})
				.children(probe(log, "btn-find-next")),
			)
			.child(toolbar_divider())
			.child(
				div()
					.id("goto-field")
					.flex()
					.flex_row()
					.items_center()
					.flex_shrink_0()
					.gap(px(4.))
					.h(px(24.))
					.px(px(6.))
					.rounded(px(4.))
					.border_1()
					.border_color(rgb(pal().button_border))
					.bg(rgb(pal().button_bg))
					.tooltip(tip(t("tip_goto", loc)))
					.child(icon(Icon::GoToLine, 14.))
					.child(
						div()
							.id("goto-input")
							.relative()
							.w(px(64.))
							.child(self.goto_input.clone())
							.children(probe(log, "goto-input")),
					),
			)
			.child(div().flex_1())
			.child(
				icon_button(
					"btn-find-close",
					Icon::Close,
					t("tip_find_close", loc),
					true,
					39,
				)
				.on_click(cx.listener(|this, _, _, cx| this.close_find(cx)))
				.children(probe(log, "btn-find-close")),
			)
			.children(probe(log, "find-bar"))
	}

	pub(super) fn render_editor(&self, cx: &mut Context<Self>) -> AnyElement {
		let loc = self.locale;
		let log = &self.probes;
		let (tab_label, crumbs, badge) = self.source_labels();
		let tab_icon = file_icon(&tab_label);
		let is_diff = self.preview.as_ref().is_some_and(|p| p.is_diff);
		let side = self.reader.diff_mode == DiffMode::SideBySide;
		let has_folds = self.preview.as_ref().is_some_and(|p| {
			p.diff.as_ref().is_some_and(|d| {
				d.trailing || d.shown.iter().any(|&l| d.inline[l].fold > 0)
			})
		});
		let has_preview = self.can_copy_preview();
		let preview_notice = self.preview.as_ref().and_then(|p| {
			if p.notice.is_some() {
				Some(tf(
					"truncated_notice",
					loc,
					&[
						&crate::reader::MAX_PREVIEW_LINES,
						&(crate::reader::MAX_PREVIEW_BYTES / 1024),
					],
				))
			} else if p.line(p.widest).len()
				> crate::reader::MAX_RENDER_LINE_BYTES
			{
				Some(t("line_truncated_notice", loc).to_string())
			} else {
				None
			}
		});

		// IntelliJ editor / diff toolbar: icon-only ghost buttons, the
		// label in the tooltip.
		let toolbar = div()
			.flex()
			.flex_row()
			.items_center()
			.flex_shrink_0()
			.gap(px(2.))
			.when(is_diff, |d| {
				d.child(
					icon_button(
						"btn-prev-diff",
						Icon::PrevDiff,
						t("tip_prev_diff", loc),
						true,
						35,
					)
					.on_click(cx.listener(|this, _, _, cx| this.prev_diff(cx)))
					.children(probe(log, "btn-prev-diff")),
				)
				.child(
					icon_button(
						"btn-next-diff",
						Icon::NextDiff,
						t("tip_next_diff", loc),
						true,
						35,
					)
					.on_click(cx.listener(|this, _, _, cx| this.next_diff(cx)))
					.children(probe(log, "btn-next-diff")),
				)
				.when(has_folds, |d| {
					d.child(
						icon_button(
							"btn-expand-folds",
							Icon::ExpandAll,
							t("tip_expand_folds", loc),
							!self.reader.expanding,
							35,
						)
						.when(!self.reader.expanding, |b| {
							b.on_click(cx.listener(|this, _, _, cx| {
								this.expand_folds(None, cx)
							}))
						})
						.children(probe(log, "btn-expand-folds")),
					)
				})
				.child(
					// Shows the current viewer; clicking switches to the other.
					icon_button(
						"btn-diff-mode",
						if side {
							Icon::SideBySide
						} else {
							Icon::Unified
						},
						t(
							if side {
								"tip_diff_unified"
							} else {
								"tip_diff_side"
							},
							loc,
						),
						true,
						35,
					)
					.on_click(
						cx.listener(|this, _, _, cx| this.toggle_diff_mode(cx)),
					)
					.children(probe(log, "btn-diff-mode")),
				)
				.child(toolbar_divider())
			})
			.when(
				self.selected_commit.is_some() && self.compare.is_none(),
				|d| {
					d.child(
						icon_button(
							"btn-browse-tree",
							Icon::Project,
							format!(
								"{} — {}",
								t("btn_browse_tree", loc),
								t("tip_browse_tree", loc)
							),
							true,
							36,
						)
						.on_click(cx.listener(|this, _, _, cx| {
							this.browse_commit_tree(cx)
						}))
						// The loading commit header changes height when files arrive.
						// Only expose bounds for the completed revision's layout.
						.children(
							self.selected_commit
								.as_ref()
								.filter(|_| {
									log.is_some() && !self.preview_loading
								})
								.and_then(|sha| {
									let sha = crate::multi_log::split_id(sha).0;
									probe(log, format!("btn-browse-tree:{sha}"))
								}),
						),
					)
				},
			)
			.child(
				icon_button(
					"btn-copy-view",
					Icon::Copy,
					format!(
						"{} — {}",
						t("btn_copy_view", loc),
						t("tip_copy_view", loc)
					),
					has_preview,
					37,
				)
				.when(has_preview, |b| {
					b.on_click(cx.listener(|this, _, _, cx| {
						this.copy_current_preview_content(cx)
					}))
				})
				.children(probe(log, "btn-copy-view")),
			);

		let body: AnyElement = if self.preview_loading && self.preview.is_none()
		{
			div()
				.p(px(12.))
				.text_color(rgb(pal().text_muted))
				.child(t("status_loading", loc))
				.into_any_element()
		} else if let Some(err) = &self.preview_error {
			div()
				.id("editor-error")
				.relative()
				.flex()
				.gap(px(6.))
				.p(px(12.))
				.text_color(rgb(pal().error))
				.child(icon(Icon::Warning, 14.))
				.child(div().flex_1().child(err.render(loc)))
				.children(probe(log, "editor-error"))
				.into_any_element()
		} else if self.preview.is_some() {
			div()
				.id("reader")
				.relative()
				.key_context("Reader")
				.track_focus(&self.reader_focus)
				.on_action(cx.listener(|this, _: &ReaderCopy, _, cx| {
					if !this.copy_reader_selection(cx) {
						this.copy_selection_to_clipboard(cx);
					}
				}))
				.on_action(cx.listener(|this, _: &ReaderSelectAll, _, cx| {
					this.select_all_text(cx)
				}))
				.on_action(cx.listener(|this, _: &ReaderUp, _, cx| {
					this.move_cursor_line(-1, cx)
				}))
				.on_action(cx.listener(|this, _: &ReaderDown, _, cx| {
					this.move_cursor_line(1, cx)
				}))
				.on_action(cx.listener(|this, _: &ReaderPageUp, _, cx| {
					this.move_cursor_line(-30, cx)
				}))
				.on_action(cx.listener(|this, _: &ReaderPageDown, _, cx| {
					this.move_cursor_line(30, cx)
				}))
				.on_action(cx.listener(|this, _: &ReaderClear, _, cx| {
					// Esc clears the selection, then closes the find bar.
					if this.reader.selection().is_some() {
						this.reader.anchor = None;
						this.reader.head = None;
						cx.notify();
					} else {
						this.close_find(cx);
					}
				}))
				.flex()
				.flex_col()
				.flex_1()
				// Keep the notice and at least one code row visible at compact sizes.
				.min_h(px(48.))
				.when_some(preview_notice, |d, notice| {
					d.child(
						div()
							.id("reader-truncated-notice")
							.relative()
							.children(probe(log, "reader-truncated-notice"))
							.flex_shrink_0()
							.px(px(12.))
							.py(px(2.))
							.bg(rgb(pal().panel_bg))
							.text_size(px(SMALL_TEXT))
							.text_color(rgb(pal().warning))
							.child(notice),
					)
				})
				.child(self.render_code_view(false, cx))
				.children(probe(log, "reader"))
				.into_any_element()
		} else {
			self.editor_empty_hints().into_any_element()
		};

		let showing = self.preview.is_some()
			|| self.selected_commit.is_some()
			|| self.compare.is_some();
		// Same model method as the tab menu's Close.
		let closable =
			self.open_tab_count() > 0 || self.preview_error.is_some();
		div()
			.flex()
			.flex_col()
			.flex_1()
			.min_w_0()
			.h_full()
			.bg(rgb(pal().editor_bg))
			.rounded(px(ISLAND_RADIUS))
			.child(self.tab_strip(
				if showing { tab_label } else { String::new() },
				tab_icon,
				self.preview.is_some() && !self.reader.pinned,
				closable.then_some(
					(|this, cx| this.close_tab(0, cx))
						as fn(&mut Self, &mut Context<Self>),
				),
				cx,
			))
			.child(
				div()
					.flex()
					.flex_row()
					.items_center()
					.flex_shrink_0()
					.h(px(28.))
					.pl(px(12.))
					.pr(px(6.))
					.gap(px(8.))
					.border_b_1()
					.border_color(rgb(pal().divider))
					.text_size(px(SMALL_TEXT))
					.child(
						div()
							.id("breadcrumb")
							.relative()
							.flex_1()
							.min_w_0()
							.overflow_hidden()
							.line_clamp(1)
							.text_ellipsis()
							.text_color(rgb(pal().text_muted))
							.tooltip(tip(crumbs.clone()))
							.child(crumbs)
							.children(probe(log, "breadcrumb")),
					)
					.when(!badge.is_empty(), |d| {
						d.child(
							div()
								.id("source-badge")
								.relative()
								.min_w_0()
								.max_w(px(300.))
								.px(px(6.))
								.rounded(px(3.))
								.bg(rgb(pal().ref_bg))
								.text_color(rgb(pal().text))
								.overflow_hidden()
								.line_clamp(1)
								.text_ellipsis()
								.tooltip(tip(badge.clone()))
								.child(badge)
								.children(probe(log, "source-badge")),
						)
					})
					.child(toolbar),
			)
			.when(self.reader.find_open, |d| d.child(self.find_bar(cx)))
			.child(body)
			.into_any_element()
	}
}
