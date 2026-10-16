//! The `[APP:…]` lines of the probes (`CTRL_BOUNDS`, `CTRL_GONE`,
//! `CTRL_DUPLICATE`, `CTRL_COVERED`, `TEXT_FIT_GONE`; `ui/fit.rs` builds the
//! `TEXT_FIT` family the same way). One place spells them, so the producers
//! and the tests of the parsers read the same text.
//!
//! An id can hold any character a repository, branch or file name can, so
//! `id=` is the last field of every such line and the id runs to the closing
//! `]`: `[APP:<KIND>: <fixed fields> id=<id>]`. A background tab's
//! `ws_tab=<n>` is the first field, right after `KIND: `
//! ([`crate::tabs::tag_line`]), so nothing is ever appended behind the id. See
//! `docs/real-ui-operator-protocol.md`, section 3.

/// `[APP:CTRL_BOUNDS: x=<n> y=<n> w=<n> h=<n> id=<id>]`
pub(crate) fn bounds(id: &str, v: [i32; 4]) -> String {
	format!(
		"[APP:CTRL_BOUNDS: x={} y={} w={} h={} id={id}]",
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

/// `[APP:CTRL_COVERED: by=<by, encoded> id=<id>]`: the control that covers
/// is a free-text id too, so it is written without a space, `]`, `%` or
/// line break (`%20`, `%5D`, `%25`, `%0A`, `%0D`) and the first ` id=` of the
/// line is the covered control's.
pub(crate) fn covered(id: &str, by: &str) -> String {
	format!("[APP:CTRL_COVERED: by={} id={id}]", encode_by(by))
}

fn encode_by(by: &str) -> String {
	let mut out = String::with_capacity(by.len());
	for c in by.chars() {
		match c {
			'%' => out.push_str("%25"),
			' ' => out.push_str("%20"),
			']' => out.push_str("%5D"),
			'\n' => out.push_str("%0A"),
			'\r' => out.push_str("%0D"),
			c => out.push(c),
		}
	}
	out
}

/// `[APP:TEXT_FIT_GONE: id=<id>]`
pub(crate) fn text_fit_gone(id: &str) -> String {
	format!("[APP:TEXT_FIT_GONE: id={id}]")
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::tabs::{set_active_tab, tag_line, with_log_tab};
	use snip_native_e2e as e2e;

	/// Ids and covering names a repository, branch or file can give.
	const ODD: [&str; 9] = [
		"commit-row:my repo:ab12cd3",
		"change-row:report ws_tab=2",
		"change-row:a rule=whole by=b fit=1 drawn=2",
		"commit-row:x x=1 y=2 w=3 h=4",
		"row:ends in by=",
		"row:ends in id=",
		"row:has id=inside it",
		"row:odd ] name 100%",
		"row: lead and trail ",
	];

	/// What a driver reads out of `line`, printed by the shown tab (`None`)
	/// or by a background tab.
	fn printed(tab: Option<u32>, line: String) -> String {
		set_active_tab(Some(1));
		let out = with_log_tab(tab, || tag_line(line));
		set_active_tab(None);
		out
	}

	#[test]
	fn every_probe_line_reads_back_its_ids_whole() {
		for tab in [None, Some(2)] {
			for id in ODD {
				let line = printed(tab, bounds(id, [12, -3, 100, 20]));
				assert_eq!(
					e2e::parse_bounds(&line),
					Some((id.to_string(), [12, -3, 100, 20])),
					"{line}"
				);
				let line = printed(tab, gone(id));
				assert_eq!(
					e2e::parse_gone(&line).as_deref(),
					Some(id),
					"{line}"
				);
				let line = printed(tab, duplicate(id));
				assert_eq!(
					e2e::parse_duplicate(&line).as_deref(),
					Some(id),
					"{line}"
				);
				let line = printed(tab, text_fit_gone(id));
				assert_eq!(
					e2e::parse_text_fit_gone(&line).as_deref(),
					Some(id),
					"{line}"
				);
				for by in ODD {
					let line = printed(tab, covered(id, by));
					assert_eq!(
						e2e::parse_covered(&line),
						Some((id.to_string(), by.to_string())),
						"{line}"
					);
				}
			}
		}
	}
}
