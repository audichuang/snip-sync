//! Git Log list, commit details and changed files.

use super::*;

impl WorkbenchModel {
	pub(super) fn render_log(
		&self,
		height: f32,
		cx: &mut Context<Self>,
	) -> AnyElement {
		let loc = self.locale;
		let log = &self.probes;
		let range = self.range_rows();
		let header = div()
			.flex()
			.flex_row()
			.items_center()
			.flex_shrink_0()
			.h(px(PANEL_HEADER_H + 2.0))
			.px(px(8.))
			.gap(px(8.))
			.border_b_1()
			.border_color(rgb(pal().divider))
			.child(
				div()
					.flex_shrink_0()
					.font_weight(FontWeight::SEMIBOLD)
					.child("Git"),
			)
			.child(
				div()
					.flex_shrink_0()
					.h(px(22.))
					.px(px(8.))
					.flex()
					.items_center()
					.rounded(px(4.))
					.bg(rgb(pal().selection_inactive_bg))
					.child(match &self.active_ref_filter {
						Some(r) => {
							format!("{}: {}", t("log_tab", loc), short_ref(r))
						}
						None => t("log_tab", loc).to_string(),
					}),
			)
			.child(div().flex_1())
			.child(
				log_icon_button(
					"btn-log-more",
					Icon::More,
					t("tip_log_more", loc),
					true,
					self.log_menu == Some(LogMenu::More),
				)
				.on_click(cx.listener(|this, _, _, cx| {
					this.toggle_log_menu(LogMenu::More, cx)
				}))
				.when(self.log_menu == Some(LogMenu::More), |d| {
					d.child(self.log_menu_panel(LogMenu::More, cx))
				})
				.children(probe(log, "btn-log-more")),
			)
			.child(
				log_icon_button(
					"btn-log-hide",
					Icon::Hide,
					t("hide", loc),
					true,
					false,
				)
				.tab_index(47)
				.on_click(cx.listener(|this, _, _, cx| this.toggle_log(cx)))
				.children(probe(log, "btn-log-hide")),
			);

		let text_toggle = |id: &'static str,
		                   ic: Icon,
		                   tooltip: &'static str,
		                   on: bool,
		                   cx: &mut Context<Self>| {
			div()
				.id(id)
				.relative()
				.flex_shrink_0()
				.size(px(20.))
				.flex()
				.items_center()
				.justify_center()
				.rounded(px(3.))
				.cursor_pointer()
				.tooltip(tip(t(tooltip, loc)))
				.when(on, |d| {
					d.bg(rgb(pal().selection_bg))
						.border_1()
						.border_color(rgb(pal().focus_ring))
				})
				.when(!on, |d| d.hover(|s| s.bg(rgb(pal().hover_bg))))
				.on_click(cx.listener(move |this, _, _, cx| {
					if id == "btn-log-regex" {
						this.toggle_log_regex(cx)
					} else {
						this.toggle_log_match_case(cx)
					}
				}))
				.child(icon(ic, 14.))
				.children(probe(log, id))
		};
		let branch_value = self
			.active_ref_filter
			.as_deref()
			.map(|r| short_ref(r).to_string());
		let user_value = self.log_filter.author.clone().map(|a| {
			if self.git_user_email.as_deref() == Some(a.as_str()) {
				t("log_user_me", loc).to_string()
			} else {
				a
			}
		});
		let day = |s: &Option<String>| {
			s.as_deref()
				.map(|s| s.split(' ').next().unwrap_or(s).to_string())
				.unwrap_or_else(|| "…".into())
		};
		let date_value = match (&self.log_filter.since, &self.log_filter.until)
		{
			(None, None) => None,
			(Some(s), None)
				if DATE_PRESETS.iter().any(|(_, since, _)| since == s) =>
			{
				DATE_PRESETS
					.iter()
					.find(|(_, since, _)| since == s)
					.map(|(_, _, label)| t(label, loc).to_string())
			}
			(since, until) => Some(format!("{} – {}", day(since), day(until))),
		};
		// The repo chip shows the repository scope, then the paths.
		let repo_value = match self.log_repo_filter.as_slice() {
			[] => None,
			_ => {
				let scope = self.log_scope();
				scope.first().map(|(_, name)| match scope.len() {
					1 => name.clone(),
					n => format!("{name} +{}", n - 1),
				})
			}
		};
		let paths_value = self.log_filter.paths.first().map(|p| {
			match self.log_filter.paths.len() {
				1 => p.clone(),
				n => format!("{p} +{}", n - 1),
			}
		});
		let scope_value = match (repo_value, paths_value) {
			(Some(r), Some(p)) => Some(format!("{r} · {p}")),
			(r, p) => r.or(p),
		};
		let filter_bar = div()
			.flex()
			.flex_row()
			.items_center()
			.flex_shrink_0()
			.h(px(34.))
			.px(px(6.))
			.gap(px(4.))
			.border_b_1()
			.border_color(rgb(pal().divider))
			.child(
				div()
					// A narrow log keeps room for the filter chips.
					.w(px(
						if self.log_width.get() > 0.
							&& self.log_width.get() < 1200.
						{
							160.
						} else {
							240.
						},
					))
					.min_w(px(96.))
					.h(px(26.))
					.px(px(6.))
					.flex()
					.items_center()
					.gap(px(4.))
					.rounded(px(4.))
					.border_1()
					.border_color(rgb(pal().button_border))
					.child(icon(Icon::Search, 14.))
					.child(
						div()
							.id("log-search-input")
							.relative()
							.flex_1()
							.min_w_0()
							.child(self.log_search_input.clone())
							.children(probe(log, "log-search-input")),
					)
					.child(text_toggle(
						"btn-log-regex",
						Icon::Regex,
						"tip_log_regex",
						self.log_filter.regex,
						cx,
					))
					.child(text_toggle(
						"btn-log-case",
						Icon::MatchCase,
						"tip_log_case",
						self.log_filter.match_case,
						cx,
					)),
			)
			// Chips give way first on a narrow window; the actions stay.
			.child(
				div()
					.flex()
					.flex_row()
					.items_center()
					.gap(px(4.))
					.flex_1()
					.min_w_0()
					.overflow_hidden()
					// Repositories and paths share one chip, first; a one-repo
					// workspace only has paths.
					.child(self.log_chip(
						LogMenu::Repo,
						t(
							if self.repos.len() > 1 {
								"log_chip_repo"
							} else {
								"log_chip_paths"
							},
							loc,
						),
						scope_value,
						cx,
					))
					.child(self.log_chip(
						LogMenu::Branch,
						t("log_chip_branch", loc),
						branch_value,
						cx,
					))
					.child(self.log_chip(
						LogMenu::User,
						t("log_chip_user", loc),
						user_value,
						cx,
					))
					.child(self.log_chip(
						LogMenu::Date,
						t("log_chip_date", loc),
						date_value,
						cx,
					)),
			)
			.child(
				log_icon_button(
					"btn-log-refresh",
					Icon::Refresh,
					t("tip_log_refresh", loc),
					true,
					false,
				)
				.on_click(cx.listener(|this, _, _, cx| {
					app_log!("[APP:LOG_REFRESH]");
					this.apply_log_filter(cx)
				}))
				.children(probe(log, "btn-log-refresh")),
			)
			.child(
				log_icon_button(
					"btn-head",
					Icon::Locate,
					t("tip_head", loc),
					self.head_sha.is_some() || self.log_is_merged(),
					false,
				)
				.tab_index(42)
				.on_click(cx.listener(|this, _, _, cx| this.locate_head(cx)))
				.children(probe(log, "btn-head")),
			)
			.child(
				log_icon_button(
					"btn-compare",
					Icon::Diff,
					match range {
						Some(_) => {
							tf("tip_compare", loc, &[&self.range_ids().len()])
						}
						None => t("tip_compare_disabled", loc).to_string(),
					},
					range.is_some(),
					false,
				)
				.when(range.is_some(), |b| {
					b.tab_index(45).on_click(
						cx.listener(|this, _, _, cx| this.compare_range(cx)),
					)
				})
				.children(probe(log, "btn-compare")),
			)
			.child(
				log_icon_button(
					"btn-copy-commits",
					Icon::Copy,
					t("tip_copy_commits", loc),
					self.selected_commit.is_some(),
					false,
				)
				.when(self.selected_commit.is_some(), |b| {
					b.tab_index(46).on_click(cx.listener(|this, _, _, cx| {
						this.copy_commits_to_clipboard(cx)
					}))
				})
				.children(probe(log, "btn-copy-commits")),
			);

		let gutter_w = self
			.graph_layout
			.as_ref()
			.map(graph_view::gutter_width)
			.unwrap_or(40.0);
		let rows = self.display_commits();
		let n = rows.len();
		// Tint rows on the current branch only when others are shown too.
		let on_head: Rc<Vec<bool>> = {
			let marks: HashMap<&str, bool> = self
				.commits
				.iter()
				.zip(&self.log_on_head)
				.map(|(c, on)| (c.sha.as_str(), *on))
				.collect();
			let v: Vec<bool> = rows
				.iter()
				.map(|c| marks.get(c.sha.as_str()).copied().unwrap_or(false))
				.collect();
			Rc::new(if v.iter().all(|on| *on) {
				Vec::new()
			} else {
				v
			})
		};
		drop(rows);
		let list_w = f32::from(
			self.log_scroll.0.borrow().base_handle.bounds().size.width,
		);
		let loading_row = self.history_extending && self.history_has_more;
		let list = uniform_list(
			"log-rows",
			n + usize::from(loading_row),
			cx.processor(
				move |this, range: std::ops::Range<usize>, window, cx| {
					this.autoload_near(n, cx);
					range
						.map(|ix| {
							if ix >= n {
								return this.log_loading_row();
							}
							let tint =
								on_head.get(ix).copied().unwrap_or(false);
							this.log_row(ix, gutter_w, list_w, tint, window, cx)
						})
						.collect::<Vec<_>>()
				},
			),
		)
		.track_scroll(self.log_scroll.clone())
		.size_full();

		div()
			.id("log-panel")
			.relative()
			.child({
				// Remembers the panel's width so the side panes can give way.
				let width = self.log_width.clone();
				canvas(
					move |b, _, _| width.set(f32::from(b.size.width)),
					|_, _, _, _| {},
				)
				.absolute()
				.size_full()
			})
			.flex()
			.flex_col()
			.flex_shrink_0()
			.h(px(height))
			.bg(rgb(pal().panel_bg))
			.rounded(px(ISLAND_RADIUS))
			.overflow_hidden()
			.child(header)
			.when_some(self.history_error.clone(), |d, err| {
				d.child(
					div()
						.id("log-error")
						.relative()
						.flex_shrink_0()
						.px(px(10.))
						.py(px(2.))
						.bg(rgb(pal().error_bg))
						.text_size(px(SMALL_TEXT))
						.text_color(rgb(pal().error))
						.child(tf("error_history", loc, &[&err]))
						.children(probe(log, "log-error")),
				)
			})
			.child(
				div()
					.flex()
					.flex_row()
					.flex_1()
					.min_h_0()
					.child(self.render_branch_toolbar(cx))
					.when(self.log_branches_visible, |d| {
						d.child(self.render_branches(cx))
					})
					.child(
						div()
							.flex()
							.flex_col()
							.flex_1()
							.min_w_0()
							.child(filter_bar)
							.child(
								div()
									.id("log-list")
									.relative()
									.key_context("GitLog")
									.track_focus(&self.log_focus)
									.on_action(cx.listener(
										|this, _: &LogUp, _, cx| {
											this.log_move(-1, false, cx)
										},
									))
									.on_action(cx.listener(
										|this, _: &LogDown, _, cx| {
											this.log_move(1, false, cx)
										},
									))
									.on_action(cx.listener(
										|this, _: &LogExtendUp, _, cx| {
											this.log_move(-1, true, cx)
										},
									))
									.on_action(cx.listener(
										|this, _: &LogExtendDown, _, cx| {
											this.log_move(1, true, cx)
										},
									))
									.on_action(cx.listener(
										|this, _: &LogOpen, window, cx| {
											window.focus(&this.reader_focus);
											cx.notify();
										},
									))
									.on_action(cx.listener(
										|this,
										 _: &LogSearchFocus,
										 window,
										 cx| {
											window.focus(
												&this
													.log_search_input
													.read(cx)
													.handle(),
											);
										},
									))
									.on_action(cx.listener(
										|this, _: &LogHead, _, cx| {
											this.locate_head(cx)
										},
									))
									.on_scroll_wheel(cx.listener(
										|this, _: &ScrollWheelEvent, _, cx| {
											if !this.history_autoload {
												this.rearm_autoload();
												cx.notify();
											}
										},
									))
									.flex_1()
									.min_h_0()
									.when(
										n == 0
											&& self.history_error.is_none()
											&& !self.history_extending,
										|d| {
											d.child(
												div()
													.p(px(10.))
													.text_color(rgb(
														pal().text_muted
													))
													.child(t("empty_log", loc)),
											)
										},
									)
									.child(list)
									.children(probe(log, "log-list")),
							),
					)
					.when(self.log_details_visible, |d| {
						d.child(self.splitter(Splitter::LogDetails, cx))
							.child(self.render_commit_panel(cx))
					}),
			)
			.into_any_element()
	}

