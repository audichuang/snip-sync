# Vendored gpui 0.2.2

Unmodified crates.io `gpui` 0.2.2 except for the lines below. `examples/` and
`Cargo.lock` are dropped and the `[[example]]` targets removed from
`Cargo.toml`; nothing else in the manifest changed.

## Patch

`src/platform/linux/x11/client.rs`, `Event::XinputButtonPress`: reset the XIM
input context on every click while an XIM connection exists, not only when
`state.composing` is set.

Why: Fcitx 5 defaults to off-the-spot XIM (`UseOnTheSpot=false`). In that
style no preedit callbacks arrive, so GPUI never sets `composing`, never
resets the IC on click, and the candidate window stays up after focus moves
to another control. Zed's upstream `gpui_linux` has the same condition.
`scripts/check_native_ime.py` phase `focus-switch` is the acceptance test.

The same file's `enable_ime`, `reset_ime`, and `update_ime_position` wait for
`XimHandler.connected` before sending IC requests. The handler sets this only
after the initial `CREATE_IC_REPLY`; its handshake still runs while disconnected.
Without these guards, clicking an input during startup can send
`SET_IC_VALUES(im=0, ic=0)`. Fcitx replies with an invalid XIM error (code 0),
the XIM parser rejects it, and GPUI drops the connection permanently. The race
was reproduced with a release binary and by withholding `CONNECT_REPLY` until
after a real input click. `scripts/check_native_ime_startup.py` repeats this
ordering, then runs the original IME acceptance, including focus-switch.

`src/platform/linux/x11/client.rs`, `process_x11_events`: when
`poll_for_event` fails with anything but a parse error, return the error
instead of logging and breaking. Upstream kept returning
`PostAction::Continue`, but xcb's connection error is sticky and the dead
socket stays readable, so calloop redispatched the source forever: after Xvfb
exited, the process spun its main thread at 100% CPU until killed (one orphan
ran over 10 minutes). The returned error ends `event_loop.run`, GPUI runs the
quit callbacks, and the process exits 0. Parse errors stay per-event warnings.
The lifecycle test `x_server_loss_exits_instead_of_spinning` kills a private
Xvfb after `[APP:WINDOW_READY]`: without the patch the app burned 5 s of CPU in
5 s and was still alive; with it, it exits 0 within about 50 ms.

`src/elements/text.rs`, `TextElement::request_layout`: truncate a copy of the
text runs on every measure. Upstream moved one `runs` vector into the measure
closure and let `truncate_line` shorten it in place. Taffy calls that closure
again when the available width changes (a window or panel resize), and the
second call laid out the full text with runs cut for the first width. On
macOS `layout_line` slices the text by those run lengths, so a long CJK label
cut with "…" aborted the app (`str::slice_error_fail`). This was the user's
crash on opening the Project view. `src/text_system.rs`, `shape_text`: a
`debug_assert_eq!` that the runs cover the text exactly, which makes the same
bug fail on Linux debug builds, where cosmic-text does not slice. The smoke
test `native_cjk_truncation_survives_resize` resizes a window showing
truncated CJK paths. It fails on the upstream code and passes with the patch.

`src/gpui.rs`: `#![allow(warnings)]`. As a path dependency the crate is built
without `--cap-lints`, so the workspace's `RUSTFLAGS="-D warnings"` would turn
upstream warnings (e.g. `float_literal_f32_fallback` in `taffy.rs`) into
errors on every CI platform. The in-source allow restores registry behaviour.

## Upgrading gpui

Re-copy the new version's crate, re-apply the patches above, and rerun
`scripts/check_native_ime.py` and `just native-lifecycle`. Drop the vendor
copy once upstream resets the IC without relying on `composing`, stops the
event loop when the X connection dies, and truncates a copy of the text runs.
