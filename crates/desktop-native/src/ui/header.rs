//! Header bar (workspace, repo and ref selectors) and status bar.

use super::*;

impl WorkbenchModel {
	// ───────────────────────── header ─────────────────────────

	pub(super) fn render_workspace_chip(
		&self,
		cx: &mut Context<Self>,
	) -> AnyElement {
		let loc = self.locale;
		let log = &self.probes;
		let label = if self.workspace_open {
			self.workspace_root
				.file_name()
				.map(|n| n.to_string_lossy().to_string())
				.filter(|n| !n.is_empty())
				.unwrap_or_else(|| self.workspace_root.display().to_string())
		} else {
			t("workspace_none", loc).to_string()
		};
		let tip_text = if self.workspace_open {
			self.workspace_root.display().to_string()
		} else {
			t("workspace_none", loc).to_string()
		};
		let open = self.workspace_open;
		let current = self.workspace_root.clone();
		let recent: Vec<AnyElement> = self
			.recent_workspaces
			.iter()
			.enumerate()
			.map(|(ix, path)| {
				let name = path
					.file_name()
					.map(|n| n.to_string_lossy().to_string())
					.unwrap_or_else(|| path.display().to_string());
				let is_current = open && *path == current;
				let target = path.clone();
				menu_row(
					SharedString::from(format!("workspace-recent:{ix}")),
					70 + ix as isize,
				)
				.h(px(38.))
				.when(is_current, |d| d.bg(rgb(pal().hover_bg)))
				.on_click(cx.listener(move |this, _, _, cx| {
					this.workspace_menu = false;
					this.open_workspace_path(target.clone(), cx);
				}))
				.child(div().flex_shrink_0().child(icon(Icon::Folder, 16.)))
				.child(
					div()
						.flex()
						.flex_col()
						.min_w_0()
						.child(
							div()
								.font_weight(FontWeight::SEMIBOLD)
								.child(clip_text(name)),
						)
						.child(
							div()
								.text_size(px(SMALL_TEXT))
								.text_color(rgb(pal().text_muted))
								.child(clip_text(crate::recent::tilde(path))),
						),
				)
				.children(probe(log, format!("workspace-recent:{ix}")))
				.into_any_element()
			})
			.collect();
		let sep = || {
			div()
				.h(px(1.))
				.mx(px(8.))
				.my(px(4.))
				.bg(rgb(pal().popup_border))
		};
		let panel = div()
			.id("workspace-menu")
			.occlude()
			.w(px(340.))
			.flex()
			.flex_col()
			.p(px(4.))
			.text_size(px(UI_TEXT))
			.text_color(rgb(pal().text))
			.bg(rgb(pal().popup_bg))
			.border_1()
			.border_color(rgb(pal().popup_border))
			.rounded(px(8.))
			.shadow_lg()
			.on_mouse_down_out(cx.listener(|this, _, _, cx| {
				this.workspace_menu = false;
				this.workspace_picker = false;
				cx.notify();
			}))
			.child(
				menu_row("btn-open-folder", 60)
					.on_click(cx.listener(|this, _, _, cx| {
						this.open_folder_dialog(cx);
					}))
					.child(icon(Icon::Folder, 16.))
					.child(fill_text(t("workspace_open_folder", loc)))
					.child(div().text_color(rgb(pal().text_muted)).child(
						if cfg!(target_os = "macos") {
							"⇧⌘O"
						} else {
							"Ctrl+Shift+O"
						},
					))
					.children(probe(log, "btn-open-folder")),
			)
			.child(
				menu_row("btn-open-workspace", 62)
					.on_click(cx.listener(|this, _, _, cx| {
						this.show_workspace_picker(cx);
					}))
					.child(div().w(px(16.)))
					.child(fill_text(t("workspace_open", loc)))
					.children(probe(log, "btn-open-workspace")),
			)
			.when(self.workspace_picker, |d| {
				d.child(
					div()
						.flex()
						.flex_row()
						.gap(px(6.))
						.px(px(8.))
						.py(px(4.))
						.child(
							div()
								.id("workspace-path-input")
								.relative()
								.flex_1()
								.min_w_0()
								.child(self.workspace_path_input.clone())
								.children(probe(log, "workspace-path-input")),
						)
						.child(
							button(
								"btn-workspace-open-confirm",
								t("workspace_open_confirm", loc),
								Btn::Primary,
								true,
								63,
							)
							.on_click(cx.listener(|this, _, _, cx| {
								let text = this
									.workspace_path_input
									.read(cx)
									.text()
									.trim()
									.to_string();
								this.confirm_open_workspace(&text, cx);
							}))
							.children(probe(log, "btn-workspace-open-confirm")),
						),
				)
			})
			.when(!recent.is_empty(), |d| {
				d.child(sep())
					.child(
						div()
							.px(px(8.))
							.py(px(2.))
							.text_size(px(SMALL_TEXT))
							.text_color(rgb(pal().text_muted))
							.child(t("workspace_recent", loc)),
					)
					.children(recent)
			})
			.child(sep())
			.child(
				menu_row("btn-close-workspace", 61)
					.when(!open, |d| d.text_color(rgb(pal().text_disabled)))
					.when(open, |d| {
						d.on_click(cx.listener(|this, _, _, cx| {
							this.workspace_menu = false;
							this.request_user_close(
								crate::lifecycle::Intent::CloseWorkspace,
								cx,
							);
						}))
					})
					.child(div().w(px(16.)))
					.child(fill_text(t("workspace_close", loc)))
					.children(probe(log, "btn-close-workspace")),
			);
		div()
			.relative()
			.flex_shrink_0()
			.max_w(px(180.))
			.child(
				div()
					.id("btn-workspace-menu")
					.relative()
					.flex()
					.items_center()
					.min_w_0()
					.max_w(px(160.))
					.h(px(26.))
					.px(px(8.))
					.rounded(px(4.))
					.cursor_pointer()
					.hover(|s| s.bg(rgb(pal().hover_bg)))
					.when(self.workspace_menu, |d| d.bg(rgb(pal().hover_bg)))
					.tooltip(tip(format!(
						"{tip_text} · {}",
						mac_keys(t("tip_workspace_menu", loc))
					)))
					.on_click(cx.listener(|this, _, _, cx| {
						this.toggle_workspace_menu(cx);
					}))
					.child(clip_text(label))
					.children(probe(log, "btn-workspace-menu")),
			)
			.when(self.workspace_menu, |d| {
				d.child(
					div().absolute().top(px(28.)).left_0().child(
						deferred(anchored().snap_to_window().child(panel))
							.with_priority(1),
					),
				)
			})
			.into_any_element()
	}

