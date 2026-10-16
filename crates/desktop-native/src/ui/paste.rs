//! Paste preview in the editor area.

use super::*;
use crate::paste::{commit_header_id, PasteCounts, RowControl};

impl WorkbenchModel {
	// ───────────────────────── paste ─────────────────────────

	pub(super) fn disambiguate_candidate_labels(
		candidates: &[PathBuf],
	) -> Vec<String> {
		let orig_basenames: Vec<String> = candidates
			.iter()
			.map(|p| {
				p.file_name()
					.map(|n| n.to_string_lossy().into_owned())
					.unwrap_or_else(|| p.display().to_string())
			})
			.collect();
		let mut labels = orig_basenames.clone();
		let mut has_dups = false;
		let mut dup_indices = Vec::new();
		for i in 0..candidates.len() {
			let is_dup = orig_basenames
				.iter()
				.enumerate()
				.any(|(j, l)| j != i && *l == orig_basenames[i]);
			if is_dup {
				has_dups = true;
				dup_indices.push(i);
			}
		}
		if has_dups {
			for &i in &dup_indices {
				let comps: Vec<_> = candidates[i]
					.components()
					.map(|c| c.as_os_str().to_string_lossy().into_owned())
					.collect();
				if comps.len() >= 2 {
					labels[i] = format!(
						"{}/{}",
						comps[comps.len() - 2],
						comps[comps.len() - 1]
					);
				} else {
					labels[i] = candidates[i].display().to_string();
				}
			}
			for i in 0..candidates.len() {
				let still_dup = labels
					.iter()
					.enumerate()
					.any(|(j, l)| j != i && *l == labels[i]);
				if still_dup {
					labels[i] = candidates[i].display().to_string();
				}
			}
		}
		labels
	}

	/// Apply and Cancel. Apply stays locked while a read-only preview,
	/// remap or retarget is still running, so an older plan cannot be
	/// written, and while the destination field is open, so a path typed
	/// but not yet confirmed cannot be pasted into the old folder.
	pub(super) fn paste_action_bar(
		&self,
		applying: bool,
		mapping_ready: bool,
		executable: bool,
		cx: &mut Context<Self>,
	) -> Div {
		let loc = self.locale;
		let log = &self.probes;
		let loading = self.paste.is_loading();
		let editing = self.paste.dest_edit().is_some();
		let can_apply = !applying && executable && !loading && !editing;
		div()
			.flex()
			.flex_row()
			.items_center()
			.flex_shrink_0()
			.h(px(38.))
			.px(px(12.))
			.gap(px(8.))
			.bg(rgb(pal().panel_bg))
			.border_b_1()
			.border_color(rgb(pal().border))
			.child(div().flex_1().min_w_0())
			.child(
				button("btn-apply", "", Btn::Primary, can_apply, 50)
					.pl(px(6.))
					.child(icon_tinted(
						Icon::Apply,
						14.,
						if can_apply {
							pal().accent_text
						} else {
							pal().text_disabled
						},
					))
					.child(if applying {
						t("applying", loc)
					} else {
						t("apply", loc)
					})
					.when(loading, |b| {
						b.tooltip(tip(t("paste_loading_refused", loc)))
					})
					.when(!loading && editing, |b| {
						b.tooltip(tip(t("paste_dest_editing", loc)))
					})
					.when(!loading && !editing && !mapping_ready, |b| {
						b.tooltip(tip(t("mapping_required", loc)))
					})
					.when(can_apply, |b| {
						b.on_click(cx.listener(|this, _, _, cx| {
							this.apply_paste_restore(cx)
						}))
					})
					.children(probe(log, "btn-apply")),
			)
			.child(
				// The write cannot be interrupted, so Cancel is locked until it
				// finishes rather than pretending to cancel it.
				button("btn-cancel", "", Btn::Default, !applying, 51)
					.pl(px(6.))
					.child(icon(Icon::Cancel, 14.))
					.child(t("cancel", loc))
					.when(!applying, |b| {
						b.on_click(cx.listener(|this, _, _, cx| {
							this.cancel_paste_preview(cx)
						}))
					})
					.when(applying, |b| {
						b.tooltip(tip(t("paste_busy_refused", loc)))
					})
					.children(probe(log, "btn-cancel")),
			)
	}

	pub(super) fn paste_loading_note(&self) -> Stateful<Div> {
		div()
			.id("paste-loading")
			.relative()
			.flex_shrink_0()
			.px(px(12.))
			.py(px(6.))
			.text_size(px(SMALL_TEXT))
			.text_color(rgb(pal().text_muted))
			.child(t("paste_loading", self.locale))
			.children(probe(&self.probes, "paste-loading"))
	}

