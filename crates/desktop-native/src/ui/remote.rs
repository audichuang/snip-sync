//! Workspace menu: the remote section (the hosts of `~/.ssh/config`, a
//! folder browser on the one being browsed, and a path field).

use super::*;

impl WorkbenchModel {
	pub(super) fn render_remote_section(
		&self,
		cx: &mut Context<Self>,
	) -> Vec<AnyElement> {
		let loc = self.locale;
		let log = &self.probes;
		let muted = |text: String| {
			div()
				.px(px(8.))
				.py(px(2.))
				.text_size(px(SMALL_TEXT))
				.text_color(rgb(pal().text_muted))
				.child(text)
		};
		let mut out: Vec<AnyElement> = vec![muted(
			t("remote_section", loc).to_string(),
		)
		.into_any_element()];
		if self.remote.hosts.is_empty() {
			out.push(
				muted(t("remote_no_hosts", loc).to_string()).into_any_element(),
			);
		}

		let browse = self.remote.browse.as_ref();
		for (ix, host) in self.remote.hosts.iter().enumerate() {
			let id = format!("remote-host:{ix}");
			let open = browse.is_some_and(|b| b.host == ix);
			out.push(
				menu_row(SharedString::from(id.clone()), 82)
					.tooltip(tip(host.name.clone()))
					.when(open, |d| d.bg(rgb(pal().hover_bg)))
					.on_click(cx.listener(move |this, _, _, cx| {
						this.browse_remote_host(ix, cx);
					}))
					.child(div().flex_shrink_0().child(icon(Icon::Folder, 16.)))
					.child(
						div()
							.flex_1()
							.min_w_0()
							.font_weight(FontWeight::SEMIBOLD)
							.child(clip_text(host.name.clone())),
					)
					.children(probe(log, id))
					.into_any_element(),
			);
			let Some(browse) = browse.filter(|b| b.host == ix) else {
				continue;
			};
			let busy = self.remote.busy;
			// The folder shown, and the button that opens it.
			let listed = matches!(browse.listing, Some(Ok(_)));
			out.push(
				div()
					.flex()
					.flex_row()
					.items_center()
					.gap(px(6.))
					.pl(px(32.))
					.pr(px(8.))
					.py(px(2.))
					.child(
						div()
							.id("remote-path")
							.flex_1()
							.min_w_0()
							.text_size(px(SMALL_TEXT))
							.text_color(rgb(pal().text_muted))
							.tooltip(tip(browse.path.clone()))
							.child(clip_text(browse.path.clone()))
							.children(probe(log, "remote-path")),
					)
					.child(
						button(
							"btn-remote-open-here",
							t("remote_open_here", loc),
							Btn::Primary,
							listed && !busy,
							83,
						)
						.when(listed && !busy, |d| {
							d.on_click(cx.listener(|this, _, _, cx| {
								this.open_remote_here(cx);
							}))
						})
						.children(probe(log, "btn-remote-open-here")),
					)
					.into_any_element(),
			);
			if !crate::remote::is_root(&browse.path) {
				out.push(
					menu_row("remote-up", 84)
						.pl(px(32.))
						.on_click(cx.listener(|this, _, _, cx| {
							this.remote_up(cx);
						}))
						.child(
							div()
								.flex_shrink_0()
								.child(icon(Icon::Folder, 14.)),
						)
						.child(fill_text(t("remote_up", loc)))
						.children(probe(log, "remote-up"))
						.into_any_element(),
				);
			}
			match &browse.listing {
				None => out.push(
					muted(t("remote_loading", loc).to_string())
						.pl(px(32.))
						.into_any_element(),
				),
				Some(Err(err)) => out.push(
					muted(err.clone())
						.pl(px(32.))
						.text_color(rgb(pal().error))
						.into_any_element(),
				),
				Some(Ok(listing)) if listing.folders.is_empty() => out.push(
					muted(t("remote_no_subfolders", loc).to_string())
						.pl(px(32.))
						.into_any_element(),
				),
				Some(Ok(listing)) => {
					for (fx, name) in listing.folders.iter().enumerate() {
						let id = format!("remote-folder:{fx}");
						let tip_path =
							crate::remote::child_path(&listing.path, name);
						let enter = name.clone();
						out.push(
							menu_row(SharedString::from(id.clone()), 84)
								.pl(px(32.))
								.tooltip(tip(tip_path))
								.on_click(cx.listener(move |this, _, _, cx| {
									this.enter_remote_folder(&enter, cx);
								}))
								.child(
									div()
										.flex_shrink_0()
										.child(icon(Icon::Folder, 14.)),
								)
								.child(fill_text(name.clone()))
								.children(probe(log, id))
								.into_any_element(),
						);
					}
				}
			}
			out.push(
				div()
					.flex()
					.flex_row()
					.gap(px(6.))
					.pl(px(32.))
					.pr(px(8.))
					.py(px(4.))
					.child(
						div()
							.id("remote-path-input")
							.relative()
							.flex_1()
							.min_w_0()
							.child(self.remote_path_input.clone())
							.children(probe(log, "remote-path-input")),
					)
					.child(
						button(
							"btn-remote-open",
							t(
								if busy {
									"remote_opening"
								} else {
									"remote_open_path"
								},
								loc,
							),
							Btn::Default,
							!busy,
							86,
						)
						.when(!busy, |d| {
							d.on_click(cx.listener(|this, _, _, cx| {
								this.open_remote_typed(cx);
							}))
						})
						.children(probe(log, "btn-remote-open")),
					)
					.into_any_element(),
			);
		}
		if let Some((ok, text)) = &self.remote.message {
			out.push(
				muted(text.clone())
					.when(!ok, |d| d.text_color(rgb(pal().error)))
					.into_any_element(),
			);
		}
		out
	}
}
