//! The `[APP:…]` lines of the probes (`CTRL_BOUNDS`, `CTRL_GONE`,
//! `CTRL_DUPLICATE`, `CTRL_COVERED`, `TEXT_FIT_GONE`; `ui/fit.rs` builds the
//! `TEXT_FIT` family the same way). One place spells them, so the producers
//! and the tests of the parsers read the same text.

/// `[APP:CTRL_BOUNDS: id=<id> x=<n> y=<n> w=<n> h=<n>]`
pub(crate) fn bounds(id: &str, v: [i32; 4]) -> String {
	format!(
		"[APP:CTRL_BOUNDS: id={id} x={} y={} w={} h={}]",
		v[0], v[1], v[2], v[3]
	)
}

/// `[APP:CTRL_GONE: id=<id>]`
pub(crate) fn gone(id: &str) -> String {
	format!("[APP:CTRL_GONE: id={id}]")
}

/// `[APP:CTRL_DUPLICATE: id=<id>]`
pub(crate) fn duplicate(id: &str) -> String {
	format!("[APP:CTRL_DUPLICATE: id={id}]")
}

/// `[APP:CTRL_COVERED: id=<id> by=<by>]`
pub(crate) fn covered(id: &str, by: &str) -> String {
	format!("[APP:CTRL_COVERED: id={id} by={by}]")
}

/// `[APP:TEXT_FIT_GONE: id=<id>]`
pub(crate) fn text_fit_gone(id: &str) -> String {
	format!("[APP:TEXT_FIT_GONE: id={id}]")
}