	/// First preview still being read: no plan exists, so only Cancel works.
	pub(super) fn render_paste_loading(
		&self,
		cx: &mut Context<Self>,
	) -> AnyElement {
		div()
			.id("paste-panel")
			.relative()
			.key_context("PastePanel")
			.track_focus(&self.paste_focus)
			.flex()
			.flex_col()
			.flex_1()
			.min_w_0()
			.min_h_0()
			.h_full()
			.overflow_hidden()
			.bg(rgb(pal().editor_bg))
			.rounded(px(ISLAND_RADIUS))
			.child(self.tab_strip(
				t("paste_tab", self.locale).to_string(),
				Icon::Paste,
				false,
				Some(|this, cx| this.close_tab(0, cx)),
				cx,
			))
			.child(self.paste_action_bar(false, false, false, cx))
			.child(self.paste_dest_bar(cx))
			.child(self.paste_loading_note())
			.children(probe(&self.probes, "paste-panel"))
			.into_any_element()
	}

	/// One commit of a replay: index, message, author and date, then how
	/// many of its files are written. Shown for empty commits too, since
	/// Apply creates them.
	fn commit_header(
		plan: &PastePreviewPlan,
		c: usize,
		loc: Locale,
		log: &Option<Probes>,
		cx: &Context<Self>,
	) -> Stateful<Div> {
		let Some(commit) = plan
			.commit_preview
			.as_ref()
			.and_then(|p| p.plan().commits.get(c))
		else {
			return div().id("paste-commit:none");
		};
		let id = commit_header_id(c);
		let (subject, author, date) = commit_header_labels(commit, loc);
		let (files, skipped) = plan.commit_counts(c);
		let folded = plan.collapsed_commits.contains(&c);
		let counts = if commit.files.is_empty() {
			t("commit_empty_note", loc).to_string()
		} else {
			tf(
				"commit_header_counts",
				loc,
				&[&files.to_string(), &skipped.to_string()],
			)
		};
		div()
			.id(SharedString::from(id.clone()))
			.relative()
			.flex()
			.flex_row()
			.items_center()
			.flex_shrink_0()
			.h(px(26.))
			.px(px(8.))
			.gap(px(8.))
			.bg(rgb(pal().panel_bg))
			.text_size(px(SMALL_TEXT))
			.cursor_pointer()
			.on_click(cx.listener(move |this, _, _, cx| {
				this.toggle_paste_commit(c, cx)
			}))
			.when(c > 0, |d| d.border_t_1().border_color(rgb(pal().divider)))
			.tooltip(tip(format!(
				"{}\n{author}\n{}",
				commit.message.trim_end(),
				commit.author_date
			)))
			.child(icon(
				if folded {
					Icon::ChevronRight
				} else {
					Icon::ChevronDown
				},
				10.,
			))
			.child(
				div()
					.flex_shrink_0()
					.font_weight(FontWeight::SEMIBOLD)
					.child(format!("#{}", c + 1)),
			)
			.child(
				clip_text(subject)
					.flex_1()
					.font_weight(FontWeight::SEMIBOLD),
			)
			.child(
				clip_text(author)
					.flex_shrink()
					.text_color(rgb(pal().text_muted)),
			)
			.child(
				div()
					.flex_shrink_0()
					.text_color(rgb(pal().text_muted))
					.child(date),
			)
			.child(
				div()
					.flex_shrink_0()
					.text_color(rgb(pal().text_muted))
					.child(counts),
			)
			.children(probe(log, id))
	}

