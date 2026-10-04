//! Workspace menu: the remote-node section (this machine's worker, paired
//! workers, their shared workspaces, and the pairing form).

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

		// This machine as a worker.
		let status = crate::remote::worker_status();
		out.push(
			menu_row("btn-remote-worker-toggle", 80)
				.on_click(cx.listener(|this, _, _, cx| {
					this.toggle_worker_mode(cx);
				}))
				.child(div().w(px(16.)))
				.child(fill_text(t(
					if status.is_some() {
						"remote_worker_stop"
					} else {
						"remote_worker_start"
					},
					loc,
				)))
				.children(probe(log, "btn-remote-worker-toggle"))
				.into_any_element(),
		);
		if let Some(status) = status {
			out.push(
				muted(tf(
					"remote_worker_listening",
					loc,
					&[
						status.addr.to_string(),
						status.fingerprint.clone(),
						status.shared.to_string(),
						status.masters.to_string(),
					],
				))
				.into_any_element(),
			);
			match status.code {
				Some((code, left)) => out.push(
					div()
						.id("remote-worker-code")
						.relative()
						.px(px(8.))
						.py(px(2.))
						.font_weight(FontWeight::SEMIBOLD)
						.child(tf(
							"remote_worker_code",
							loc,
							&[code, left.div_ceil(60).to_string()],
						))
						.children(probe(log, "remote-worker-code"))
						.into_any_element(),
				),
				None => out.push(
					menu_row("btn-remote-worker-pair", 81)
						.on_click(cx.listener(|this, _, _, cx| {
							this.open_worker_pairing(cx);
						}))
						.child(div().w(px(16.)))
						.child(fill_text(t("remote_worker_pair", loc)))
						.children(probe(log, "btn-remote-worker-pair"))
						.into_any_element(),
				),
			}
		}

		// Paired workers and, for the one being browsed, its workspaces.
		let browse = self.remote.browse.as_ref();
		for (ix, worker) in self.remote.workers.iter().enumerate() {
			let forget = format!("btn-remote-forget:{ix}");
			let tip_text = crate::remote::remote_worker_tip(worker);
			out.push(
				menu_row(SharedString::from(format!("remote-worker:{ix}")), 82)
					.tooltip(tip(tip_text))
					.when(browse.is_some_and(|(i, _)| *i == ix), |d| {
						d.bg(rgb(pal().hover_bg))
					})
					.on_click(cx.listener(move |this, _, _, cx| {
						this.browse_remote_worker(ix, cx);
					}))
					.child(div().flex_shrink_0().child(icon(Icon::Folder, 16.)))
					.child(
						div()
							.flex_1()
							.min_w_0()
							.flex()
							.flex_col()
							.child(
								div()
									.font_weight(FontWeight::SEMIBOLD)
									.child(clip_text(worker.name.clone())),
							)
							.child(
								div()
									.text_size(px(SMALL_TEXT))
									.text_color(rgb(pal().text_muted))
									.child(clip_text(format!(
										"{} · {}",
										worker.addr,
										snip_remote::Fingerprint::from_hex(
											&worker.fingerprint
										)
										.map(|f| f.short())
										.unwrap_or_else(|| worker
											.fingerprint
											.clone())
									))),
							),
					)
					.child(
						button(
							SharedString::from(forget.clone()),
							t("remote_forget", loc),
							Btn::Ghost,
							true,
							83,
						)
						.on_click(cx.listener(move |this, _, _, cx| {
							cx.stop_propagation();
							this.forget_remote_worker(ix, cx);
						}))
						.children(probe(log, forget)),
					)
					.children(probe(log, format!("remote-worker:{ix}")))
					.into_any_element(),
			);
			let Some((_, listing)) = browse.filter(|(i, _)| *i == ix) else {
				continue;
			};
			match listing {
				None => out.push(
					muted(t("remote_loading", loc).to_string())
						.into_any_element(),
				),
				Some(Err(err)) => out.push(
					muted(err.clone())
						.text_color(rgb(pal().error))
						.into_any_element(),
				),
				Some(Ok(items)) if items.is_empty() => out.push(
					muted(t("remote_no_workspaces", loc).to_string())
						.into_any_element(),
				),
				Some(Ok(items)) => {
					for (wx, ws) in items.iter().enumerate() {
						let id = format!("remote-workspace:{wx}");
						out.push(
							menu_row(SharedString::from(id.clone()), 84)
								.pl(px(32.))
								.tooltip(tip(ws.path.clone()))
								.on_click(cx.listener(move |this, _, _, cx| {
									this.open_remote_workspace(ix, wx, cx);
								}))
								.child(
									div()
										.flex_1()
										.min_w_0()
										.flex()
										.flex_col()
										.child(clip_text(ws.name.clone()))
										.child(
											div()
												.text_size(px(SMALL_TEXT))
												.text_color(rgb(
													pal().text_muted
												))
												.child(clip_text(
													ws.path.clone(),
												)),
										),
								)
								.children(probe(log, id))
								.into_any_element(),
						);
					}
				}
			}
		}

		// Pairing form.
		out.push(
			menu_row("btn-remote-pair-new", 85)
				.on_click(cx.listener(|this, _, _, cx| {
					this.show_remote_pairing(cx);
				}))
				.child(div().w(px(16.)))
				.child(fill_text(t("remote_pair_new", loc)))
				.children(probe(log, "btn-remote-pair-new"))
				.into_any_element(),
		);
		if self.remote.pairing {
			let busy = self.remote.busy;
			out.push(
				div()
					.flex()
					.flex_col()
					.gap(px(4.))
					.px(px(8.))
					.py(px(4.))
					.child(
						div()
							.id("remote-addr-input")
							.relative()
							.child(self.remote_addr_input.clone())
							.children(probe(log, "remote-addr-input")),
					)
					.child(
						div()
							.flex()
							.flex_row()
							.gap(px(6.))
							.child(
								div()
									.id("remote-code-input")
									.relative()
									.flex_1()
									.min_w_0()
									.child(self.remote_code_input.clone())
									.children(probe(log, "remote-code-input")),
							)
							.child(
								button(
									"btn-remote-pair",
									t(
										if busy {
											"remote_pairing"
										} else {
											"remote_pair_confirm"
										},
										loc,
									),
									Btn::Primary,
									!busy,
									86,
								)
								.when(!busy, |d| {
									d.on_click(cx.listener(|this, _, _, cx| {
										this.pair_remote_worker(cx);
									}))
								})
								.children(probe(log, "btn-remote-pair")),
							),
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