	pub(super) fn render_workspace_closed(
		&self,
		cx: &mut Context<Self>,
	) -> AnyElement {
		let loc = self.locale;
		div()
			.id("workspace-closed")
			.relative()
			.flex_1()
			.min_w_0()
			.flex()
			.items_center()
			.justify_center()
			.bg(rgb(pal().editor_bg))
			.rounded(px(ISLAND_RADIUS))
			.child(
				div()
					.flex()
					.flex_col()
					.items_center()
					.gap(px(12.))
					.max_w(px(520.))
					.px(px(16.))
					.child(
						div()
							.text_color(rgb(pal().text_muted))
							.child(t("workspace_closed", loc)),
					)
					.child(
						button(
							"btn-welcome-open-folder",
							t("workspace_open_folder", loc),
							Btn::Primary,
							true,
							64,
						)
						.on_click(cx.listener(|this, _, _, cx| {
							this.open_folder_dialog(cx);
						}))
						.children(probe(
							&self.probes,
							"btn-welcome-open-folder",
						)),
					),
			)
			.children(probe(&self.probes, "workspace-closed"))
			.into_any_element()
	}

	pub(super) fn render_header(&self, cx: &mut Context<Self>) -> AnyElement {
		let loc = self.locale;
		let log = &self.probes;

		let repo = self.repo();
		let count = self.basket_count();
		let copy_reason = if self.is_copying {
			Some(t("btn_copying", loc))
		} else if count > 0 {
			None
		} else if self.selected_commit.is_some() && self.selected_file.is_none()
		{
			Some(t("btn_copy_commit_readonly", loc))
		} else {
			Some(t("btn_copy_empty", loc))
		};
		let copy_enabled = copy_reason.is_none();
		let branch = repo
			.and_then(|r| r.summary.as_ref().ok())
			.and_then(|s| s.branch.clone())
			.unwrap_or_else(|| "-".into());
		let ref_label = match &self.active_ref_filter {
			Some(r) => r.trim_start_matches("refs/heads/").to_string(),
			None => branch.clone(),
		};
		let repo_error = repo.is_some_and(|r| r.summary.is_err());

		let selector = |id: &'static str,
		                ic: Icon,
		                label: String,
		                open: bool,
		                tab: isize,
		                tooltip: String| {
			div()
				.id(id)
				.relative()
				.flex()
				.items_center()
				.gap(px(5.))
				.min_w_0()
				.max_w(px(260.))
				.h(px(26.))
				.px(px(8.))
				.rounded(px(4.))
				.border_1()
				.border_color(transparent_black())
				.cursor_pointer()
				.tab_index(tab)
				.hover(|s| s.bg(rgb(pal().hover_bg)))
				.when(open, |d| d.bg(rgb(pal().hover_bg)))
				.map(focus_ring)
				// No tooltip over an open popup (IntelliJ hides it).
				.when(!open, |d| d.tooltip(tip(tooltip)))
				.child(icon(ic, 14.))
				.child(clip_text(label).font_weight(FontWeight::SEMIBOLD))
				.child(icon(Icon::ChevronDown, 10.))
				.children(probe(log, id))
		};

		div()
			.flex()
			.flex_row()
			.items_center()
			.flex_shrink_0()
			.h(px(HEADER_H))
			.px(px(8.))
			.gap(px(4.))
			.bg(rgb(pal().header_bg))
			.child(
				div()
					.flex_shrink_0()
					.size(px(18.))
					.rounded(px(4.))
					.bg(rgb(pal().accent))
					.flex()
					.items_center()
					.justify_center()
					.text_size(px(10.))
					.font_weight(FontWeight::BOLD)
					.text_color(rgb(pal().accent_text))
					.child("S"),
			)
			.child(self.render_workspace_chip(cx))
			.child(toolbar_divider())
			.child(
				div()
					.relative()
					.flex()
					.min_w_0()
					.child(
						selector(
							"btn-repo-selector",
							if repo_error {
								Icon::Warning
							} else {
								Icon::Project
							},
							repo.map(|r| r.name.clone())
								.unwrap_or_else(|| t("no_repo", loc).into()),
							self.popover == Some(Popover::Repo),
							1,
							t("tip_repo_selector", loc).into(),
						)
						.on_click(cx.listener(|this, _, window, cx| {
							this.open_popover(Popover::Repo, window, cx)
						})),
					)
					.when(self.popover == Some(Popover::Repo), |d| {
						d.child(self.render_popover(cx))
					}),
			)
			.child(
				div()
					.relative()
					.flex()
					.min_w_0()
					.child(
						selector(
							"btn-ref-selector",
							Icon::Branch,
							ref_label,
							self.popover == Some(Popover::Ref),
							2,
							tf("tip_ref_selector", loc, &[&branch]),
						)
						.on_click(cx.listener(|this, _, window, cx| {
							this.open_popover(Popover::Ref, window, cx)
						})),
					)
					.when(self.popover == Some(Popover::Ref), |d| {
						d.child(self.render_popover(cx))
					}),
			)
			.child(div().flex_1())
			.child(
				div()
					.flex()
					.flex_row()
					.items_center()
					.gap(px(2.))
					.flex_shrink_0()
					.child(
						// IntelliJ main toolbar widget: a transparent icon with a
						// small count badge instead of a filled button.
						icon_button(
							"btn-copy",
							Icon::Basket,
							copy_reason.map(str::to_string).unwrap_or_else(
								|| tf("tip_basket_copy", loc, &[count]),
							),
							copy_enabled,
							3,
						)
						.when(copy_enabled, |b| {
							b.on_click(cx.listener(|this, _, _, cx| {
								this.copy_selection_to_clipboard(cx)
							}))
						})
						.when(count > 0, |b| {
							b.child(
								div()
									.absolute()
									.top(px(-3.))
									.right(px(-4.))
									.min_w(px(13.))
									.h(px(13.))
									.px(px(3.))
									.rounded(px(7.))
									.flex()
									.items_center()
									.justify_center()
									.bg(rgb(pal().accent))
									.text_size(px(9.))
									.font_weight(FontWeight::SEMIBOLD)
									.text_color(rgb(pal().accent_text))
									.child(count.to_string()),
							)
						})
						.children(probe(log, "btn-copy")),
					)
					.when(self.is_copying, |row| {
						row.child(
							button(
								"btn-copy-cancel",
								t("btn_copy_cancel", loc),
								Btn::Default,
								true,
								32,
							)
							.on_click(cx.listener(|this, _, _, cx| {
								this.cancel_copy(cx)
							}))
							.children(probe(log, "btn-copy-cancel")),
						)
					})
					.when(count > 0, |row| {
						row.child(
							icon_button(
								"btn-basket-clear",
								Icon::Close,
								t("basket_clear", loc),
								true,
								31,
							)
							.on_click(cx.listener(|this, _, _, cx| {
								this.clear_basket(cx);
							}))
							.children(probe(log, "btn-basket-clear")),
						)
					})
					.child(
						icon_button(
							"btn-paste",
							Icon::Paste,
							t("btn_paste", loc),
							true,
							4,
						)
						.on_click(cx.listener(|this, _, _, cx| {
							this.trigger_paste_preview(cx)
						}))
						.children(probe(log, "btn-paste")),
					)
					.child(
						icon_button(
							"btn-refresh",
							Icon::Refresh,
							t("btn_refresh", loc),
							true,
							5,
						)
						.on_click(
							cx.listener(|this, _, _, cx| this.reload_repos(cx)),
						)
						.children(probe(log, "btn-refresh")),
					),
			)
			.into_any_element()
	}

