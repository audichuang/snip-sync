//! Paste preview in the editor area.

use super::changes::{change_pad, tree_chevron};
use super::*;
use crate::paste::{
	commit_header_id, dir_control_id, DirControl, DirKey, DirSummary,
	PasteCounts, RowControl, Tick,
};

/// The restore-base hint's sentence, worded from the suggestion's data in
/// the interface language. Never `RestoreBaseSuggestion::label`: that is
/// core's English, written for the CLI.
fn restore_hint_text(
	hint: &snip_core::restore::RestoreBaseSuggestion,
	loc: crate::i18n::Locale,
) -> String {
	match &hint.base {
		snip_core::restore::RestoreBase::Strip { segment } => {
			tf("paste_adjust_strip", loc, &[segment])
		}
		snip_core::restore::RestoreBase::Add { prefix } => {
			tf("paste_adjust_add", loc, &[prefix])
		}
	}
}

impl WorkbenchModel {
	// ───────────────────────── paste ─────────────────────────

	/// The hint row's text for the open paste plan, if the row is shown.
	/// Read again by the row's tooltip so it follows the interface language
	/// (#148).
	pub(crate) fn paste_hint_text(&self) -> Option<String> {
		let hint = self.paste.plan()?.restore_hint()?;
		Some(restore_hint_text(hint, self.locale))
	}

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