	pub(super) fn render_paste(
		&self,
		plan: &PastePreviewPlan,
		cx: &mut Context<Self>,
	) -> AnyElement {
		let loc = self.locale;
		let log = &self.probes;
		let PasteCounts {
			creates,
			overwrites,
			existing,
			deletes,
			skips,
		} = plan.counts();
		let commit_label = plan.commit_summary_label(loc);
		let applying = plan.is_applying;
		let mapping_ready = plan.mapping_ready();
		let selected = plan.items.get(plan.selected_item_idx);

		let loading = self.paste.is_loading();
		let action_bar = self.paste_action_bar(
			applying,
			mapping_ready,
			plan.executable(),
			cx,
		);
		let dest_bar = self.paste_dest_bar(cx);
		let tab = self.tab_strip(
			format!("{} ({})", t("paste_tab", loc), plan.items.len()),
			Icon::Paste,
			false,
			(!applying).then_some(
				(|this, cx| this.close_tab(0, cx))
					as fn(&mut Self, &mut Context<Self>),
			),
			cx,
		);

		let summary = div()
			.flex()
			.flex_row()
			.items_center()
			.flex_shrink_0()
			.h(px(24.))
			.px(px(12.))
			.gap(px(12.))
			.text_size(px(SMALL_TEXT))
			.border_b_1()
			.border_color(rgb(pal().divider))
			.children(commit_label.map(|label| {
				div()
					.id("paste-commit-count")
					.relative()
					.flex_shrink_0()
					.font_weight(FontWeight::SEMIBOLD)
					.child(label)
					.children(probe(log, "paste-commit-count"))
			}))
			.child(
				div()
					.flex_shrink_0()
					.text_color(rgb(pal().git_added))
					.child(format!("{} {creates}", t("op_create", loc))),
			)
			.child(
				div()
					.flex_shrink_0()
					.text_color(rgb(pal().git_modified))
					.child(format!(
						"{} {overwrites}/{existing}",
						t("op_overwrite", loc)
					)),
			)
			.child(
				div()
					.flex_shrink_0()
					.text_color(rgb(pal().git_deleted))
					.child(format!("{} {deletes}", t("op_delete", loc))),
			)
			.child(
				div()
					.flex_shrink_0()
					.text_color(rgb(pal().text_muted))
					.child(format!("{} {skips}", t("op_skip", loc))),
			)
			.child(
				fill_text(t("paste_keys", loc))
					.text_color(rgb(pal().text_disabled)),
			);

		// IntelliJ "Apply Patch": a change tree grouped by destination root
		// and directory; the file name's colour says what happens to it.
		let node_row = |depth: usize| {
			div()
				.flex()
				.flex_row()
				.items_center()
				.flex_shrink_0()
				.h(px(24.))
				.pl(px(8. + 18. * depth as f32))
				.pr(px(8.))
				.gap(px(6.))
		};
		let nodes = if plan.whole_commit {
			plan.commit_rows()
		} else {
			plan.tree_rows()
		};
		let rows = nodes.into_iter().map(|node| {
			let (ix, depth) = match node {
				PasteNode::Commit(c) => {
					return Self::commit_header(plan, c, loc, log, cx)
						.into_any_element();
				}
				PasteNode::Root(ref name, n) | PasteNode::Dir(ref name, n) => {
					let root = matches!(node, PasteNode::Root(..));
					let name = name.clone();
					return node_row(usize::from(!root))
						.child(icon(Icon::Folder, 14.))
						.child(clip_text(name).flex_shrink().when(root, |d| {
							d.font_weight(FontWeight::SEMIBOLD)
						}))
						.child(
							div()
								.flex_shrink_0()
								.text_size(px(SMALL_TEXT))
								.text_color(rgb(pal().text_muted))
								.child(n.to_string()),
						)
						.into_any_element();
				}
				PasteNode::File(ix, depth) => (ix, depth),
			};
			let item = &plan.items[ix];
			let (_, name_color, _) = paste_op(item);
			let is_sel = ix == plan.selected_item_idx;
			let path = item.path.clone();
			let row_id = RowControl::Row.id(ix, &path);
			let inc_id = RowControl::Include.id(ix, &path);
			let ow_id = RowControl::Overwrite.id(ix, &path);
			let can_overwrite = item.overwritable();
			let planned_skip = item.planned_skip();
			let by_commit = item.commit.is_some();
			let ow_on = item.overwrite_allowed;
			// A commit's rows are in replay order, so they keep the full path.
			let (_, base) = split_dir(&path);
			let name = if by_commit { path.as_str() } else { base };
			node_row(depth)
				.id(SharedString::from(row_id.clone()))
				.debug_selector(|| row_id.clone())
				.relative()
				.cursor_pointer()
				.when(is_sel, |d| d.bg(rgb(pal().selection_bg)))
				.when(!is_sel, |d| d.hover(|s| s.bg(rgb(pal().hover_bg))))
				.tooltip(tip(if planned_skip {
					format!("{path}\n{}", t(paste_op(item).2, loc))
				} else {
					format!("{path}\n→ {}", item.dest_path.display())
				}))
				.on_click(cx.listener(move |this, _, _, cx| {
					this.select_paste_item(ix, cx)
				}))
				.child(
					div()
						.id(SharedString::from(inc_id.clone()))
						.debug_selector(|| inc_id.clone())
						.relative()
						.flex_shrink_0()
						.size(px(16.))
						.flex()
						.items_center()
						.justify_center()
						.when(applying, |d| {
							d.opacity(0.4)
								.tooltip(tip(t("paste_busy_refused", loc)))
						})
						// Still routed to the model while applying so the refusal
						// is explicit (it logs and explains).
						.when(!planned_skip, |d| {
							d.on_click(cx.listener(move |this, _, _, cx| {
								cx.stop_propagation();
								this.toggle_paste_selected(ix, cx);
							}))
						})
						.when(planned_skip, |d| d.opacity(0.4))
						.child(checkbox(item.selected))
						.children(probe(log, inc_id)),
				)
				.child(icon(file_icon(base), 14.))
				.child(
					clip_text(name.to_string())
						.flex_1()
						.text_color(rgb(name_color)),
				)
				.children(item.rename_note.clone().map(|note| {
					clip_text(note)
						.flex_shrink()
						.text_size(px(SMALL_TEXT))
						.text_color(rgb(pal().text_muted))
				}))
				.when(planned_skip, |d| {
					let id = format!("paste-skip-reason:{ix}:{path}");
					d.child(
						div()
							.id(SharedString::from(id.clone()))
							.debug_selector(|| id.clone())
							.relative()
							.text_size(px(SMALL_TEXT))
							.text_color(rgb(pal().text_muted))
							.child(t(paste_op(item).2, loc))
							.children(probe(log, id)),
					)
				})
				.when(by_commit, |d| {
					d.child(
						div()
							.flex_shrink_0()
							.text_size(px(SMALL_TEXT))
							.text_color(rgb(name_color))
							.child(t(paste_op(item).0, loc)),
					)
				})
				.child(
					div()
						.flex_shrink_0()
						.text_size(px(SMALL_TEXT))
						.text_color(rgb(pal().text_muted))
						.child(if item.writes_content() {
							crate::paste::human_bytes(item.bytes as u64)
						} else {
							String::new()
						}),
				)
				// Fixed column so the toggles line up whether or not a row
				// can be overwritten.
				.child(div().flex_shrink_0().w(px(88.)).when(
					can_overwrite,
					|d| {
						d.child(
							div()
								.id(SharedString::from(ow_id.clone()))
								.debug_selector(|| ow_id.clone())
								.relative()
								.flex()
								.flex_row()
								.items_center()
								.gap(px(5.))
								.h(px(20.))
								.px(px(4.))
								.rounded(px(3.))
								.text_size(px(SMALL_TEXT))
								.text_color(rgb(if ow_on {
									pal().text
								} else {
									pal().text_muted
								}))
								.when(!applying, |d| {
									d.cursor_pointer()
										.hover(|s| s.bg(rgb(pal().hover_bg)))
								})
								.when(applying, |d| {
									d.opacity(0.4).tooltip(tip(t(
										"paste_busy_refused",
										loc,
									)))
								})
								.on_click(cx.listener(move |this, _, _, cx| {
									cx.stop_propagation();
									this.toggle_paste_overwrite(ix, cx);
								}))
								.child(checkbox(ow_on))
								.child(clip_text(t("overwrite_toggle", loc)))
								.children(probe(log, ow_id)),
						)
					},
				))
				.children(probe(log, row_id))
				.into_any_element()
		});

		let (detail_title, reason) = match selected {
			Some(it) if it.planned_skip() => {
				(it.path.clone(), t(paste_op(it).2, loc))
			}
			Some(it) => (
				format!("{} → {}", it.path, it.dest_path.display()),
				t(paste_op(it).2, loc),
			),
			None => (String::new(), ""),
		};

		let mappings = (!plan.prefix_choices.is_empty()).then(|| {
			div()
				.id("paste-mappings")
				.relative()
				.flex()
				.flex_col()
				.flex_shrink_0()
				.mx(px(8.))
				.my(px(6.))
				.rounded(px(4.))
				.border_1()
				.border_color(rgb(pal().divider))
				.bg(rgb(pal().panel_bg))
				.children(plan.prefix_choices.iter().enumerate().map(
					|(n, choice)| {
						let prefix = choice.prefix.clone();
						let kept = choice.keep_relative;
						let target = if kept {
							format!(
								"{}/{}",
								plan.shown(&plan.destination),
								choice.prefix
							)
						} else {
							choice
								.destination
								.as_ref()
								.map(|p| plan.shown(p))
								.unwrap_or_else(|| {
									t("mapping_unresolved", loc).into()
								})
						};
						let resolved = kept || choice.destination.is_some();
						let target_id = format!("paste-map-target:{prefix}");
						let row_id = format!("paste-map:{prefix}");
						let shown: Vec<PathBuf> = choice
							.candidates
							.iter()
							.map(|p| PathBuf::from(plan.shown(p)))
							.collect();
						let labels =
							Self::disambiguate_candidate_labels(&shown);
						let keep_id = format!("paste-map-keep:{prefix}");
						let prefix_keep = prefix.clone();
						let keep_btn = button(
							SharedString::from(keep_id.clone()),
							t("mapping_keep", loc),
							if kept { Btn::Primary } else { Btn::Default },
							!applying,
							69,
						)
						.tooltip(tip(format!(
							"{}/{}",
							plan.shown(&plan.destination),
							prefix
						)))
						.when(!applying, |b| {
							b.on_click(cx.listener(move |this, _, _, cx| {
								this.choose_paste_keep(&prefix_keep, cx);
							}))
						})
						.children(probe(log, keep_id));
						let picks = choice.candidates.iter().enumerate().map(
							|(idx, path)| {
								let picked =
									choice.destination.as_ref() == Some(path);
								let id =
									format!("paste-map-pick:{prefix}:{idx}");
								let full = plan.shown(path);
								let label = labels
									.get(idx)
									.cloned()
									.unwrap_or_else(|| full.clone());
								let prefix_click = prefix.clone();
								button(
									SharedString::from(id.clone()),
									label,
									if picked {
										Btn::Primary
									} else {
										Btn::Default
									},
									!applying,
									70 + idx as isize,
								)
								.debug_selector(|| id.clone())
								.tooltip(tip(full))
								.when(!applying, |b| {
									b.on_click(cx.listener(
										move |this, _, _, cx| {
											this.choose_paste_prefix(
												&prefix_click,
												idx,
												cx,
											);
										},
									))
								})
								.children(probe(log, id))
							},
						);
						// One wrapping line: source, choices, then the resolved
						// target filling what is left (wraps below when narrow).
						div()
							.id(SharedString::from(row_id.clone()))
							.relative()
							.flex()
							.flex_row()
							.flex_wrap()
							.items_center()
							.gap_x(px(6.))
							.gap_y(px(4.))
							.px(px(8.))
							.py(px(5.))
							.text_size(px(SMALL_TEXT))
							.when(n > 0, |d| {
								d.border_t_1().border_color(rgb(pal().divider))
							})
							.child(
								clip_text(tf(
									"mapping_prefix",
									loc,
									&[&prefix],
								))
								.flex_shrink_0()
								.max_w(px(240.))
								.font_weight(FontWeight::SEMIBOLD),
							)
							.child(keep_btn)
							.children(picks)
							.child(
								div()
									.id(SharedString::from(target_id.clone()))
									.relative()
									.flex_1()
									.min_w(px(180.))
									.overflow_hidden()
									.line_clamp(1)
									.text_ellipsis()
									.text_color(rgb(if resolved {
										pal().text_muted
									} else {
										pal().warning
									}))
									.tooltip(tip(target.clone()))
									.child(format!("→ {target}"))
									.children(probe(log, target_id)),
							)
							.children(probe(log, row_id))
					},
				))
				.children(probe(log, "paste-mappings"))
		});

		let is_diff = self.paste.detail().is_some_and(|p| p.is_diff);
		let side = self.reader.diff_mode == DiffMode::SideBySide;

		div()
			.id("paste-panel")
			.relative()
			.key_context("PastePanel")
			.track_focus(&self.paste_focus)
			.flex()
			.flex_col()
			.flex_1()
			.min_w_0()
			.min_h_0()
			.h_full()
			// Never paint over the bottom tool window when space runs out.
			.overflow_hidden()
			.bg(rgb(pal().editor_bg))
			.rounded(px(ISLAND_RADIUS))
			.child(tab)
			.child(action_bar)
			.child(dest_bar)
			.child(summary)
			.when(loading, |d| d.child(self.paste_loading_note()))
			.when(plan.whole_commit, |d| {
				d.child(
					div()
						.id("paste-commit-whole")
						.relative()
						.flex_shrink_0()
						.px(px(12.))
						.py(px(4.))
						.text_size(px(SMALL_TEXT))
						.text_color(rgb(pal().text_muted))
						.child(t("commit_whole_note", loc))
						.children(probe(log, "paste-commit-whole")),
				)
			})
			.when_some(plan.error.clone(), |d, err| {
				d.child(
					div()
						.flex_shrink_0()
						.px(px(12.))
						.py(px(4.))
						.bg(rgb(pal().error_bg))
						.text_size(px(SMALL_TEXT))
						.text_color(rgb(pal().error))
						.child(err.render(loc)),
				)
			})
			// Mappings and rows share one scroll region: it takes its content
			// height up to 60% of the panel and shrinks first when the window
			// is small, so the action bar above stays pinned and every row is
			// reachable by scrolling.
			.child(
				div()
					.id("paste-items")
					.flex()
					.flex_col()
					.min_h(px(0.))
					.max_h(gpui::relative(0.6))
					.overflow_y_scroll()
					.border_b_1()
					.border_color(rgb(pal().divider))
					.children(mappings)
					.children(rows)
					.children(probe(log, "paste-items")),
			)
			.when(selected.is_some(), |d| {
				d.child(
					div()
						.flex()
						.flex_row()
						.items_center()
						.flex_shrink_0()
						.h(px(24.))
						.px(px(12.))
						.gap(px(8.))
						.bg(rgb(pal().panel_bg))
						.text_size(px(SMALL_TEXT))
						.child(fill_text(detail_title))
						.when(is_diff, |h| {
							h.child(
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
								.debug_selector(|| "btn-diff-mode".into())
								.on_click(cx.listener(|this, _, _, cx| {
									this.toggle_diff_mode(cx)
								}))
								.children(probe(log, "btn-diff-mode")),
							)
						})
						.child(
							div()
								.flex_shrink_0()
								.text_color(rgb(pal().text_muted))
								.child(reason),
						),
				)
			})
			.when_some(
				selected.and_then(|i| i.commit).and_then(|c| {
					plan.commit_preview.as_ref()?.plan().commits.get(c)
				}),
				|d, commit| {
					let (subject, author, date) =
						commit_header_labels(commit, loc);
					let metadata = format!("{subject} · {author} · {date}");
					d.child(
						div()
							.id("paste-commit-metadata")
							.debug_selector(|| "paste-commit-metadata".into())
							.relative()
							.flex_shrink_0()
							.min_w_0()
							.px(px(12.))
							.py(px(4.))
							.text_size(px(SMALL_TEXT))
							.text_color(rgb(pal().text_muted))
							.tooltip(tip(metadata.clone()))
							.child(clip_text(metadata))
							.children(probe(log, "paste-commit-metadata")),
					)
				},
			)
			.when_some(
				// Worded now, in the language in force now.
				self.paste
					.detail()
					.and_then(|p| p.notice.as_ref())
					.map(|n| n.render(loc)),
				|d, notice| {
					d.child(
						div()
							.id("paste-diff-notice")
							.relative()
							.children(probe(log, "paste-diff-notice"))
							.flex_shrink_0()
							.px(px(12.))
							.py(px(2.))
							.bg(rgb(pal().panel_bg))
							.text_size(px(SMALL_TEXT))
							.text_color(rgb(pal().warning))
							.child(notice),
					)
				},
			)
			.when(selected.is_some_and(|i| i.shows_delete_notice()), |d| {
				d.child(
					div()
						.relative()
						.p(px(12.))
						.text_color(rgb(pal().error))
						.child(t("reason_delete", loc))
						.children(probe(log, "paste-delete-notice")),
				)
			})
			.when(
				selected.is_some_and(|i| i.commit.is_some() || !i.is_delete()),
				|d| d.child(self.render_code_view(true, cx)),
			)
			.children(probe(log, "paste-panel"))
			.into_any_element()
	}
}

