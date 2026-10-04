//! Workspace menu: the remote section (the hosts of `~/.ssh/config`, the
//! folders of the one being browsed, its recent folders and a path field).

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
			let open = browse.is_some_and(|(i, _)| *i == ix);
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
			let Some((_, listing)) = browse.filter(|(i, _)| *i == ix) else {
				continue;
			};
			// Recent folders of this host first, then its home's folders.
			let recent: Vec<(usize, &crate::remote::RecentRemote)> = self
				.remote
				.recent
				.iter()
				.enumerate()
				.filter(|(_, r)| r.host == host.name)
				.collect();
			if !recent.is_empty() {
				out.push(
					muted(t("remote_recent", loc).to_string())
						.pl(px(32.))
						.into_any_element(),
				);
			}
			for (n, r) in recent {
				let id = format!("remote-recent:{n}");
				out.push(
					menu_row(SharedString::from(id.clone()), 84)
						.pl(px(32.))
						.tooltip(tip(r.path.clone()))
						.on_click(cx.listener(move |this, _, _, cx| {
							this.open_remote_recent(n, cx);
						}))
						.child(fill_text(r.path.clone()))
						.children(probe(log, id))
						.into_any_element(),
				);
			}
			match listing {
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
				Some(Ok(listing)) => {
					let base = listing.home.trim_end_matches(['/', '\\']);
					for (fx, name) in listing.folders.iter().enumerate() {
						let id = format!("remote-folder:{fx}");
						let path = format!("{base}/{name}");
						let tip_path = path.clone();
						out.push(
							menu_row(SharedString::from(id.clone()), 84)
								.pl(px(32.))
								.tooltip(tip(tip_path))
								.on_click(cx.listener(move |this, _, _, cx| {
									this.open_remote_path(ix, path.clone(), cx);
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
			let busy = self.remote.busy;
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
							Btn::Primary,
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