	/// Branches pane width: narrower when the log itself is narrow.
	pub(super) fn log_branches_w(&self) -> f32 {
		let w = self.log_width.get();
		if w > 0. && w < 1200. {
			LOG_BRANCHES_W * 0.75
		} else {
			LOG_BRANCHES_W
		}
	}

	/// Details pane width, at most 28% of the log so the list keeps room.
	pub(super) fn log_details_width(&self) -> f32 {
		let w = self.log_width.get();
		if w > 0. {
			self.log_details_w.min((w * 0.28).max(LOG_DETAILS_W_MIN))
		} else {
			self.log_details_w
		}
	}

	pub(super) fn log_loading_row(&self) -> AnyElement {
		div()
			.id("log-loading")
			.relative()
			.w_full()
			.h(px(graph_view::ROW_HEIGHT))
			.px(px(10.))
			.flex()
			.items_center()
			.text_size(px(SMALL_TEXT))
			.text_color(rgb(pal().text_muted))
			.child(t("log_loading_more", self.locale))
			.children(probe(&self.probes, "log-loading"))
			.into_any_element()
	}

	pub(super) fn log_row(
		&self,
		ix: usize,
		gutter_w: f32,
		list_w: f32,
		on_head: bool,
		window: &Window,
		cx: &mut Context<Self>,
	) -> AnyElement {
		let rows = self.display_commits();
		let Some(c) = rows.get(ix).copied() else {
			return div().into_any_element();
		};
		let loc = self.locale;
		// Row tooltips are off while a context menu is open (they would
		// draw over it).
		let show_tips = self.chrome.menu.is_none();
		// Every selected row (a range selects its anchor's repository only).
		let selected = self.log_is_selected(&c.sha);
		let sha = c.sha.clone();
		// The merged log: the row's repository (root stripe, ids).
		let repo = self.log_row_repo(&c.sha);
		let key = match repo {
			Some((feed, _)) => format!("{}:{}", feed.name, short(&sha)),
			None => short(&sha).to_string(),
		};
		// Drivers address the selected repository's rows by SHA alone.
		let legacy_id = repo
			.filter(|(feed, _)| self.repo_root().as_ref() == Some(&feed.root))
			.map(|_| format!("commit-row:{}", short(&sha)));
		let stripe = repo.map(|(feed, color)| {
			(graph_view::palette_rgb(color), feed.name.clone())
		});
		let sha_click = sha.clone();
		let graph_row = self
			.graph_layout
			.as_ref()
			.and_then(|l| l.rows.get(ix))
			.cloned();
		let strokes = self
			.graph_layout
			.as_ref()
			.zip(graph_row.as_ref())
			.map(|(l, r)| graph_view::row_strokes(l, r))
			.unwrap_or_default();
		let is_merge = c.parents.len() > 1 && self.log_search.is_none();
		let collapsed = self.collapsed_merges.contains(&c.sha);
		let hidden_n = if collapsed {
			crate::history::side_only(&self.commits, &c.sha).len()
		} else {
			0
		};
		let current_branch = self.log_current_branch(&c.sha);
		let (labels, labels_w) = graph_row
			.as_ref()
			.map(|r| {
				ref_label_elements(
					&r.refs,
					current_branch.as_deref(),
					&key,
					show_tips,
					&|s: &str| text_width(window, s, SMALL_TEXT),
				)
			})
			.unwrap_or_default();
		let mine = self
			.git_user_email
			.as_deref()
			.is_some_and(|e| e.eq_ignore_ascii_case(&c.author_email));
		let hash_w = if self.log_show_hash { 64. } else { 0. };
		// The subject cell is what the row's fixed parts leave (8px right
		// padding, 8px gaps between the cells, 6px before the labels).
		let gaps = if self.log_show_hash { 4. } else { 3. } * 8.;
		let subject_room =
			list_w
				- 8. - gaps - gutter_w
				- AUTHOR_W - DATE_W
				- hash_w - if labels_w > 0. { labels_w + 6. } else { 0. };
		let truncated = text_width(window, &c.subject, UI_TEXT) > subject_room;
		let row_id = format!("commit-row:{key}");
		let col_id = format!("collapse:{key}");
		let merge_sha = sha.clone();
		let node_x = graph_row
			.as_ref()
			.map(|r| graph_view::lane_x(r.node.lane))
			.unwrap_or(0.);
		let date = log_date(&c.author_date, loc);
		div()
			.id(SharedString::from(row_id.clone()))
			.relative()
			.w_full()
			.flex()
			.flex_row()
			.items_center()
			.h(px(graph_view::ROW_HEIGHT))
			.pr(px(8.))
			.gap(px(8.))
			.cursor_pointer()
			.when(on_head && !selected, |d| {
				d.bg(rgb(pal().log_current_branch_bg))
			})
			.when(selected, |d| {
				d.bg(rgb(if self.log_active {
					pal().selection_bg
				} else {
					pal().selection_inactive_bg
				}))
			})
			.when(!selected, |d| d.hover(|s| s.bg(rgb(pal().hover_bg))))
			.on_click(cx.listener(
				move |this, ev: &gpui::ClickEvent, window, cx| {
					window.focus(&this.log_focus);
					// Cmd on macOS, Ctrl elsewhere: IntelliJ's toggle.
					if ev.modifiers().secondary() {
						this.toggle_commit(&sha_click, cx);
					} else if ev.modifiers().shift {
						this.extend_range(&sha_click, cx);
					} else {
						this.select_commit(&sha_click, cx);
					}
				},
			))
			.on_mouse_down(MouseButton::Right, {
				let sha = sha.clone();
				cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
					window.focus(&this.log_focus);
					this.open_log_menu(&sha, ev.position, window, cx);
				})
			})
			.child(
				div()
					.relative()
					.flex_shrink_0()
					.w(px(gutter_w))
					.h(px(graph_view::ROW_HEIGHT))
					.when_some(graph_row, |el, r| {
						el.child(
							canvas(
								|_, _, _| {},
								move |bounds, _, window, _| {
									graph_view::paint_row_graph(
										window, &r, &strokes, bounds,
									);
								},
							)
							.size_full(),
						)
					})
					// Merge rows collapse from their graph node.
					.when(is_merge, |d| {
						d.child(
							div()
								.id(SharedString::from(col_id.clone()))
								.absolute()
								.left(px(node_x - 7.))
								.top(px(graph_view::ROW_HEIGHT / 2. - 7.))
								.size(px(14.))
								.rounded(px(7.))
								.hover(|s| s.bg(rgb(pal().hover_bg)))
								.when(show_tips, |d| {
									d.tooltip(tip(t(
										if collapsed {
											"tip_expand_merge"
										} else {
											"tip_collapse_merge"
										},
										loc,
									)))
								})
								.on_click(cx.listener(move |this, _, _, cx| {
									cx.stop_propagation();
									this.toggle_collapse(merge_sha.clone(), cx);
								}))
								.children(probe(&self.probes, col_id.clone())),
						)
					}),
			)
			.child(
				div()
					.flex_1()
					.min_w_0()
					.flex()
					.flex_row()
					.items_center()
					.gap(px(6.))
					.overflow_hidden()
					.child(
						div()
							.id(SharedString::from(format!("subject:{key}")))
							.flex_1()
							.min_w_0()
							.overflow_hidden()
							.line_clamp(1)
							.text_ellipsis()
							.when(show_tips && truncated, |d| {
								d.tooltip(tip(c.subject.clone()))
							})
							.child(c.subject.clone()),
					)
					.when(collapsed, |d| {
						d.child(
							div()
								.flex_shrink_0()
								.text_size(px(SMALL_TEXT))
								.text_color(rgb(pal().text_muted))
								.child(tf("collapsed_n", loc, &[&hidden_n])),
						)
					})
					.child(
						div()
							.flex_shrink_0()
							.ml_auto()
							.flex()
							.flex_row()
							.items_center()
							.gap(px(8.))
							.children(labels),
					),
			)
			.child(
				div()
					.id(SharedString::from(format!("author:{key}")))
					.flex_shrink_0()
					.w(px(AUTHOR_W))
					.when(show_tips, |d| {
						d.tooltip(tip(format!(
							"{} <{}>",
							c.author_name, c.author_email
						)))
					})
					.child(
						clip_text(c.author_name.clone()).when(mine, |d| {
							d.font_weight(FontWeight::SEMIBOLD)
						}),
					),
			)
			.child(
				div()
					.id(SharedString::from(format!("date:{key}")))
					.flex_shrink_0()
					.w(px(DATE_W))
					.when(show_tips, |d| {
						d.tooltip(tip(short_date(&c.author_date)))
					})
					.child(clip_text(date)),
			)
			.when(self.log_show_hash, |d| {
				d.child(
					div()
						.id(SharedString::from(format!("sha:{key}")))
						.flex_shrink_0()
						.w(px(hash_w))
						.font_family(CODE_FONT)
						.text_size(px(SMALL_TEXT))
						.text_color(rgb(pal().text_muted))
						.when(show_tips, |d| {
							d.tooltip(tip(crate::multi_log::split_id(&c.sha)
								.0
								.to_string()))
						})
						.child(short(&c.sha).to_string()),
				)
			})
			// IntelliJ's root stripe: the repository, at the row's left edge.
			.when_some(stripe, |d, (color, name)| {
				let id = format!("root-stripe:{key}");
				d.child(
					div()
						.id(SharedString::from(id.clone()))
						.absolute()
						.left_0()
						.top_0()
						.w(px(4.))
						.h_full()
						.bg(color)
						.when(show_tips, |d| d.tooltip(tip(name)))
						.children(probe(&self.probes, id)),
				)
			})
			.children(probe(&self.probes, row_id))
			.children(legacy_id.and_then(|id| probe(&self.probes, id)))
			.into_any_element()
	}

	/// The changed-files rows, rebuilt only when their inputs change.
	fn commit_rows(&self, by_dir: bool) -> Rc<Vec<ChangeItemRow>> {
		use std::hash::{Hash, Hasher};
		let mut h = std::collections::hash_map::DefaultHasher::new();
		(&self.commit_files, by_dir, &self.changed_dirs_collapsed).hash(&mut h);
		let key = h.finish();
		let mut cache = self.commit_rows_cache.borrow_mut();
		if let Some((_, rows)) = cache.as_ref().filter(|(k, _)| *k == key) {
			return rows.clone();
		}
		let rows = Rc::new(super::changes::commit_file_rows(
			&self.commit_files,
			by_dir,
			&self.changed_dirs_collapsed,
		));
		*cache = Some((key, rows.clone()));
		rows
	}

	/// Shift-click on a changed-files row: selects the shown rows (folders
	/// keyed "dir/") from the open file to `key`.
	fn shift_click_commit_row(&mut self, key: &str, cx: &mut Context<Self>) {
		let shown: Vec<String> = self
			.commit_rows(self.log_details_by_dir)
			.iter()
			.filter_map(|r| match r {
				ChangeItemRow::Dir { path, .. } => Some(format!("{path}/")),
				ChangeItemRow::File { file_idx, .. } => {
					self.commit_files.get(*file_idx).map(|(p, _)| p.clone())
				}
				_ => None,
			})
			.collect();
		let shown: Vec<&str> = shown.iter().map(String::as_str).collect();
		self.extend_commit_files(&shown, key, cx);
	}

	/// The log's right pane: the selected commit's changed files grouped by
	/// directory, then its details (or the compare's range).
	pub(super) fn render_commit_panel(
		&self,
		cx: &mut Context<Self>,
	) -> AnyElement {
		let loc = self.locale;
		let log = &self.probes;
		let by_dir = self.log_details_by_dir;
		let rows = self.commit_rows(by_dir);
		let n = self.commit_files.len();
		let files_header = div()
			.flex()
			.flex_row()
			.items_center()
			.flex_shrink_0()
			.h(px(30.))
			.px(px(8.))
			.gap(px(4.))
			.border_b_1()
			.border_color(rgb(pal().divider))
			.child(
				fill_text(if n > 0 {
					tf("changed_files", loc, &[&n])
				} else {
					String::new()
				})
				.text_size(px(SMALL_TEXT))
				.text_color(rgb(pal().text_muted)),
			)
			.child(
				log_icon_button(
					"details-group-dir",
					Icon::Folder,
					t("tip_group_by_dir", loc),
					true,
					by_dir,
				)
				.on_click(cx.listener(|this, _, _, cx| {
					this.log_details_by_dir = !this.log_details_by_dir;
					app_log!(
						"[APP:LOG_DETAILS_GROUP_DIR: {}]",
						this.log_details_by_dir
					);
					cx.notify();
				}))
				.children(probe(log, "details-group-dir")),
			)
			.child(
				log_icon_button(
					"details-expand-all",
					Icon::ExpandAll,
					t("tip_expand_all", loc),
					n > 0 && by_dir,
					false,
				)
				.on_click(cx.listener(|this, _, _, cx| {
					this.changed_dirs_collapsed.clear();
					cx.notify();
				}))
				.children(probe(log, "details-expand-all")),
			)
			.child(
				log_icon_button(
					"details-collapse-all",
					Icon::CollapseAll,
					t("tip_collapse_all", loc),
					n > 0 && by_dir,
					false,
				)
				.on_click(cx.listener(|this, _, _, cx| {
					this.changed_dirs_collapsed =
						super::changes::commit_file_dirs(&this.commit_files);
					cx.notify();
				}))
				.children(probe(log, "details-collapse-all")),
			);
		let files = uniform_list(
			"commit-files",
			rows.len(),
			cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
				range
					.filter_map(|ix| rows.get(ix).cloned())
					.map(|row| this.changed_file_row(row, cx))
					.collect::<Vec<_>>()
			}),
		)
		.size_full();

		let details: AnyElement = if let Some((from, to)) = &self.compare {
			div()
				.text_color(rgb(pal().text))
				.font_weight(FontWeight::SEMIBOLD)
				.child(tf("compare_header", loc, &[&short(from), &short(to)]))
				.into_any_element()
		} else if self.log_selected.len() > 1 {
			self.selection_details_view(cx).into_any_element()
		} else if let Some(sha) = self.selected_commit.as_deref() {
			self.commit_details_view(sha, true, cx).into_any_element()
		} else {
			div()
				.text_color(rgb(pal().text_muted))
				.child(t("log_details_empty", loc))
				.into_any_element()
		};
		div()
			.id("commit-panel")
			.relative()
			.flex()
			.flex_col()
			.flex_shrink_0()
			.w(px(self.log_details_width()))
			.h_full()
			// The changed files matter most (IntelliJ): they take what the
			// details, below a splitter, leave.
			.child(
				div()
					.id("commit-files-pane")
					.relative()
					.flex()
					.flex_col()
					.flex_1()
					.min_h_0()
					.child(files_header)
					.child(div().flex_1().min_h_0().py(px(2.)).child(files))
					.children(probe(log, "commit-files-pane")),
			)
			.child(self.splitter(Splitter::LogFiles, cx))
			.child(
				div()
					.id("commit-details")
					.relative()
					.flex_shrink_0()
					.map(|d| match self.log_details_h {
						Some(h) => d.h(px(h)),
						None => d.h(gpui::relative(LOG_DETAILS_H_SHARE)),
					})
					.overflow_y_scroll()
					.p(px(12.))
					.child(details)
					.children(probe(log, "commit-details")),
			)
			.children(probe(log, "commit-panel"))
			.into_any_element()
	}

	/// One row of the changed-files pane, drawn like the Changes tool
	/// window's tree (chevron, folder, compacted name, muted count; a file
	/// by its name in its change colour) without the checkboxes: history
	/// is read-only here.
	pub(super) fn changed_file_row(
		&self,
		row: super::changes::ChangeItemRow,
		cx: &mut Context<Self>,
	) -> AnyElement {
		use super::changes::ChangeItemRow;
		use super::changes::{change_pad, tree_chevron, tree_count};
		match row {
			ChangeItemRow::Dir {
				path,
				name,
				count,
				depth,
				..
			} => {
				let collapsed =
					self.changed_dirs_collapsed.iter().any(|d| d == &path);
				let id = format!("commit-dir:{path}");
				let p2 = path.clone();
				// A folder in the selection is keyed "dir/".
				let key = format!("{path}/");
				let sel = self.rev_tree.is_none()
					&& self.commit_file_sel.contains(&key);
				div()
					.id(SharedString::from(id.clone()))
					.relative()
					.flex()
					.flex_row()
					.items_center()
					.gap(px(2.))
					.h(px(ROW_H))
					.w_full()
					.pl(px(change_pad(depth)))
					.pr(px(8.))
					.cursor_pointer()
					.when(sel, |d| d.bg(rgb(pal().selection_bg)))
					.when(!sel, |d| d.hover(|s| s.bg(rgb(pal().hover_bg))))
					.when(self.chrome.menu.is_none(), |d| {
						d.tooltip(tip(path.clone()))
					})
					.on_mouse_down(MouseButton::Right, {
						let path = path.clone();
						let key = key.clone();
						cx.listener(move |this, ev: &MouseDownEvent, w, cx| {
							// IntelliJ: a menu outside the selection selects
							// its row alone.
							if !sel && !this.commit_file_sel.is_empty() {
								this.commit_file_sel = vec![key.clone()];
							}
							let items = this.commit_file_menu(&path, true);
							w.focus(&this.log_focus);
							this.open_menu(
								crate::menu::MenuOrigin::Log,
								items,
								ev.position,
								w,
								cx,
							);
						})
					})
					.on_click(cx.listener(
						move |this, ev: &gpui::ClickEvent, _, cx| {
							// Cmd/Ctrl and Shift select the folder with the
							// files; a plain click opens or closes it.
							if ev.modifiers().secondary() {
								return this.toggle_commit_file(&key, cx);
							}
							if ev.modifiers().shift {
								return this.shift_click_commit_row(&key, cx);
							}
							let dirs = &mut this.changed_dirs_collapsed;
							match dirs.iter().position(|d| d == &p2) {
								Some(i) => {
									dirs.remove(i);
								}
								None => dirs.push(p2.clone()),
							}
							cx.notify();
						},
					))
					.child(tree_chevron(
						format!("commit-dir-toggle:{path}"),
						collapsed,
					))
					.child(
						div()
							.flex_shrink_0()
							.ml(px(4.))
							.child(icon(Icon::Folder, 14.)),
					)
					.child(
						clip_text(name)
							.ml(px(4.))
							.flex_shrink()
							.text_color(rgb(pal().text)),
					)
					.child(tree_count(count))
					.children(probe(&self.probes, id))
					.into_any_element()
			}
			ChangeItemRow::File { file_idx, depth } => {
				let Some((path, ct)) = self.commit_files.get(file_idx).cloned()
				else {
					return div().into_any_element();
				};
				let (letter, color) = change_style(ct);
				let deleted = ct == Some(ChangeType::Deleted);
				let sel = self.rev_tree.is_none()
					&& if self.commit_file_sel.is_empty() {
						self.selected_commit_file.as_deref() == Some(&path)
					} else {
						self.commit_file_sel.contains(&path)
					};
				let id = format!("commit-file:{path}");
				let (dir, name) = match path.rsplit_once('/') {
					Some((d, n)) => (d.to_string(), n.to_string()),
					None => (String::new(), path.clone()),
				};
				let p2 = path.clone();
				div()
					.id(SharedString::from(id.clone()))
					.relative()
					.flex()
					.flex_row()
					.items_center()
					.gap(px(6.))
					.h(px(ROW_H))
					.w_full()
					// Under the sibling directories' folder icons.
					.pl(px(change_pad(depth) + 22.))
					.pr(px(8.))
					.cursor_pointer()
					.when(sel, |d| d.bg(rgb(pal().selection_bg)))
					.when(!sel, |d| d.hover(|s| s.bg(rgb(pal().hover_bg))))
					.when(self.chrome.menu.is_none(), |d| {
						d.tooltip(tip(format!("{path}  ({letter})")))
					})
					.on_click(cx.listener(
						move |this, ev: &gpui::ClickEvent, _, cx| {
							// Cmd on macOS, Ctrl elsewhere toggles the file;
							// Shift selects the rows from the open one.
							if ev.modifiers().secondary() {
								this.toggle_commit_file(&p2, cx);
							} else if ev.modifiers().shift {
								this.shift_click_commit_row(&p2, cx);
							} else {
								this.select_commit_file(&p2, cx);
							}
						},
					))
					// IntelliJ selects the row a menu opens on.
					.on_mouse_down(MouseButton::Right, {
						let path = path.clone();
						cx.listener(move |this, ev: &MouseDownEvent, w, cx| {
							if !sel {
								this.select_commit_file(&path, cx);
							}
							let items = this.commit_file_menu(&path, false);
							w.focus(&this.log_focus);
							this.open_menu(
								crate::menu::MenuOrigin::Log,
								items,
								ev.position,
								w,
								cx,
							);
						})
					})
					.child(icon(file_icon(&path), 14.))
					.child(
						clip_text(name)
							.flex_shrink_0()
							.max_w(gpui::relative(0.7))
							.text_color(rgb(color))
							.when(deleted, |d| d.line_through()),
					)
					// The flat list names the file's folder after it.
					.when(!self.log_details_by_dir, |d| {
						d.child(
							fill_text(dir)
								.text_size(px(SMALL_TEXT))
								.text_color(rgb(pal().text_muted)),
						)
					})
					.children(probe(&self.probes, id))
					.into_any_element()
			}
			_ => div().into_any_element(),
		}
	}

	/// A multi-selection's info: its repository, then a header row that
	/// opens the list of its commits (newest first), each with its full
	/// details, like IntelliJ's details pane.
	pub(super) fn selection_details_view(&self, cx: &mut Context<Self>) -> Div {
		let loc = self.locale;
		let repo = self
			.log_selected
			.first()
			.and_then(|id| self.log_row_repo(id))
			.map(|(feed, color)| {
				(feed.name.clone(), graph_view::palette_rgb(color))
			});
		let open = self.log_selection_expanded;
		div()
			.relative()
			.flex()
			.flex_col()
			.gap(px(4.))
			.text_color(rgb(pal().text))
			.when_some(repo, |d, (name, color)| {
				d.child(
					div()
						.flex()
						.items_center()
						.gap(px(6.))
						.child(
							div()
								.flex_shrink_0()
								.size(px(8.))
								.rounded(px(2.))
								.bg(color),
						)
						.child(
							div()
								.text_size(px(SMALL_TEXT))
								.text_color(rgb(pal().text_muted))
								.child(tf("log_details_repo", loc, &[&name])),
						),
				)
			})
			.child(
				div()
					.id("log-selection-toggle")
					.relative()
					.flex()
					.items_center()
					.gap(px(4.))
					.cursor_pointer()
					.rounded(px(3.))
					.hover(|s| s.bg(rgb(pal().hover_bg)))
					.font_weight(FontWeight::SEMIBOLD)
					.on_click(cx.listener(|this, _, _, cx| {
						this.toggle_selection_expanded(cx)
					}))
					.child(tf(
						"log_selection_header",
						loc,
						&[&self.log_selected.len()],
					))
					.child(icon(
						if open {
							Icon::ChevronDown
						} else {
							Icon::ChevronRight
						},
						12.,
					))
					.children(probe(&self.probes, "log-selection-toggle")),
			)
			.when(open, |d| {
				// The commits whose details are read; each one scans the
				// log for its refs, so the list stays bounded per frame.
				let max = crate::history::MAX_SELECTION_DETAILS;
				let more = self.log_selected.len() > max;
				let ids = self.log_selected.iter().take(max);
				d.children(ids.map(|id| {
					let probe_id = format!("selection-commit:{}", short(id));
					self.commit_details_view(id, false, cx)
						.relative()
						.pt(px(8.))
						.border_t_1()
						.border_color(rgb(pal().divider))
						.children(probe(&self.probes, probe_id))
				}))
				.when(more, |d| {
					d.child(div().text_color(rgb(pal().text_muted)).child("…"))
				})
			})
			.children(probe(&self.probes, "commit-details-selection"))
	}

	/// Message, hash, author and date, the commit's refs and the branches
	/// that contain it, like IntelliJ's commit details.
	pub(super) fn commit_details_view(
		&self,
		sha: &str,
		show_repo: bool,
		cx: &mut Context<Self>,
	) -> Div {
		let loc = self.locale;
		let row = self.commits.iter().find(|c| c.sha == sha);
		let details = self
			.commit_details
			.as_ref()
			.filter(|d| d.sha == sha)
			.or_else(|| self.selection_details.iter().find(|d| d.sha == sha));
		let message = details
			.map(|d| d.message.clone())
			.or_else(|| row.map(|c| c.subject.clone()))
			.unwrap_or_default();
		let (subject, body) = match message.split_once('\n') {
			Some((s, b)) => (s.to_string(), b.trim().to_string()),
			None => (message, String::new()),
		};
		let (author, email, date) = match (details, row) {
			(Some(d), _) => (
				d.author.clone(),
				d.author_email.clone(),
				d.author_date.clone(),
			),
			(None, Some(c)) => (
				c.author_name.clone(),
				c.author_email.clone(),
				c.author_date.clone(),
			),
			_ => Default::default(),
		};
		let current_branch = self.log_current_branch(sha);
		let repo = self.log_row_repo(sha).filter(|_| show_repo).map(
			|(feed, color)| (feed.name.clone(), graph_view::palette_rgb(color)),
		);
		let refs = self
			.display_commits()
			.iter()
			.position(|c| c.sha == sha)
			.and_then(|ix| {
				self.graph_layout.as_ref().and_then(|l| l.rows.get(ix))
			})
			.map(|r| r.refs.clone())
			.unwrap_or_default();
		let muted = |s: String| {
			div()
				.text_size(px(SMALL_TEXT))
				.text_color(rgb(pal().text_muted))
				.child(s)
		};
		div()
			.flex()
			.flex_col()
			.gap(px(6.))
			.text_color(rgb(pal().text))
			.when_some(repo, |d, (name, color)| {
				let id = format!("commit-details-repo:{name}");
				d.child(
					div()
						.relative()
						.flex()
						.items_center()
						.gap(px(6.))
						.child(
							div()
								.flex_shrink_0()
								.size(px(8.))
								.rounded(px(2.))
								.bg(color),
						)
						.child(muted(tf("log_details_repo", loc, &[&name])))
						.children(probe(&self.probes, id)),
				)
			})
			.child(div().font_weight(FontWeight::SEMIBOLD).child(subject))
			.when(!body.is_empty(), |d| {
				d.child(div().whitespace_normal().child(body))
			})
			// `<hash> <author> <email> on <date>`, as IntelliJ writes it.
			.child(
				div()
					.relative()
					.flex()
					.flex_wrap()
					.items_center()
					.gap(px(4.))
					.child(
						div()
							.font_family(CODE_FONT)
							.text_size(px(SMALL_TEXT))
							.child(short(sha).to_string()),
					)
					.child(div().child(author))
					.when(!email.is_empty(), |d| {
						d.child(
							div()
								.text_color(rgb(pal().link))
								.child(format!("<{email}>")),
						)
					})
					.child(muted(tf(
						"log_details_on",
						loc,
						&[&log_date(&date, loc)],
					)))
					.children(probe(
						&self.probes,
						format!("commit-details-author:{}", short(sha)),
					)),
			)
			.when_some(
				details.filter(|d| {
					d.commit_date != d.author_date
						|| d.committer_email != d.author_email
				}),
				|el, d| {
					el.child(muted(tf(
						"log_details_committed",
						loc,
						&[&d.committer, &log_date(&d.commit_date, loc)],
					)))
				},
			)
			.when(!refs.is_empty(), |d| {
				let badges = graph_view::merge_tracking_refs(&refs);
				d.child(div().flex().flex_col().gap(px(2.)).children(
					badges.iter().map(|b| {
						let l =
							graph_view::ref_label(b, current_branch.as_deref());
						div()
							.flex()
							.items_center()
							.gap(px(4.))
							.child(label_icon(&l))
							.child(l.text)
					}),
				))
			})
			.when_some(details.filter(|d| !d.branches.is_empty()), |el, d| {
				let all = self.log_branches_all.iter().any(|s| s == sha);
				let (list, cut) =
					branches_list(&d.branches, d.branches_more, all);
				let id = sha.to_string();
				let probe_id =
					format!("commit-details-branches:{}", short(sha));
				let more_id = format!("details-branches-all:{}", short(sha));
				el.child(
					div()
						.relative()
						.flex()
						.flex_wrap()
						.gap(px(4.))
						.child(muted(tf(
							"log_details_in_branches",
							loc,
							&[
								&format!(
									"{}{}",
									d.branches.len(),
									if d.branches_more { "+" } else { "" }
								),
								&list,
							],
						)))
						.when(cut, |row| {
							row.child(
								div()
									.id(SharedString::from(more_id.clone()))
									.relative()
									.cursor_pointer()
									.text_size(px(SMALL_TEXT))
									.text_color(rgb(pal().link))
									.on_click(cx.listener(
										move |this, _, _, cx| {
											this.log_branches_all
												.push(id.clone());
											cx.notify();
										},
									))
									.child(t("log_details_show_all", loc))
									.children(probe(&self.probes, more_id)),
							)
						})
						.children(probe(&self.probes, probe_id)),
				)
			})
	}
}

/// Branches a commit's details name before "Show all".
const BRANCHES_SHOWN: usize = 5;

/// The containing branches as the details line lists them: the first
/// [`BRANCHES_SHOWN`] unless `all`, "…" when the read was cut; true when
/// "Show all" would list more.
pub(super) fn branches_list(
	branches: &[String],
	more: bool,
	all: bool,
) -> (String, bool) {
	let cut = !all && branches.len() > BRANCHES_SHOWN;
	let shown = if cut {
		&branches[..BRANCHES_SHOWN]
	} else {
		branches
	};
	let mut list = shown.join(", ");
	if cut || more {
		list.push_str(", …");
	}
	(list, cut)
}