/// Where the Project selection says a paste should go.
impl WorkbenchModel {
	/// The workspace-relative folder of the single selected Project row: the
	/// folder itself, or a file's folder. Nothing for no selection, for
	/// several (one cannot be guessed), for the commit tree browser, or for
	/// a row that is no real entry.
	fn paste_left_folder(&self) -> Option<String> {
		if self.rev_tree.is_some()
			|| self.selected_count() != 1
			|| self.project_targets().len() != 1
		{
			return None;
		}
		let (row, ws) =
			self.project_rows().into_iter().find_map(|r| match r {
				ProjRow::Ws(row) if row.selected => Some((row, true)),
				ProjRow::Work(row) if row.selected => Some((row, false)),
				_ => None,
			})?;
		if !row.is_valid_utf8
			|| row.is_error
			|| row.is_loading
			|| row.is_truncation_marker
			|| row.is_more_marker
			|| row.is_view_limit
		{
			return None;
		}
		let root = if ws { self.ws_root() } else { self.repo_root() }?;
		let rel = row.rel_path.trim_end_matches('/');
		let folder = if row.is_dir {
			rel
		} else {
			rel.rsplit_once('/').map_or("", |(dir, _)| dir)
		};
		let abs = folder
			.split('/')
			.filter(|part| !part.is_empty())
			.fold(root, |path, part| path.join(part));
		crate::paste::dest::folder_rel(
			&self.workspace_root,
			&abs,
			self.remote.session.is_some(),
		)
	}

