//! Tool window rail and the left tool window: its toolbar, the
//! Project tree rows and speed search.

use super::*;

impl WorkbenchModel {
	// ───────────────────────── rail ─────────────────────────

	pub(super) fn render_rail(&self, cx: &mut Context<Self>) -> AnyElement {
		let loc = self.locale;
		let log = &self.probes;
		let project_on =
			self.left_visible && self.active_tab == WorkbenchTab::FileExplorer;
		let changes_on =
			self.left_visible && self.active_tab == WorkbenchTab::GitChanges;
		let rail_button = |id: &'static str,
		                   ic: Icon,
		                   on: bool,
		                   label: &'static str,
		                   tab: isize| {
			div()
				.id(id)
				.relative()
				.size(px(28.))
				.rounded(px(5.))
				.flex()
				.items_center()
				.justify_center()
				.cursor_pointer()
				.border_1()
				.border_color(transparent_black())
				.tab_index(tab)
				// Islands: the open tool is a filled accent pill on the frame.
				.when(on, |d| d.bg(rgb(pal().rail_active_bg)))
				.when(!on, |d| d.hover(|s| s.bg(rgb(pal().hover_bg))))
				.map(focus_ring)
				.tooltip(tip(label))
				.child(if on {
					icon_tinted(ic, 16., pal().accent_text).into_any_element()
				} else {
					icon(ic, 16.).into_any_element()
				})
				.children(probe(log, id))
		};
		div()
			.flex()
			.flex_col()
			.items_center()
			.flex_shrink_0()
			.w(px(RAIL_W))
			.h_full()
			.py(px(6.))
			.gap(px(4.))
			.child(
				rail_button(
					"rail-project",
					Icon::Project,
					project_on,
					t("tip_project", loc),
					10,
				)
				.on_click(cx.listener(|this, _, _, cx| {
					this.activate_tool(WorkbenchTab::FileExplorer, cx)
				})),
			)
			.child(
				rail_button(
					"rail-changes",
					Icon::Changes,
					changes_on,
					t("tip_changes", loc),
					11,
				)
				.on_click(cx.listener(|this, _, _, cx| {
					this.activate_tool(WorkbenchTab::GitChanges, cx)
				})),
			)
			.child(div().flex_1())
			.child(
				rail_button(
					"rail-log",
					Icon::GitLog,
					self.bottom_visible,
					t("tip_git_log", loc),
					12,
				)
				.on_click(cx.listener(|this, _, _, cx| this.toggle_log(cx))),
			)
			.into_any_element()
	}

	// ───────────────────────── left tool window ─────────────────────────