	/// IntelliJ branches / repositories popup: search field on top, then the
	/// list with Local / Remote / Tags section headers. Headers are display
	/// rows only; `popover_cursor` and the filter count stay over items.
	pub(super) fn render_popover(&self, cx: &mut Context<Self>) -> AnyElement {
		const ITEM_H: f32 = 24.0;
		let log = &self.probes;
		let q = self.selector_input.read(cx).text().to_string();
		// (first item index, group) of each section, at most three.
		let mut sections: Vec<(usize, &'static str)> = Vec::new();
		let mut n = 0;
		let mut prev = None;
		for c in self.selector_candidates(&q) {
			let g = c.group();
			if g.is_some() && g != prev {
				sections.push((n, g.unwrap_or_default()));
			}
			prev = g;
			n += 1;
		}
		let rows = n + sections.len();
		let list_h = (rows.max(1) as f32 * ITEM_H).min(360.0);
		let cursor = self.popover_cursor;
		let panel = div()
			.id("selector-popover")
			.occlude()
			.w(px(360.))
			.flex()
			.flex_col()
			.bg(rgb(pal().popup_bg))
			.border_1()
			.border_color(rgb(pal().popup_border))
			.rounded(px(ISLAND_RADIUS))
			.shadow_lg()
			.p(px(4.))
			.gap(px(4.))
			.on_mouse_down_out(
				cx.listener(|this, _, _, cx| this.close_popover(cx)),
			)
			.child(
				div()
					.id("selector-input")
					.relative()
					.flex()
					.items_center()
					.gap(px(4.))
					.pl(px(6.))
					.child(icon(Icon::Search, 14.))
					.child(
						div()
							.flex_1()
							.min_w_0()
							.child(self.selector_input.clone()),
					)
					.children(probe(log, "selector-input")),
			)
			.child(div().h(px(1.)).mx(px(-4.)).bg(rgb(pal().popup_border)))
			.child(
				div()
					.h(px(list_h))
					.when(n == 0, |d| {
						d.child(
							div()
								.p(px(6.))
								.text_color(rgb(pal().text_muted))
								.child(t("selector_empty", self.locale)),
						)
					})
					.when(n > 0, |d| {
						d.child(
							uniform_list(
								"selector-items",
								rows,
								cx.processor(
									move |this,
									      range: std::ops::Range<usize>,
									      _,
									      cx| {
										this.popover_rows(
											&sections, range, cursor, cx,
										)
									},
								),
							)
							.size_full(),
						)
					}),
			);
		div()
			.absolute()
			.top(px(28.))
			.left_0()
			.child(
				deferred(anchored().snap_to_window().child(panel))
					.with_priority(1),
			)
			.into_any_element()
	}

	/// Display rows `range` of the popup: section headers and items.
	pub(super) fn popover_rows(
		&self,
		sections: &[(usize, &'static str)],
		range: std::ops::Range<usize>,
		cursor: usize,
		cx: &mut Context<Self>,
	) -> Vec<Stateful<Div>> {
		// Display index -> Err(header group) or Ok(item index).
		let map = |d: usize| {
			let mut shift = 0;
			for (k, (start, g)) in sections.iter().enumerate() {
				if d == start + k {
					return Err(*g);
				}
				if d > start + k {
					shift = k + 1;
				}
			}
			Ok(d - shift)
		};
		let items: Vec<usize> =
			range.clone().filter_map(|d| map(d).ok()).collect();
		let q = self.selector_input.read(cx).text().to_string();
		let mut picked = match (items.first(), items.last()) {
			(Some(&a), Some(&b)) => self.selector_items(&q, a..b + 1).collect(),
			_ => Vec::new(),
		}
		.into_iter();
		let current_branch = self
			.repo()
			.and_then(|r| r.summary.as_ref().ok())
			.and_then(|s| s.branch.clone());
		range
			.map(|d| match map(d) {
				Err(g) => div()
					.id(SharedString::from(format!("selector-group:{g}")))
					.w_full()
					.flex()
					.items_center()
					.h(px(24.))
					.px(px(8.))
					.text_size(px(11.))
					.font_weight(FontWeight::SEMIBOLD)
					.text_color(rgb(pal().text_muted))
					.child(t(g, self.locale)),
				Ok(_) => {
					let Some((ix, it)) = picked.next() else {
						return div().id(SharedString::from(format!(
							"selector-gap:{d}"
						)));
					};
					let pick = it.pick.clone();
					let active = match &it.pick {
						Pick::Repo(i) => self.selected_repo_idx == Some(*i),
						Pick::Ref(r) => r == &self.active_ref_filter,
					};
					let ic = if it.error {
						Icon::Warning
					} else {
						match (&it.pick, it.group) {
							(Pick::Repo(_), _) => Icon::Project,
							(Pick::Ref(None), _) => Icon::GitLog,
							(Pick::Ref(Some(r)), _) if r == "HEAD" => {
								Icon::Head
							}
							(_, Some("refs_tags")) => Icon::Tag,
							(_, Some("refs_remote")) => Icon::RemoteBranch,
							_ if current_branch.as_deref()
								== Some(it.label.as_str()) =>
							{
								Icon::Head
							}
							_ => Icon::Branch,
						}
					};
					div()
						.id(SharedString::from(it.id.clone()))
						.relative()
						.w_full()
						.flex()
						.items_center()
						.gap(px(6.))
						.h(px(24.))
						.px(px(8.))
						.rounded(px(4.))
						.cursor_pointer()
						.when(ix == cursor, |d| d.bg(rgb(pal().selection_bg)))
						.when(ix != cursor, |d| {
							d.hover(|s| s.bg(rgb(pal().hover_bg)))
						})
						.on_click(cx.listener(move |this, _, _, cx| {
							this.choose(pick.clone(), cx)
						}))
						.child(icon(ic, 14.))
						.child(fill_text(it.label.clone()))
						.child(
							div()
								.flex_shrink_0()
								.text_size(px(11.))
								.text_color(rgb(if it.error {
									pal().error
								} else {
									pal().text_muted
								}))
								.child(it.detail.clone()),
						)
						.child(
							div()
								.flex_shrink_0()
								.w(px(14.))
								.when(active, |d| {
									d.child(icon(Icon::Checked, 14.))
								}),
						)
						.children(probe(&self.probes, it.id.clone()))
				}
			})
			.collect()
	}

	/// IntelliJ status bar: message on the left, borderless widgets on the
	/// right with no separators; zero counters are not shown.
	pub(super) fn render_status(&self, cx: &mut Context<Self>) -> AnyElement {
		let loc = self.locale;
		let errors = self.repos.iter().filter(|r| r.summary.is_err()).count();
		let basket_n = self.basket_count().to_string();
		let (basket_detail, collision) = &self.basket_view;
		let basket_label = if let Some(collision) = collision {
			tf("basket_collision", loc, &[collision])
		} else if self.basket_count() == 0 {
			t("basket_empty", loc).to_string()
		} else {
			tf("basket_summary", loc, &[&basket_n, basket_detail])
		};
		let repos_s = self.repos.len().to_string();
		let repo_label = if errors > 0 {
			tf("status_repo_count", loc, &[&repos_s, &errors.to_string()])
		} else {
			tf("status_repo_count_ok", loc, &[&repos_s])
		};
		let jobs = self.lifecycle.live_jobs();
		let branch =
			self.repo().and_then(|r| r.summary.as_ref().ok()).map(|s| {
				match (&s.branch, &s.head) {
					(Some(b), _) => b.clone(),
					(None, Some(h)) => short(h).to_string(),
					(None, None) => t("repo_unborn", loc).to_string(),
				}
			});
		let widget = || {
			div()
				.flex_shrink_0()
				.flex()
				.items_center()
				.gap(px(4.))
				.h(px(STATUS_H - 4.))
				.px(px(6.))
				.rounded(px(4.))
		};
		div()
			.id("status-bar")
			.relative()
			.flex()
			.flex_row()
			.items_center()
			.flex_shrink_0()
			.h(px(STATUS_H))
			.pl(px(10.))
			.pr(px(6.))
			.gap(px(2.))
			.bg(rgb(pal().frame_bg))
			.text_size(px(SMALL_TEXT))
			.text_color(rgb(pal().text_muted))
			// The line clamps; a long status (per-file paste failures) shows
			// in full on hover.
			.child({
				let status = self.status.render(loc);
				fill_text(status.clone())
					.id("status-text")
					.tooltip(tip(status))
					.mr(px(8.))
			})
			.child(
				widget()
					.id("basket-summary")
					.relative()
					.max_w(px(360.))
					.min_w(px(40.))
					.overflow_hidden()
					.tooltip(tip(basket_label.clone()))
					.child(clip_text(basket_label))
					.children(probe(&self.probes, "basket-summary")),
			)
			.child(
				widget()
					.text_color(rgb(if errors > 0 {
						pal().error
					} else {
						pal().text_muted
					}))
					.child(repo_label),
			)
			.when(jobs > 0, |d| {
				d.child(
					widget()
						.id("lifecycle-jobs")
						.relative()
						.child(tf("lifecycle_jobs", loc, &[&jobs.to_string()]))
						.children(probe(&self.probes, "lifecycle-jobs")),
				)
			})
			.when_some(branch, |d, b| {
				// VCS widget: branch icon and name, like IntelliJ's Git widget.
				d.child(
					widget()
						.id("status-vcs")
						.relative()
						.max_w(px(200.))
						.cursor_pointer()
						.hover(|s| s.bg(rgb(pal().hover_bg)))
						.tooltip(tip(t("tip_vcs_branch", loc)))
						.on_click(cx.listener(|this, _, window, cx| {
							this.open_popover(Popover::Ref, window, cx)
						}))
						.child(icon(Icon::Branch, 12.))
						.child(clip_text(b).text_color(rgb(pal().text)))
						.children(probe(&self.probes, "status-vcs")),
				)
			})
			.child(
				button(
					"btn-locale",
					t("btn_toggle_lang", loc),
					Btn::Ghost,
					true,
					6,
				)
				.h(px(STATUS_H - 4.))
				.px(px(6.))
				.text_size(px(SMALL_TEXT))
				.text_color(rgb(pal().text_muted))
				.tooltip(tip(t("tip_language", loc)))
				.on_click(cx.listener(|this, _, _, cx| this.toggle_locale(cx)))
				.children(probe(&self.probes, "btn-locale")),
			)
			.children(probe(&self.probes, "status-bar"))
			.into_any_element()
	}
}