	/// Destination, Apply and Cancel. Apply stays locked while a read-only
	/// preview or remap is still running, so an older plan cannot be written.
	pub(super) fn paste_action_bar(
		&self,
		dest: String,
		applying: bool,
		mapping_ready: bool,
		executable: bool,
		cx: &mut Context<Self>,
	) -> Div {
		let loc = self.locale;
		let log = &self.probes;
		let loading = self.paste.is_loading();
		let can_apply = !applying && executable && !loading;
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
			.child(
				div()
					.id("paste-dest")
					.flex()
					.flex_row()
					.items_center()
					.gap(px(6.))
					.flex_1()
					.min_w_0()
					.tooltip(tip(dest.clone()))
					.child(
						div()
							.flex_shrink_0()
							.text_color(rgb(pal().text_muted))
							.child(format!("{}:", t("destination_label", loc))),
					)
					.child(fill_text(dest)),
			)
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
					.when(!loading && !mapping_ready, |b| {
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
		let dest = crate::remote::display_path(
			self.remote.session.as_ref(),
			&self.current_restore_destination(),
		);
		div()
			.id("paste-panel")
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
			.child(self.paste_action_bar(dest, false, false, false, cx))
			.child(self.paste_loading_note())
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

	/// A destination root's or a folder's row: chevron, a three-state
	/// include box, the folder icon, its name and how many of its rows are
	/// written. A click selects it, the chevron folds it.
	#[allow(clippy::too_many_arguments)]
	fn paste_dir_row(
		&self,
		plan: &PastePreviewPlan,
		key: DirKey,
		ord: usize,
		name: String,
		sum: DirSummary,
		collapsed: bool,
		root: bool,
		row: Div,
		cx: &Context<Self>,
	) -> AnyElement {
		let loc = self.locale;
		let log = &self.probes;
		let applying = plan.is_applying;
		let rel = key.path.clone();
		let row_id = dir_control_id(DirControl::Row, ord, &rel);
		let toggle_id = dir_control_id(DirControl::Toggle, ord, &rel);
		let inc_id = dir_control_id(DirControl::Include, ord, &rel);
		let tick = sum.tick();
		let on_cursor = plan.cursor_dir.as_ref() == Some(&key);
		// A replay's folders fold like its headers: a click anywhere on the
		// row, and its include box is locked (all or nothing is written).
		let by_commit = plan.whole_commit;
		let written = if sum.included == sum.files {
			sum.files.to_string()
		} else {
			format!("{}/{}", sum.included, sum.files)
		};
		let (on_row, on_fold, on_box) = (key.clone(), key.clone(), key);
		row.id(SharedString::from(row_id.clone()))
			.debug_selector(|| row_id.clone())
			.relative()
			.cursor_pointer()
			.when(on_cursor, |d| d.bg(rgb(pal().selection_bg)))
			.when(!on_cursor, |d| d.hover(|s| s.bg(rgb(pal().hover_bg))))
			.on_click(cx.listener(move |this, _, _, cx| {
				if by_commit {
					this.toggle_paste_dir(on_row.clone(), cx)
				} else {
					this.select_paste_dir(on_row.clone(), cx)
				}
			}))
			.child(
				tree_chevron(toggle_id.clone(), collapsed)
					.debug_selector(|| toggle_id.clone())
					.cursor_pointer()
					.on_click(cx.listener(move |this, _, _, cx| {
						cx.stop_propagation();
						this.toggle_paste_dir(on_fold.clone(), cx);
					}))
					.children(probe(log, toggle_id)),
			)
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
					.when(tick == Tick::Locked, |d| d.opacity(0.4))
					// A locked box does not fold the row it sits on.
					.when(by_commit, |d| {
						d.on_click(|_, _, cx| cx.stop_propagation())
					})
					.when(by_commit && !applying, |d| {
						d.tooltip(tip(t("commit_subset_rejected", loc)))
					})
					// Still routed to the model while applying so the refusal
					// is explicit, like a file's box.
					.when(tick != Tick::Locked, |d| {
						d.on_click(cx.listener(move |this, _, _, cx| {
							cx.stop_propagation();
							this.toggle_paste_dir_selected(on_box.clone(), cx);
						}))
					})
					.child(checkbox_tri(tick))
					.children(probe(log, inc_id)),
			)
			.child(icon(Icon::Folder, 14.))
			.child(
				clip_text(name)
					.flex_shrink()
					.when(root, |d| d.font_weight(FontWeight::SEMIBOLD)),
			)
			.child(
				div()
					.flex_shrink_0()
					.text_size(px(SMALL_TEXT))
					.text_color(rgb(pal().text_muted))
					.child(written),
			)
			.children(probe(log, row_id))
			.into_any_element()
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
		let dest = plan.shown(&plan.destination);
		let applying = plan.is_applying;
		let mapping_ready = plan.mapping_ready();
		// A cursor on a folder row has no file to show.
		let selected = plan
			.cursor_dir
			.is_none()
			.then(|| plan.items.get(plan.selected_item_idx))
			.flatten();
		let folder_detail = plan.detail_dir().map(|key| {
			let title = if key.path.is_empty() {
				plan.shown(&key.root)
			} else {
				key.path.clone()
			};
			(title, plan.counts_under(key))
		});

		let loading = self.paste.is_loading();
		let action_bar = self.paste_action_bar(
			dest,
			applying,
			mapping_ready,
			plan.executable(),
			cx,
		);
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
				.pl(px(change_pad(depth)))
				.pr(px(8.))
				.gap(px(6.))
		};
		let nodes = if plan.whole_commit {
			plan.commit_rows()
		} else {
			plan.tree_rows()
		};
		// Roots that share a name tell themselves apart by their folders.
		let root_labels: Vec<String> = {
			let labels: Vec<&str> = nodes
				.iter()
				.filter_map(|n| match n {
					PasteNode::Root { label, .. } => Some(label.as_str()),
					_ => None,
				})
				.collect();
			let repeated = labels
				.iter()
				.enumerate()
				.any(|(i, l)| labels[..i].contains(l));
			if repeated {
				let shown: Vec<PathBuf> = nodes
					.iter()
					.filter_map(|n| match n {
						PasteNode::Root { key, .. } => {
							Some(PathBuf::from(plan.shown(&key.root)))
						}
						_ => None,
					})
					.collect();
				Self::disambiguate_candidate_labels(&shown)
			} else {
				labels.into_iter().map(str::to_string).collect()
			}
		};
		let rows = nodes.into_iter().map(|node| {
			let (ix, depth) = match node {
				PasteNode::Commit(c) => {
					return Self::commit_header(plan, c, loc, log, cx)
						.into_any_element();
				}
				PasteNode::Root {
					key,
					ord,
					label,
					sum,
					collapsed,
				} => {
					let label = root_labels.get(ord).cloned().unwrap_or(label);
					return self.paste_dir_row(
						plan,
						key,
						ord,
						label,
						sum,
						collapsed,
						true,
						node_row(0),
						cx,
					);
				}
				PasteNode::Dir {
					key,
					ord,
					name,
					depth,
					sum,
					collapsed,
				} => {
					return self.paste_dir_row(
						plan,
						key,
						ord,
						name,
						sum,
						collapsed,
						false,
						node_row(depth),
						cx,
					);
				}
				PasteNode::File(ix, depth) => (ix, depth),
			};
			let item = &plan.items[ix];
			let (_, name_color, _) = paste_op(item);
			let is_sel =
				plan.cursor_dir.is_none() && ix == plan.selected_item_idx;
			let path = item.path.clone();
			let row_id = RowControl::Row.id(ix, &path);
			let inc_id = RowControl::Include.id(ix, &path);
			let ow_id = RowControl::Overwrite.id(ix, &path);
			let can_overwrite = item.overwritable();
			let planned_skip = item.planned_skip();
			let by_commit = item.commit.is_some();
			let ow_on = item.overwrite_allowed;
			let (_, base) = split_dir(&path);
			// A skipped row only has its raw path, which can be anything. A
			// rename's deleted old path stands under the new path's folder,
			// so it names its own.
			let name = if planned_skip || item.is_rename_source() {
				path.as_str()
			} else {
				split_dir(plan.place_path(ix)).1
			};
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
				// Under a folder, the file lines up with its siblings'
				// checkboxes, past the folders' chevron column.
				.child(div().flex_shrink_0().w(px(16.)))
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

		// The restore-base hint: a non-modal row, never applied unasked.
		let model = cx.weak_entity();
		let restore_hint = plan.restore_hint().map(|hint| {
			let text = restore_hint_text(hint, loc);
			let adjusted = plan.adjust_paths;
			let pick = |id: &'static str, label: &str, on: bool, tab: isize| {
				button(
					id,
					label.to_string(),
					if adjusted == on {
						Btn::Primary
					} else {
						Btn::Default
					},
					!applying,
					tab,
				)
				.debug_selector(|| id.to_string())
				.when(!applying, |b| {
					b.on_click(cx.listener(move |this, _, _, cx| {
						// The choice already in force needs no re-plan.
						let current = this
							.paste
							.plan()
							.is_some_and(|p| p.adjust_paths == on);
						if !current {
							this.choose_paste_adjust(on, cx);
						}
					}))
				})
				.children(probe(log, id))
			};
			div()
				.id("paste-restore-base")
				.debug_selector(|| "paste-restore-base".into())
				.relative()
				.flex()
				.flex_row()
				.flex_wrap()
				.items_center()
				.flex_shrink_0()
				.gap_x(px(6.))
				.gap_y(px(4.))
				.mx(px(8.))
				.my(px(6.))
				.px(px(8.))
				.py(px(5.))
				.rounded(px(4.))
				.border_1()
				.border_color(rgb(pal().divider))
				.bg(rgb(pal().panel_bg))
				.text_size(px(SMALL_TEXT))
				.child(
					clip_text(text.clone())
						.id("paste-restore-base-text")
						.flex_1()
						.min_w(px(180.))
						.tooltip(live_tip(move |cx| {
							model
								.upgrade()
								.and_then(|m| m.read(cx).paste_hint_text())
								.unwrap_or_default()
								.into()
						})),
				)
				.child(pick("paste-adjust", t("paste_adjust", loc), true, 52))
				.child(pick("paste-as-is", t("paste_as_is", loc), false, 53))
				.children(probe(log, "paste-restore-base"))
		});

		let is_diff = self.paste.detail().is_some_and(|p| p.is_diff);
		let side = self.reader.diff_mode == DiffMode::SideBySide;

		div()
			.id("paste-panel")
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
			.children(restore_hint)
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
			.when_some(folder_detail, |d, (title, counts)| {
				let PasteCounts {
					creates,
					overwrites,
					existing,
					deletes,
					skips,
				} = counts;
				d.child(
					div()
						.id("paste-dir-detail")
						.debug_selector(|| "paste-dir-detail".to_string())
						.relative()
						.flex()
						.flex_row()
						.items_center()
						.flex_shrink_0()
						.h(px(24.))
						.px(px(12.))
						.gap(px(12.))
						.bg(rgb(pal().panel_bg))
						.text_size(px(SMALL_TEXT))
						.child(fill_text(title))
						.child(
							div()
								.flex_shrink_0()
								.text_color(rgb(pal().git_added))
								.child(format!(
									"{} {creates}",
									t("op_create", loc)
								)),
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
								.child(format!(
									"{} {deletes}",
									t("op_delete", loc)
								)),
						)
						.child(
							div()
								.flex_shrink_0()
								.text_color(rgb(pal().text_muted))
								.child(format!(
									"{} {skips}",
									t("op_skip", loc)
								)),
						)
						.children(probe(log, "paste-dir-detail")),
				)
			})
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
			.into_any_element()
	}
}