	/// The workspace's name for the breadcrumb's first segment.
	fn paste_workspace_name(&self) -> String {
		match self.remote.session.as_ref() {
			Some(session) => {
				let last = session
					.workspace
					.path
					.trim_end_matches(['/', '\\'])
					.rsplit(['/', '\\'])
					.next()
					.unwrap_or_default();
				if last.is_empty() {
					crate::remote::display_path(Some(session), &session.root)
				} else {
					last.to_string()
				}
			}
			None => self
				.workspace_root
				.file_name()
				.map(|n| n.to_string_lossy().into_owned())
				.unwrap_or_else(|| self.workspace_root.display().to_string()),
		}
	}

	/// The destination bar under Apply and Cancel: where the paste lands as
	/// a breadcrumb, the places to pick it from, and a typed path. It is
	/// part of the panel's own layout, never an overlay.
	pub(super) fn paste_dest_bar(
		&self,
		cx: &mut Context<Self>,
	) -> Stateful<Div> {
		let loc = self.locale;
		let log = &self.probes;
		let plan = self.paste.plan();
		let loading = self.paste.is_loading();
		let enabled =
			plan.is_some_and(|p| !p.is_applying) && self.accepting_work();
		let editing = self.paste.dest_edit();
		let remote = self.remote.session.is_some();
		let folder = self.paste.folder();
		let current_rel = folder.and_then(|f| f.rel.clone());
		let session = self.remote.session.as_ref();

		// The breadcrumb: the workspace, then each folder down to the one
		// the paste lands in. Each is a place to go back to.
		let crumbs = folder.map(|folder| match &folder.rel {
			Some(rel) => {
				let mut names = vec![self.paste_workspace_name()];
				names.extend(
					rel.split('/')
						.filter(|part| !part.is_empty())
						.map(str::to_string),
				);
				let last = names.len() - 1;
				let mut row = div()
					.flex()
					.flex_row()
					.items_center()
					.gap(px(2.))
					.flex_shrink()
					.min_w_0();
				for (i, name) in names.into_iter().enumerate() {
					let upto: String = rel
						.split('/')
						.filter(|part| !part.is_empty())
						.take(i)
						.collect::<Vec<_>>()
						.join("/");
					let path = upto
						.split('/')
						.filter(|part| !part.is_empty())
						.fold(self.workspace_root.clone(), |p, part| {
							p.join(part)
						});
					let full = crate::remote::display_path(session, &path);
					let id = format!("paste-dest-crumb:{i}");
					let goes = enabled && i != last;
					if i > 0 {
						row = row.child(icon(Icon::ChevronRight, 10.));
					}
					row = row.child(
						div()
							.id(SharedString::from(id.clone()))
							.relative()
							.flex_shrink()
							.min_w_0()
							.px(px(4.))
							.rounded(px(3.))
							.border_1()
							.border_color(transparent_black())
							.when(i == last, |d| {
								d.font_weight(FontWeight::SEMIBOLD)
							})
							.when(goes, |d| {
								d.cursor_pointer()
									.tab_index(52)
									.hover(|s| s.bg(rgb(pal().hover_bg)))
									.on_click(cx.listener(
										move |this, _, _, cx| {
											this.paste_into_rel(
												&upto, "crumb", cx,
											)
										},
									))
							})
							.map(focus_ring)
							.tooltip(tip(full))
							.child(clip_text(name))
							.children(probe(log, id)),
					);
				}
				row.into_any_element()
			}
			// Outside the workspace (an enclosing repo, a `--restore-dir`
			// elsewhere): one plain label, nothing to go back to.
			None => {
				let full = crate::remote::display_path(session, &folder.path);
				div()
					.id("paste-dest-path")
					.relative()
					.flex_shrink()
					.min_w_0()
					.tooltip(tip(full.clone()))
					.child(clip_text(full))
					.children(probe(log, "paste-dest-path"))
					.into_any_element()
			}
		});

		let chip = |id: &'static str,
		            label: &'static str,
		            on: bool,
		            tab: isize,
		            rel: Option<String>,
		            via: &'static str,
		            cx: &mut Context<Self>| {
			button(
				id,
				t(label, loc),
				if on { Btn::Primary } else { Btn::Default },
				enabled,
				tab,
			)
			.when(enabled, |b| {
				b.on_click(cx.listener(move |this, _, _, cx| {
					this.paste_into_rel(rel.as_deref().unwrap_or(""), via, cx)
				}))
			})
			.children(probe(log, id))
		};
		let at = |rel: &str| current_rel.as_deref() == Some(rel);

		// The pinned repo, when it is a folder of the workspace of its own.
		let repo_rel = self
			.repo_root()
			.and_then(|root| {
				crate::paste::dest::folder_rel(
					&self.workspace_root,
					&root,
					remote,
				)
			})
			.filter(|rel| !rel.is_empty());
		let left_rel = self.paste_left_folder();

		let status = if let Some(err) = editing
			.and_then(|e| e.error.as_ref())
			.or_else(|| self.paste.dest_refusal())
		{
			Some((false, err.render(loc)))
		} else if let Some(plan) = plan.filter(|_| !loading) {
			if plan.executable() {
				Some((true, t("paste_dest_ok", loc).to_string()))
			} else {
				plan.error
					.as_ref()
					.filter(|_| plan.mapping_ready())
					.map(|err| (false, err.render(loc)))
			}
		} else {
			None
		};

		div()
			.id("paste-dest-bar")
			.relative()
			.flex()
			.flex_row()
			.flex_wrap()
			.items_center()
			.flex_shrink_0()
			.gap_x(px(8.))
			.gap_y(px(4.))
			.px(px(12.))
			.py(px(4.))
			.text_size(px(SMALL_TEXT))
			.bg(rgb(pal().panel_bg))
			.border_b_1()
			.border_color(rgb(pal().border))
			.child(
				div()
					.flex_shrink_0()
					.text_color(rgb(pal().text_muted))
					.child(format!("{}:", t("destination_label", loc))),
			)
			.child(if editing.is_some() {
				div()
					.id("paste-dest-input")
					.relative()
					.flex_1()
					.min_w(px(160.))
					.child(self.paste_dest_input.clone())
					.children(probe(log, "paste-dest-input"))
					.into_any_element()
			} else {
				crumbs.unwrap_or_else(|| div().into_any_element())
			})
			.child(chip(
				"paste-dest-root",
				"paste_dest_root",
				at(""),
				53,
				Some(String::new()),
				"root",
				cx,
			))
			.children(repo_rel.map(|rel| {
				chip(
					"paste-dest-repo",
					"paste_dest_repo",
					at(&rel),
					54,
					Some(rel.clone()),
					"repo",
					cx,
				)
			}))
			.children(left_rel.map(|rel| {
				chip(
					"paste-dest-left",
					"paste_dest_left",
					at(&rel),
					55,
					Some(rel.clone()),
					"left",
					cx,
				)
			}))
			.child(
				button(
					"paste-dest-edit",
					t("paste_dest_edit", loc),
					Btn::Default,
					enabled,
					56,
				)
				.when(enabled, |b| {
					b.on_click(cx.listener(|this, _, _, cx| {
						this.open_paste_dest_edit(cx)
					}))
				})
				.children(probe(log, "paste-dest-edit")),
			)
			.children((!remote).then(|| {
				button(
					"paste-dest-browse",
					t("paste_dest_browse", loc),
					Btn::Default,
					enabled,
					57,
				)
				.when(enabled, |b| {
					b.on_click(
						cx.listener(|this, _, _, cx| {
							this.browse_paste_dest(cx)
						}),
					)
				})
				.children(probe(log, "paste-dest-browse"))
			}))
			.children(status.map(|(ok, text)| {
				div()
					.id("paste-dest-status")
					.relative()
					.flex_shrink()
					.min_w_0()
					.max_w(px(360.))
					.text_color(rgb(if ok {
						pal().git_added
					} else {
						pal().error
					}))
					.child(clip_text(format!(
						"{} {text}",
						if ok { "✓" } else { "✕" }
					)))
					.children(probe(log, "paste-dest-status"))
			}))
			.children(probe(log, "paste-dest-bar"))
	}
}
