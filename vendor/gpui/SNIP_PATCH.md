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

`src/gpui.rs`: `#![allow(warnings)]`. As a path dependency the crate is built
without `--cap-lints`, so the workspace's `RUSTFLAGS="-D warnings"` would turn
upstream warnings (e.g. `float_literal_f32_fallback` in `taffy.rs`) into
errors on every CI platform. The in-source allow restores registry behaviour.

## Upgrading gpui

Re-copy the new version's crate, re-apply the patch above, and rerun
`scripts/check_native_ime.py`. Drop the vendor copy once upstream resets the
IC without relying on `composing`.