	pub(super) fn render_left(
		&self,
		width: f32,
		cx: &mut Context<Self>,
	) -> AnyElement {
		let loc = self.locale;
		let is_project = self.active_tab == WorkbenchTab::FileExplorer;
		let title = if let (true, Some(tree)) = (is_project, &self.rev_tree) {
			tf("project_rev_title", loc, &[&short(&tree.sha)])
		} else if is_project {
			t("project", loc).to_string()
		} else {
			self.changes_title_text()
		};
		let header = div()
			.flex()
			.flex_row()
			.items_center()
			.flex_shrink_0()
			.h(px(PANEL_HEADER_H))
			.pl(px(10.))
			.pr(px(4.))
			.gap(px(2.))
			.child(fill_text(title).font_weight(FontWeight::SEMIBOLD))
			.when(is_project && self.rev_tree.is_some(), |d| {
				d.child(
					icon_button(
						"btn-leave-tree",
						Icon::Back,
						t("btn_back_to_working", loc),
						true,
						13,
					)
					.on_click(
						cx.listener(|this, _, _, cx| this.leave_rev_tree(cx)),
					)
					.children(probe(&self.probes, "btn-leave-tree")),
				)
			})
			.when(is_project && self.rev_tree.is_none(), |d| {
				d.child(
					icon_button(
						"btn-add-repo",
						Icon::Plus,
						t("tip_add_repo", loc),
						true,
						12,
					)
					.when(self.is_adding_repo, |b| {
						b.bg(rgb(pal().rail_active_bg))
					})
					.on_click(cx.listener(|this, _, _, cx| {
						this.is_adding_repo = !this.is_adding_repo;
						cx.notify();
					}))
					.children(probe(&self.probes, "btn-add-repo")),
				)
				.when(
					matches!(
						self.discovery_status,
						Some(
							ScanStatus::LimitReached
								| ScanStatus::Incomplete | ScanStatus::More
								| ScanStatus::Cancelled | ScanStatus::TimedOut
						)
					),
					|d| {
						d.child(
							icon_button(
								"btn-discovery-continue",
								Icon::ArrowDown,
								t("btn_discovery_continue", loc),
								true,
								10,
							)
							.on_click(cx.listener(|this, _, _, cx| {
								this.continue_discovery(cx);
							}))
							.children(probe(
								&self.probes,
								"btn-discovery-continue",
							)),
						)
					},
				)
				.when(!self.discovery_errors.is_empty(), |d| {
					d.child(
						icon_button(
							"btn-discovery-retry",
							Icon::Refresh,
							t("btn_discovery_retry", loc),
							true,
							11,
						)
						.on_click(cx.listener(|this, _, _, cx| {
							this.reload_repos(cx);
						}))
						.children(probe(&self.probes, "btn-discovery-retry")),
					)
				})
			})
			.when(!is_project, |d| {
				let on = self.chrome.changes_by_dir;
				d.child(
					icon_button(
						"btn-changes-group-dir",
						Icon::Folder,
						t("tip_group_by_dir", loc),
						true,
						12,
					)
					.when(on, |d| {
						d.bg(rgb(pal().range_bg))
							.border_color(rgb(pal().accent))
					})
					.on_click(cx.listener(|this, _, _, cx| {
						this.toggle_changes_by_dir(cx)
					}))
					.children(probe(&self.probes, "btn-changes-group-dir")),
				)
				.child(
					icon_button(
						"btn-select-all",
						Icon::SelectAll,
						t("btn_select_all", loc),
						true,
						13,
					)
					.on_click(
						cx.listener(|this, _, _, cx| this.select_all_files(cx)),
					),
				)
			})
			.when(self.rev_tree.is_none() || !is_project, |d| {
				d.child(
					icon_button(
						"btn-select-none",
						Icon::SelectNone,
						t("btn_deselect_all", loc),
						true,
						14,
					)
					.on_click(
						cx.listener(|this, _, _, cx| {
							this.deselect_all_files(cx)
						}),
					),
				)
			});
		let n = if is_project {
			self.project_rows().len()
		} else {
			self.change_item_rows().len()
		};
		let list = uniform_list(
			"left-rows",
			n,
			cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
				if this.active_tab == WorkbenchTab::FileExplorer {
					let rows = this.project_rows();
					let mut out = Vec::new();
					for (ix, row) in rows.into_iter().enumerate() {
						if range.contains(&ix) {
							out.push(this.project_row(ix, row, cx));
						}
					}
					out
				} else {
					let rows = this.change_item_rows();
					let mut out = Vec::new();
					for (row_idx, item) in rows.into_iter().enumerate() {
						if range.contains(&row_idx) {
							match item {
								ChangeItemRow::Repo {
									slot,
									group_id,
									count,
								} => {
									out.push(this.change_repo_row(
										slot, group_id, count, row_idx, cx,
									));
								}
								ChangeItemRow::Note { slot } => {
									out.push(this.change_note_row(slot));
								}
								ChangeItemRow::Header {
									label,
									count,
									group_id,
								} => {
									out.push(this.change_header_row(
										label, count, group_id, row_idx, cx,
									));
								}
								ChangeItemRow::Dir {
									slot,
									group_id,
									path,
									name,
									count,
									depth,
								} => {
									out.push(this.change_dir_row(
										ChangeDirRow {
											slot,
											group_id,
											path,
											name,
											count,
											depth,
										},
										row_idx,
										cx,
									));
								}
								ChangeItemRow::File { file_idx, depth } => {
									out.push(this.change_row(
										file_idx, depth, row_idx, cx,
									));
								}
							}
						}
					}
					out
				}
			}),
		)
		// Islands: rows are inset and rounded inside the island.
		.px(px(4.))
		.track_scroll(self.chrome.left_scroll.clone())
		.size_full();

		div()
			.flex()
			.flex_col()
			.flex_shrink_0()
			.w(px(width))
			.h_full()
			.bg(rgb(pal().panel_bg))
			.rounded(px(ISLAND_RADIUS))
			.child(header)
			.when(is_project && self.is_adding_repo, |d| {
				d.child(
					div()
						.flex()
						.flex_row()
						.items_center()
						.px(px(6.))
						.py(px(4.))
						.gap(px(4.))
						.child(
							div()
								.flex_1()
								.id(SharedString::from("input-add-repo"))
								.child(self.add_repo_input.clone())
								.children(probe(
									&self.probes,
									"input-add-repo",
								)),
						),
				)
			})
			.when(is_project && !self.discovery_errors.is_empty(), |d| {
				let err_msg =
					format!("{} scan error(s)", self.discovery_errors.len());
				d.child(
					div()
						.id(SharedString::from("discovery-error"))
						.flex()
						.flex_row()
						.items_center()
						.px(px(10.))
						.py(px(2.))
						.text_size(px(SMALL_TEXT))
						.text_color(rgb(pal().error))
						.child(clip_text(err_msg))
						.children(probe(&self.probes, "discovery-error")),
				)
			})
			.child(
				div()
					.id("left-list")
					.relative()
					.key_context("ToolList")
					.track_focus(&self.tree_focus)
					.on_action(cx.listener(|this, _: &TreeUp, w, cx| {
						this.tool_action("up", w, cx)
					}))
					.on_action(cx.listener(|this, _: &TreeDown, w, cx| {
						this.tool_action("down", w, cx)
					}))
					.on_action(cx.listener(|this, _: &TreeExpand, w, cx| {
						this.tool_action("expand", w, cx)
					}))
					.on_action(cx.listener(|this, _: &TreeCollapse, w, cx| {
						this.tool_action("collapse", w, cx)
					}))
					.on_action(cx.listener(|this, _: &TreeOpen, w, cx| {
						this.tool_action("open", w, cx)
					}))
					.on_action(cx.listener(|this, _: &TreeToggle, w, cx| {
						this.tool_action("toggle", w, cx)
					}))
					.on_action(cx.listener(
						|this, _: &crate::ToolPageUp, w, cx| {
							this.tool_move(-this.tool_page_rows(w), cx)
						},
					))
					.on_action(cx.listener(
						|this, _: &crate::ToolPageDown, w, cx| {
							this.tool_move(this.tool_page_rows(w), cx)
						},
					))
					.on_key_down(cx.listener(
						|this, ev: &gpui::KeyDownEvent, _, cx| {
							this.speed_key(ev, cx)
						},
					))
					.flex_1()
					.min_h_0()
					.when(is_project && n == 0, |d| {
						d.child(
							div()
								.p(px(10.))
								.text_color(rgb(pal().text_muted))
								.child(t("empty_project", loc)),
						)
					})
					.when_some(
						(!is_project)
							.then(|| self.changes_empty_state())
							.flatten(),
						|d, state| {
							let text = match state {
								crate::ChangesEmpty::Scanning => {
									t("changes_scanning", loc).to_string()
								}
								crate::ChangesEmpty::Loading => {
									t("changes_loading", loc).to_string()
								}
								crate::ChangesEmpty::NoRepository => {
									t("changes_no_repository", loc).to_string()
								}
								crate::ChangesEmpty::ScanFailed(msg) => {
									msg.render(loc)
								}
								crate::ChangesEmpty::NoMatch => {
									t("changes_no_match", loc).to_string()
								}
								crate::ChangesEmpty::CleanPartial => {
									t("changes_clean_partial", loc).to_string()
								}
								crate::ChangesEmpty::Clean => {
									t("clean_working_copy", loc).to_string()
								}
							};
							d.child(
								div()
									.relative()
									.p(px(10.))
									.text_color(rgb(pal().text_muted))
									.child(text)
									.children(probe(
										&self.probes,
										"changes-empty",
									)),
							)
						},
					)
					.child(list)
					.when(!self.chrome.speed.is_empty(), |d| {
						d.child(self.speed_search_popup())
					})
					.children(probe(&self.probes, "left-list")),
			)
			.into_any_element()
	}

	/// Row label with the speed-search match highlighted (IntelliJ paints
	/// the matched substring), plain text otherwise.
	pub(super) fn speed_label(&self, text: String) -> Div {
		let q = self.chrome.speed.to_lowercase();
		let hit = (!q.is_empty() && self.left_active)
			.then(|| {
				// Lowercasing may change byte lengths; only highlight when
				// the match maps back onto the original text.
				let lower = text.to_lowercase();
				(lower.len() == text.len())
					.then(|| lower.find(&q).map(|i| i..i + q.len()))
					.flatten()
			})
			.flatten()
			.filter(|r| {
				text.is_char_boundary(r.start) && text.is_char_boundary(r.end)
			});
		let base = div()
			.min_w_0()
			.overflow_hidden()
			.line_clamp(1)
			.text_ellipsis();
		match hit {
			Some(range) => {
				base.child(gpui::StyledText::new(text).with_highlights([(
					range,
					gpui::HighlightStyle {
						// IntelliJ paints speed-search matches amber with
						// dark text in both themes.
						background_color: Some(rgb(LIGHT.find_bg).into()),
						color: Some(rgb(0x000000).into()),
						..Default::default()
					},
				)]))
			}
			None => base.child(text),
		}
	}

	/// IntelliJ speed search field, floating over the tool window header
	/// at the top-left of the list so no row is covered.
	pub(super) fn speed_search_popup(&self) -> AnyElement {
		let none = self.tool_row_labels().iter().all(|l| {
			!l.to_lowercase().contains(&self.chrome.speed.to_lowercase())
		});
		div()
			.id("speed-search")
			.absolute()
			.top(px(3. - PANEL_HEADER_H))
			.left(px(6.))
			.flex()
			.items_center()
			.gap(px(4.))
			.h(px(22.))
			.px(px(6.))
			.max_w(px(240.))
			.rounded(px(4.))
			.bg(rgb(pal().popup_bg))
			.border_1()
			.border_color(rgb(if none {
				pal().error
			} else {
				pal().popup_border
			}))
			.shadow_md()
			.text_size(px(SMALL_TEXT))
			.child(icon(Icon::Search, 12.))
			.child(
				clip_text(self.chrome.speed.clone()).text_color(rgb(if none {
					pal().error
				} else {
					pal().text
				})),
			)
			.children(probe(&self.probes, "speed-search"))
			.into_any_element()
	}

	/// Bright while the left list has focus, grey otherwise, like IntelliJ.
	pub(super) fn left_selection_bg(&self) -> u32 {
		if self.left_active {
			pal().selection_bg
		} else {
			pal().selection_inactive_bg
		}
	}

	pub(super) fn project_row(
		&self,
		ix: usize,
		row: ProjRow,
		cx: &mut Context<Self>,
	) -> AnyElement {
		let log = &self.probes;
		let cursor = ix == self.tree_cursor;
		match row {
			ProjRow::Repo(idx, depth) => {
				let repo = &self.repos[idx];
				let expanded = self.repo_row_open(idx);
				let id = format!("repo-row:{}", repo.name);
				let chevron_id = format!("repo-chevron:{}", repo.name);
				let mut counts = div()
					.flex()
					.flex_row()
					.flex_shrink_0()
					.gap(px(5.))
					.text_size(px(SMALL_TEXT));
				let tooltip;
				match &repo.summary {
					Ok(s) => {
						let parts = [
							("+", s.changes.staged, pal().git_added),
							("~", s.changes.unstaged, pal().git_modified),
							("?", s.changes.untracked, pal().git_untracked),
							("!", s.changes.conflicted, pal().git_conflict),
						];
						let mut any = false;
						for (sym, n, color) in parts {
							if n > 0 {
								any = true;
								counts = counts.child(
									div()
										.text_color(rgb(color))
										.child(format!("{sym}{n}")),
								);
							}
						}
						if !any {
							counts = counts.child(
								div()
									.text_color(rgb(pal().text_muted))
									.child(t("clean", self.locale)),
							);
						}
						tooltip = format!(
							"{}\n{}",
							repo.root.display(),
							t("counts_tip", self.locale)
						);
					}
					Err(e) => {
						counts = counts.child(
							div()
								.text_color(rgb(pal().error))
								.child(t("repo_error_short", self.locale)),
						);
						tooltip = format!("{}\n{}", repo.root.display(), e);
					}
				}
				let branch = match &repo.summary {
					Ok(s) => {
						if let Some(ref b) = s.branch {
							b.clone()
						} else if let Some(ref h) = s.head {
							format!("({})", &h[..7.min(h.len())])
						} else {
							format!("({})", t("repo_unborn", self.locale))
						}
					}
					Err(_) => String::new(),
				};
				let kind_badge = match repo.kind {
					RepoEntryKind::LinkedWorktree => {
						Some(t("repo_kind_worktree", self.locale))
					}
					RepoEntryKind::Submodule => {
						Some(t("repo_kind_submodule", self.locale))
					}
					RepoEntryKind::UninitializedSubmodule => {
						Some(t("repo_kind_uninit_submodule", self.locale))
					}
					RepoEntryKind::Main => None,
				};
				let is_err = repo.summary.is_err()
					|| repo.kind == RepoEntryKind::UninitializedSubmodule;
				div()
					.id(SharedString::from(id.clone()))
					.relative()
					.flex()
					.flex_row()
					.items_center()
					.w_full()
					.h(px(ROW_H))
					.pl(px(6. + depth as f32 * 14.))
					.pr(px(6.))
					.gap(px(5.))
					.cursor_pointer()
					.rounded(px(4.))
					.when(cursor, |d| d.bg(rgb(self.left_selection_bg())))
					.when(!cursor, |d| d.hover(|s| s.bg(rgb(pal().hover_bg))))
					.when(self.chrome.menu.is_none(), |d| {
						d.tooltip(tip(tooltip))
					})
					.on_click(cx.listener(move |this, _, _, cx| {
						this.tree_cursor = ix;
						this.toggle_repo_row(idx, cx);
					}))
					.on_mouse_down(
						MouseButton::Right,
						cx.listener(move |this, ev: &MouseDownEvent, w, cx| {
							this.tree_cursor = ix;
							let items = this.repo_row_menu(idx);
							this.open_left_menu(items, ev, w, cx);
						}),
					)
					.child(
						div()
							.id(SharedString::from(chevron_id.clone()))
							.relative()
							.flex_shrink_0()
							.on_click(cx.listener(move |this, _, _, cx| {
								cx.stop_propagation();
								this.tree_cursor = ix;
								this.toggle_repo_chevron(idx, cx);
							}))
							.child(icon(
								if expanded {
									Icon::ChevronDown
								} else {
									Icon::ChevronRight
								},
								10.,
							))
							.children(probe(log, chevron_id)),
					)
					.child(icon(
						if is_err { Icon::Warning } else { Icon::Project },
						14.,
					))
					.child(
						self.speed_label(repo.name.clone())
							.font_weight(FontWeight::SEMIBOLD),
					)
					.when_some(kind_badge, |d, badge| {
						d.child(
							div()
								.flex_shrink_0()
								.px(px(4.))
								.rounded(px(3.))
								.border_1()
								.border_color(rgb(pal().divider))
								.text_size(px(10.))
								.text_color(rgb(pal().text_muted))
								.child(badge),
						)
					})
					.child(
						fill_text(branch)
							.text_size(px(SMALL_TEXT))
							.text_color(rgb(pal().ref_local)),
					)
					.child(counts)
					.children(probe(log, id))
					.into_any_element()
			}
			ProjRow::Work(row) => {
				self.work_tree_row(ix, row, cursor, false, cx)
			}
			ProjRow::Ws(row) => self.work_tree_row(ix, row, cursor, true, cx),
			ProjRow::Rev(row) => self.rev_tree_row(ix, row, cursor, cx),
		}
	}

	/// `ws`: a workspace-tree row (outside every repo), probed as `ws-*`.
	pub(super) fn work_tree_row(
		&self,
		ix: usize,
		row: FlattenedTreeRow,
		cursor: bool,
		ws: bool,
		cx: &mut Context<Self>,
	) -> AnyElement {
		let log = &self.probes;
		let pre = if ws { "ws-" } else { "" };
		let dispatch = move |this: &mut Self, cmd, cx: &mut Context<Self>| {
			if ws {
				this.dispatch_ws_tree(cmd, cx);
			} else {
				this.dispatch_tree(cmd, cx);
			}
		};
		let indent = 8.0 + row.depth as f32 * 14.0;
		if row.is_error
			|| row.is_truncation_marker
			|| row.is_more_marker
			|| row.is_view_limit
			|| row.is_loading
		{
			let marker_id = if row.is_view_limit {
				format!("{pre}tree-view-more:{}", row.rel_path)
			} else if row.is_more_marker {
				format!("{pre}tree-continue:{}", row.rel_path)
			} else if row.is_error {
				format!("{pre}tree-retry:{}", row.rel_path)
			} else if row.is_loading {
				format!("{pre}tree-loading:{}", row.rel_path)
			} else {
				format!("{pre}tree-marker:{}", row.rel_path)
			};
			let action_row = row.clone();
			let is_actionable =
				command_for_row(&row, RowGesture::Primary).is_some();
			return div()
				.id(SharedString::from(marker_id.clone()))
				.relative()
				.w_full()
				.h(px(ROW_H))
				.pl(px(indent + 28.0))
				.flex()
				.items_center()
				.text_size(px(SMALL_TEXT))
				.text_color(rgb(if row.is_error {
					pal().error
				} else {
					pal().text_muted
				}))
				.when(is_actionable, |d| {
					d.cursor_pointer().hover(|s| s.bg(rgb(pal().hover_bg)))
				})
				.on_click(cx.listener(move |this, _, _, cx| {
					dispatch(
						this,
						command_for_row(&action_row, RowGesture::Primary),
						cx,
					);
				}))
				.child(clip_text(row.name))
				.children(probe(log, marker_id))
				.into_any_element();
		}
		let rel = row.rel_path.clone();
		let click_row = row.clone();
		let chevron_row = row.clone();
		let chevron_id = format!("{pre}tree-chevron:{rel}");
		let menu_row = row.clone();
		let is_dir = row.is_dir;
		let row_id = if row.is_valid_utf8 {
			format!("{pre}tree-row:{rel}")
		} else {
			format!("{pre}tree-invalid:{}", row.id_suffix)
		};
		let is_valid_utf8 = row.is_valid_utf8;

		// Git status as filename colour, like IntelliJ's Project view.
		let open_rows = self
			.selected_change_slot()
			.filter(|_| !ws)
			.map_or(0..0, |slot| crate::slot_range(&self.files, slot));
		let name_color = self.files[open_rows]
			.iter()
			.find(|f| f.path == rel)
			.map(|f| {
				if f.is_conflict {
					pal().git_conflict
				} else if f.source == SourceKind::Working {
					pal().git_untracked
				} else {
					change_style(f.change_type).1
				}
			});
		div()
			.id(SharedString::from(row_id.clone()))
			.relative()
			.flex()
			.flex_row()
			.items_center()
			.w_full()
			.h(px(ROW_H))
			.pl(px(indent))
			.pr(px(6.))
			.gap(px(5.))
			.when(is_valid_utf8, |d| d.cursor_pointer())
			.rounded(px(4.))
			// Selected rows are the Project selection (IntelliJ: no checks).
			.when(cursor || row.selected, |d| {
				d.bg(rgb(self.left_selection_bg()))
			})
			.when(!cursor && !row.selected, |d| {
				d.hover(|s| s.bg(rgb(pal().hover_bg)))
			})
			.on_mouse_down(
				MouseButton::Right,
				cx.listener(move |this, ev: &MouseDownEvent, w, cx| {
					this.tree_cursor = ix;
					// IntelliJ: right-clicking outside the selection
					// selects that row alone first.
					let mut row = menu_row.clone();
					if !row.selected && row.is_valid_utf8 {
						let rel = [row.rel_path.clone()];
						this.select_tree_rows_alone(ws, &rel, cx);
						let tree =
							if ws { &this.ws_tree } else { &this.file_tree };
						row.selected = tree.as_ref().is_some_and(|t| {
							t.selected_paths().binary_search(&rel[0]).is_ok()
						});
					}
					let items = this.work_row_menu(&row, ws);
					this.open_left_menu(items, ev, w, cx);
				}),
			)
			.when(!is_valid_utf8, |d| {
				d.on_click(cx.listener(|this, _, _, cx| {
					this.refuse_unaddressable_row(cx)
				}))
			})
			.when(is_valid_utf8, |d| {
				d.on_click(cx.listener(
					move |this, ev: &gpui::ClickEvent, _, cx| {
						// Cmd on macOS, Ctrl elsewhere toggles the row;
						// Shift selects the range from the cursor.
						if ev.modifiers().secondary() {
							this.tree_cursor = ix;
							dispatch(
								this,
								command_for_row(&click_row, RowGesture::Toggle),
								cx,
							);
						} else if ev.modifiers().shift {
							this.select_tree_range(ix, ws, cx);
						} else {
							this.tree_cursor = ix;
							// IntelliJ: a plain click selects the row alone,
							// then opens the folder or previews the file.
							let rel = [click_row.rel_path.clone()];
							this.select_tree_rows_alone(ws, &rel, cx);
							dispatch(
								this,
								command_for_row(
									&click_row,
									RowGesture::Primary,
								),
								cx,
							);
						}
					},
				))
			})
			// IntelliJ: the chevron opens or closes the folder without
			// selecting it; the row click selects it alone and opens it.
			.child(if is_dir {
				div()
					.id(SharedString::from(chevron_id.clone()))
					.relative()
					.flex_shrink_0()
					.on_click(cx.listener(move |this, _, _, cx| {
						cx.stop_propagation();
						this.tree_cursor = ix;
						dispatch(
							this,
							command_for_row(&chevron_row, RowGesture::Primary),
							cx,
						);
					}))
					.child(icon(
						if row.is_expanded {
							Icon::ChevronDown
						} else {
							Icon::ChevronRight
						},
						10.,
					))
					.children(probe(log, chevron_id))
					.into_any_element()
			} else {
				div().flex_shrink_0().w(px(10.)).into_any_element()
			})
			.child(icon(
				if is_dir {
					if row.is_expanded {
						Icon::FolderOpen
					} else {
						Icon::Folder
					}
				} else {
					file_icon(&rel)
				},
				14.,
			))
			.child(
				self.speed_label(row.name.clone())
					.flex_1()
					.when_some(name_color, |d, c| d.text_color(rgb(c))),
			)
			.when(row.is_nested_repo, |d| {
				d.child(
					div()
						.flex_shrink_0()
						.px(px(4.))
						.rounded(px(3.))
						.border_1()
						.border_color(rgb(pal().divider))
						.text_size(px(10.))
						.text_color(rgb(pal().text_muted))
						.child("repo"),
				)
			})
			.when(!is_valid_utf8, |d| {
				d.child(
					div()
						.flex_shrink_0()
						.px(px(4.))
						.rounded(px(3.))
						.bg(rgb(pal().hover_bg))
						.text_size(px(10.))
						.text_color(rgb(pal().text_muted))
						.child("invalid UTF-8"),
				)
			})
			.children(probe(log, row_id))
			.into_any_element()
	}

	pub(super) fn rev_tree_row(
		&self,
		ix: usize,
		row: RevRow,
		cursor: bool,
		cx: &mut Context<Self>,
	) -> AnyElement {
		let log = &self.probes;
		let indent = 8.0 + row.depth as f32 * 14.0;
		if let Some(m) = &row.marker {
			return div()
				.w_full()
				.h(px(ROW_H))
				.pl(px(indent + 14.0))
				.flex()
				.items_center()
				.text_size(px(SMALL_TEXT))
				.text_color(rgb(pal().text_muted))
				.child(clip_text(m.render(self.locale)))
				.into_any_element();
		}
		let is_dir = row.kind == snip_core::browser::TreeKind::Tree;
		let submodule = row.kind == snip_core::browser::TreeKind::Submodule;
		let is_file = row.kind == snip_core::browser::TreeKind::Blob;
		let path = row.path.clone();
		let tree_sha = self
			.rev_tree
			.as_ref()
			.map(|t| t.sha.clone())
			.unwrap_or_default();
		let is_basket_selected =
			is_file && self.is_rev_file_selected(&tree_sha, &path);
		let id = format!("rev-row:{path}");
		let chk_id = format!("rev-chk:{}:{}", tree_sha, path);
		div()
			.id(SharedString::from(id.clone()))
			.relative()
			.flex()
			.flex_row()
			.items_center()
			.w_full()
			.h(px(ROW_H))
			.pl(px(indent))
			.pr(px(6.))
			.gap(px(5.))
			.cursor_pointer()
			.rounded(px(4.))
			.when(cursor, |d| d.bg(rgb(self.left_selection_bg())))
			.when(!cursor, |d| d.hover(|s| s.bg(rgb(pal().hover_bg))))
			.on_mouse_down(MouseButton::Right, {
				let menu_path = path.clone();
				cx.listener(move |this, ev: &MouseDownEvent, w, cx| {
					this.tree_cursor = ix;
					let items = this.rev_row_menu(&menu_path, is_file);
					this.open_left_menu(items, ev, w, cx);
				})
			})
			.when(!submodule, |d| {
				let row_path = path.clone();
				d.on_click(cx.listener(move |this, _, _, cx| {
					this.tree_cursor = ix;
					this.rev_tree_click(&row_path, is_dir, cx);
				}))
			})
			.child(if is_dir {
				icon(
					if row.expanded {
						Icon::ChevronDown
					} else {
						Icon::ChevronRight
					},
					10.,
				)
				.into_any_element()
			} else {
				div().flex_shrink_0().w(px(10.)).into_any_element()
			})
			.child(if is_file {
				div()
					.id(SharedString::from(chk_id.clone()))
					.relative()
					.flex_shrink_0()
					.size(px(18.))
					.flex()
					.items_center()
					.justify_center()
					.cursor_pointer()
					.on_click(cx.listener({
						let chk_path = path.clone();
						let sha = tree_sha.clone();
						move |this, _, _, cx| {
							cx.stop_propagation();
							this.tree_cursor = ix;
							this.toggle_rev_file_selection(&sha, &chk_path, cx);
						}
					}))
					.child(checkbox(is_basket_selected))
					.children(probe(log, chk_id))
					.into_any_element()
			} else {
				div().flex_shrink_0().size(px(18.)).into_any_element()
			})
			.child(icon(
				if is_dir {
					if row.expanded {
						Icon::FolderOpen
					} else {
						Icon::Folder
					}
				} else if submodule {
					Icon::Project
				} else {
					file_icon(&path)
				},
				14.,
			))
			.child(self.speed_label(row.name.clone()).flex_1())
			.when(submodule, |d| {
				d.child(
					div()
						.flex_shrink_0()
						.text_size(px(10.))
						.text_color(rgb(pal().text_muted))
						.child(t("submodule", self.locale)),
				)
			})
			.children(probe(log, id))
			.into_any_element()
	}
}
