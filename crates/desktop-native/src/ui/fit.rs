//! One rule for how a text gives way to its neighbours in the Log window
//! (#228, ADR 0005): the rule table ([`FitRole::rule`]), the solver that
//! shares a row's width ([`fit_row`]) and the `TEXT_FIT` probe that checks
//! what was drawn against it.
//!
//! The first half is pure: it needs no GPUI, only a `measure` function from
//! text to width. The second draws a text in its box ([`FitCell`]) and
//! reports it to the probe.

use std::fmt;

use gpui::{
	canvas, div, prelude::*, px, AnyElement, Div, FontWeight, SharedString,
	Window,
};
use unicode_segmentation::UnicodeSegmentation;

use super::Probes;

use crate::theme::{CODE_FONT, SMALL_TEXT, UI_FONT, UI_TEXT};

/// A text of the Log window that shares its width with others. Every role
/// has a rule, a presence, a font and a chrome: a new role does not compile
/// until it has them (the matches below have no `_ =>`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum FitRole {
	Subject,
	BranchLabel,
	TagLabel,
	RefMore,
	Collapsed,
	Author,
	Date,
	Hash,
	ChipRepo,
	ChipBranch,
	ChipUser,
	ChipDate,
	ChipMore,
	RefHint,
	PaneGroup,
	PaneRefName,
	PaneRepo,
	MenuRefName,
	MenuRepo,
}

/// The one description of how a text is drawn: its family, size and weight.
/// A role has a style ([`FitRole::style`]) and a cell is drawn in it
/// ([`FitCell::render`] pins all three on its box), the floors and the
/// `TEXT_FIT` probe measure in it, and nothing else decides: a weight put on
/// a cell after it is rendered is a [`TextFit::style_drift`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct FitStyle {
	pub family: &'static str,
	pub size: f32,
	pub weight: FontWeight,
}

impl FitStyle {
	/// The regular weight, which is what everything but the viewer's own
	/// name is drawn in.
	pub(crate) const fn regular(family: &'static str, size: f32) -> Self {
		Self {
			family,
			size,
			weight: FontWeight::NORMAL,
		}
	}

	/// The text system's width of `s` in this style.
	pub(crate) fn width(self, window: &Window, s: &str) -> f32 {
		text_width_in(window, self.family, self.size, self.weight, s)
	}
}

/// How much of a text must stay in sight (graphemes, not pixels).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FitRule {
	/// All of it, or none.
	Whole,
	/// At least this many graphemes before the ellipsis.
	Readable(usize),
	/// The key whole, and this many graphemes of the value.
	KeyValue(usize),
}

/// What a role does when there is not even room for its floor. The table
/// states it; the claims of the Log rows (`log.rs`) are written to match, and
/// the tests read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum Presence {
	/// Never absent.
	Always,
	/// With its group: all of them or none.
	Group,
	/// With its column: the column is absent in every row.
	Column,
	/// Folded into the overflow chip.
	Overflow,
	/// Whole or absent.
	Optional,
}

/// The icon and the gap before a label's text.
pub(crate) const LABEL_ICON_W: f32 = 14.;
pub(crate) const LABEL_ICON_GAP: f32 = 3.;
/// A chip's padding (6 + 6), the gap (3) and the 12px icon around its text.
pub(crate) const CHIP_CHROME_W: f32 = 27.;
/// The overflow chip's padding (4 + 4) and its 10px icon; the gap before the
/// icon is [`LABEL_ICON_GAP`]. A hidden filter with a value adds a dot in
/// front of the icon, which is not part of [`FitRole::chrome`].
pub(crate) const MORE_PAD_W: f32 = 8.;
pub(crate) const MORE_ICON_W: f32 = 10.;
/// What gpui's ellipsis needs beyond the shaped width: it adds up the
/// characters one by one, which can come out a hair wider than the shaped
/// line, and cuts the line when that sum, rounded down, exceeds the box
/// (`LineWrapper::truncate_line`). Every width the rule table gives
/// includes it.
pub(crate) const FIT_MARGIN: f32 = 1.;

pub(crate) const ELLIPSIS: &str = "\u{2026}";

impl FitRole {
	#[cfg_attr(not(test), allow(dead_code))]
	pub(crate) const ALL: [FitRole; 19] = [
		FitRole::Subject,
		FitRole::BranchLabel,
		FitRole::TagLabel,
		FitRole::RefMore,
		FitRole::Collapsed,
		FitRole::Author,
		FitRole::Date,
		FitRole::Hash,
		FitRole::ChipRepo,
		FitRole::ChipBranch,
		FitRole::ChipUser,
		FitRole::ChipDate,
		FitRole::ChipMore,
		FitRole::RefHint,
		FitRole::PaneGroup,
		FitRole::PaneRefName,
		FitRole::PaneRepo,
		FitRole::MenuRefName,
		FitRole::MenuRepo,
	];

	/// The name in a `TEXT_FIT` id.
	pub(crate) fn key(self) -> &'static str {
		match self {
			FitRole::Subject => "subject",
			FitRole::BranchLabel => "branch",
			FitRole::TagLabel => "tag",
			FitRole::RefMore => "more",
			FitRole::Collapsed => "collapsed",
			FitRole::Author => "author",
			FitRole::Date => "date",
			FitRole::Hash => "hash",
			FitRole::ChipRepo => "chip-repo",
			FitRole::ChipBranch => "chip-branch",
			FitRole::ChipUser => "chip-user",
			FitRole::ChipDate => "chip-date",
			FitRole::ChipMore => "chip-more",
			FitRole::RefHint => "ref-hint",
			FitRole::PaneGroup => "pane-group",
			FitRole::PaneRefName => "pane-name",
			FitRole::PaneRepo => "pane-repo",
			FitRole::MenuRefName => "menu-name",
			FitRole::MenuRepo => "menu-repo",
		}
	}

	/// The role named by `key`.
	#[cfg_attr(not(test), allow(dead_code))]
	pub(crate) fn from_key(key: &str) -> Option<FitRole> {
		FitRole::ALL.into_iter().find(|r| r.key() == key)
	}

	/// How much of the text must stay in sight (ADR 0005's table).
	pub(crate) fn rule(self) -> FitRule {
		match self {
			FitRole::Subject => FitRule::Readable(6),
			FitRole::BranchLabel | FitRole::TagLabel => FitRule::Readable(6),
			FitRole::RefMore => FitRule::Whole,
			FitRole::Collapsed => FitRule::Whole,
			FitRole::Author => FitRule::Readable(3),
			FitRole::Date => FitRule::Whole,
			FitRole::Hash => FitRule::Whole,
			FitRole::ChipRepo
			| FitRole::ChipBranch
			| FitRole::ChipUser
			| FitRole::ChipDate => FitRule::KeyValue(4),
			FitRole::ChipMore => FitRule::Whole,
			FitRole::RefHint => FitRule::KeyValue(6),
			FitRole::PaneGroup
			| FitRole::PaneRefName
			| FitRole::MenuRefName => FitRule::Readable(6),
			FitRole::PaneRepo | FitRole::MenuRepo => FitRule::Whole,
		}
	}

	/// What happens when even the floor does not fit.
	#[cfg_attr(not(test), allow(dead_code))]
	pub(crate) fn presence(self) -> Presence {
		match self {
			FitRole::Subject => Presence::Always,
			FitRole::BranchLabel | FitRole::TagLabel => Presence::Group,
			FitRole::RefMore => Presence::Group,
			FitRole::Collapsed => Presence::Optional,
			FitRole::Author => Presence::Column,
			FitRole::Date => Presence::Always,
			FitRole::Hash => Presence::Always,
			FitRole::ChipRepo
			| FitRole::ChipBranch
			| FitRole::ChipUser
			| FitRole::ChipDate => Presence::Overflow,
			FitRole::ChipMore => Presence::Always,
			FitRole::RefHint => Presence::Always,
			FitRole::PaneGroup
			| FitRole::PaneRefName
			| FitRole::MenuRefName => Presence::Always,
			FitRole::PaneRepo | FitRole::MenuRepo => Presence::Optional,
		}
	}

	/// The style the text is drawn in, for everything but the viewer's own
	/// name.
	pub(crate) fn style(self) -> FitStyle {
		self.style_for(false)
	}

	/// The style of a text that may be the viewer's own: the author's name
	/// of their own commits is semibold, and its floor, its box and the
	/// probe all follow.
	pub(crate) fn style_for(self, own: bool) -> FitStyle {
		match self {
			FitRole::Hash => FitStyle::regular(CODE_FONT, SMALL_TEXT),
			FitRole::BranchLabel
			| FitRole::TagLabel
			| FitRole::RefMore
			| FitRole::Collapsed
			| FitRole::PaneRepo
			| FitRole::MenuRepo => FitStyle::regular(UI_FONT, SMALL_TEXT),
			FitRole::Author => FitStyle {
				weight: if own {
					FontWeight::SEMIBOLD
				} else {
					FontWeight::NORMAL
				},
				..FitStyle::regular(UI_FONT, UI_TEXT)
			},
			FitRole::Subject
			| FitRole::Date
			| FitRole::ChipRepo
			| FitRole::ChipBranch
			| FitRole::ChipUser
			| FitRole::ChipDate
			| FitRole::ChipMore
			| FitRole::RefHint
			| FitRole::PaneGroup
			| FitRole::PaneRefName
			| FitRole::MenuRefName => FitStyle::regular(UI_FONT, UI_TEXT),
		}
	}

	/// The width around the text inside the role's box: a label's icon and
	/// gap, a chip's padding, gap and icon (the More chip's without its
	/// dot).
	pub(crate) fn chrome(self) -> f32 {
		match self {
			FitRole::BranchLabel | FitRole::TagLabel => {
				LABEL_ICON_W + LABEL_ICON_GAP
			}
			FitRole::ChipRepo
			| FitRole::ChipBranch
			| FitRole::ChipUser
			| FitRole::ChipDate => CHIP_CHROME_W,
			FitRole::ChipMore => MORE_PAD_W + LABEL_ICON_GAP + MORE_ICON_W,
			FitRole::Subject
			| FitRole::RefMore
			| FitRole::Collapsed
			| FitRole::Author
			| FitRole::Date
			| FitRole::Hash
			| FitRole::RefHint
			| FitRole::PaneGroup
			| FitRole::PaneRefName
			| FitRole::PaneRepo
			| FitRole::MenuRefName
			| FitRole::MenuRepo => 0.,
		}
	}
}

impl FitRule {
	/// `whole`, `readable<n>` or `kv<n>`, as a `TEXT_FIT` line writes it.
	pub(crate) fn name(self) -> String {
		match self {
			FitRule::Whole => "whole".into(),
			FitRule::Readable(n) => format!("readable{n}"),
			FitRule::KeyValue(n) => format!("kv{n}"),
		}
	}

	/// The rule `name` writes.
	#[cfg_attr(not(test), allow(dead_code))]
	pub(crate) fn parse(name: &str) -> Option<FitRule> {
		if name == "whole" {
			return Some(FitRule::Whole);
		}
		if let Some(n) = name.strip_prefix("readable") {
			return n.parse().ok().map(FitRule::Readable);
		}
		name.strip_prefix("kv")
			.and_then(|n| n.parse().ok())
			.map(FitRule::KeyValue)
	}
}

pub(crate) fn grapheme_count(s: &str) -> usize {
	s.graphemes(true).count()
}

/// The first `n` graphemes of `s`.
fn prefix_graphemes(s: &str, n: usize) -> &str {
	match s.grapheme_indices(true).nth(n) {
		Some((i, _)) => &s[..i],
		None => s,
	}
}

/// The width of the whole text in its role's box: its chrome, the text, and
/// [`FIT_MARGIN`].
pub(crate) fn natural_px(
	role: FitRole,
	text: &str,
	measure: &dyn Fn(&str) -> f32,
) -> f32 {
	role.chrome() + measure(text) + FIT_MARGIN
}

/// The narrowest the role's rule allows `text`'s box (see [`FitRule`]); never
/// more than [`natural_px`]. `key_end` is where a `KeyValue` text's key ends
/// (a byte index; the key is what comes before, `": "` included).
pub(crate) fn floor_px(
	role: FitRole,
	text: &str,
	key_end: usize,
	measure: &dyn Fn(&str) -> f32,
) -> f32 {
	let natural = natural_px(role, text, measure);
	let kept = match role.rule() {
		FitRule::Whole => return natural,
		FitRule::Readable(n) => {
			if grapheme_count(text) <= n {
				return natural;
			}
			format!("{}{ELLIPSIS}", prefix_graphemes(text, n))
		}
		FitRule::KeyValue(n) => {
			let key_end = key_end.min(text.len());
			let (key, value) = text.split_at(key_end);
			if grapheme_count(value) <= n {
				return natural;
			}
			format!("{key}{}{ELLIPSIS}", prefix_graphemes(value, n))
		}
	};
	(role.chrome() + measure(&kept) + FIT_MARGIN).min(natural)
}

/// How many graphemes of `text` a box `visible` wide shows, of how many it
/// has: all of them when the whole text fits, else the most whose prefix and
/// the ellipsis fit (the way gpui cuts).
pub(crate) fn shown_graphemes(
	role: FitRole,
	text: &str,
	visible: f32,
	measure: &dyn Fn(&str) -> f32,
) -> (usize, usize) {
	let total = grapheme_count(text);
	if role.chrome() + measure(text) <= visible + 0.5 {
		return (total, total);
	}
	// gpui's ellipsis keeps the prefix whose width and the ellipsis's stay
	// under the box; the margin is the floor's, it is not asked of what is
	// seen (a box snapped to a physical pixel can be 0.5px under it).
	let fits = |k: usize| {
		let kept = format!("{}{ELLIPSIS}", prefix_graphemes(text, k));
		role.chrome() + measure(&kept) <= visible + 0.01
	};
	// The most graphemes that fit; a longer prefix is never narrower.
	let (mut lo, mut hi) = (0, total);
	while lo < hi {
		let mid = (lo + hi).div_ceil(2);
		if fits(mid) {
			lo = mid;
		} else {
			hi = mid - 1;
		}
	}
	(lo, total)
}

/// Whether `shown` of `total` graphemes satisfies `rule`; `key_graphemes` is a
/// `KeyValue` text's key length.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn rule_holds(
	rule: FitRule,
	shown: usize,
	total: usize,
	key_graphemes: usize,
) -> bool {
	match rule {
		FitRule::Whole => shown == total,
		FitRule::Readable(n) => shown >= n.min(total),
		FitRule::KeyValue(n) => shown >= (key_graphemes + n).min(total),
	}
}

// ───────────────────────────── the solver ─────────────────────────────

/// One text of a row: the widest it wants, the least its rule allows, and the
/// gap before it (paid only while it is shown). `floor` never exceeds
/// `natural`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Part<P> {
	pub id: P,
	pub natural: f32,
	pub floor: f32,
	pub gap: f32,
}

impl<P> Part<P> {
	pub(crate) fn new(id: P, natural: f32, floor: f32, gap: f32) -> Self {
		let natural = natural.max(0.);
		Self {
			id,
			natural,
			floor: floor.clamp(0., natural),
			gap,
		}
	}
}

/// What a row gives next, in priority order.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Claim<P> {
	/// Every part at its floor, or none of them: when the rest cannot pay all
	/// their floors and gaps, they are `dropped` and no later claim gives
	/// them anything.
	Atomic(Vec<P>),
	/// This part up to the width (at most its natural); a part not shown yet
	/// first pays its gap. It may get only a part.
	Grow(P, f32),
	/// The shown parts, shortest natural first, each up to its natural: the
	/// short ones are whole before a long one gets more.
	ShortestFirst(Vec<P>),
	/// Everything left, up to the part's natural.
	Rest(P),
}

#[derive(Clone, Copy, Debug)]
struct Placed<P> {
	part: Part<P>,
	width: f32,
	shown: bool,
	dropped: bool,
}

/// The widths [`fit_row`] settled on.
#[derive(Clone, Debug)]
pub(crate) struct RowFit<P> {
	placed: Vec<Placed<P>>,
	slack: f32,
}

impl<P: Copy + Eq + std::fmt::Debug> RowFit<P> {
	fn get(&self, p: P) -> &Placed<P> {
		self.placed
			.iter()
			.find(|s| s.part.id == p)
			.unwrap_or_else(|| panic!("{p:?} is not a part of this row"))
	}

	/// The width given to `p`: 0 when it was dropped or got nothing.
	pub(crate) fn width(&self, p: P) -> f32 {
		self.get(p).width
	}

	pub(crate) fn natural(&self, p: P) -> f32 {
		self.get(p).part.natural
	}

	#[cfg_attr(not(test), allow(dead_code))]
	pub(crate) fn floor(&self, p: P) -> f32 {
		self.get(p).part.floor
	}

	/// Whether `p` was given a place in the row (its gap was paid).
	pub(crate) fn shown(&self, p: P) -> bool {
		self.get(p).shown
	}

	/// Whether an [`Claim::Atomic`] group holding `p` was refused.
	#[cfg_attr(not(test), allow(dead_code))]
	pub(crate) fn dropped(&self, p: P) -> bool {
		self.get(p).dropped
	}

	/// What was left after every claim.
	#[cfg_attr(not(test), allow(dead_code))]
	pub(crate) fn slack(&self) -> f32 {
		self.slack
	}
}

/// Shares `avail` px between `parts` as `claims` say, in their order.
/// Invariants: no part is wider than its natural; only shown parts pay their
/// gap; the shown widths and gaps add up to at most `avail`.
pub(crate) fn fit_row<P: Copy + Eq + std::fmt::Debug>(
	avail: f32,
	parts: &[Part<P>],
	claims: &[Claim<P>],
) -> RowFit<P> {
	let mut fit = RowFit {
		placed: parts
			.iter()
			.map(|&part| Placed {
				part,
				width: 0.,
				shown: false,
				dropped: false,
			})
			.collect(),
		slack: avail,
	};
	let ix = |fit: &RowFit<P>, p: P| {
		fit.placed
			.iter()
			.position(|s| s.part.id == p)
			.unwrap_or_else(|| panic!("{p:?} is not a part of this row"))
	};
	// Costs are sums of f32: a part that fits to the last pixel still fits.
	const EPS: f32 = 1e-3;
	for claim in claims {
		match claim {
			Claim::Atomic(ps) => {
				let ixs: Vec<usize> = ps
					.iter()
					.map(|&p| ix(&fit, p))
					.filter(|&i| !fit.placed[i].shown && !fit.placed[i].dropped)
					.collect();
				let cost: f32 = ixs
					.iter()
					.map(|&i| fit.placed[i].part.floor + fit.placed[i].part.gap)
					.sum();
				if cost <= fit.slack + EPS {
					for &i in &ixs {
						let s = &mut fit.placed[i];
						s.shown = true;
						s.width = s.part.floor;
					}
					fit.slack -= cost;
				} else {
					for &i in &ixs {
						fit.placed[i].dropped = true;
					}
				}
			}
			Claim::Grow(p, target) => {
				let i = ix(&fit, *p);
				if fit.placed[i].dropped {
					continue;
				}
				if !pay_gap(&mut fit, i) {
					continue;
				}
				let s = &mut fit.placed[i];
				let to = target.min(s.part.natural);
				let add = (to - s.width).min(fit.slack).max(0.);
				s.width += add;
				fit.slack -= add;
			}
			Claim::ShortestFirst(ps) => {
				let mut ixs: Vec<usize> = ps
					.iter()
					.map(|&p| ix(&fit, p))
					.filter(|&i| fit.placed[i].shown)
					.collect();
				ixs.sort_by(|&a, &b| {
					fit.placed[a]
						.part
						.natural
						.total_cmp(&fit.placed[b].part.natural)
				});
				for i in ixs {
					let s = &mut fit.placed[i];
					let add = (s.part.natural - s.width).min(fit.slack).max(0.);
					s.width += add;
					fit.slack -= add;
				}
			}
			Claim::Rest(p) => {
				let i = ix(&fit, *p);
				if fit.placed[i].dropped || !pay_gap(&mut fit, i) {
					continue;
				}
				let s = &mut fit.placed[i];
				let add = (s.part.natural - s.width).min(fit.slack).max(0.);
				s.width += add;
				fit.slack -= add;
			}
		}
	}
	fit
}

/// Shows the part at `i` if it is not yet: pays its gap, if some room is
/// left after it. Whether it is shown.
fn pay_gap<P>(fit: &mut RowFit<P>, i: usize) -> bool {
	if fit.placed[i].shown {
		return true;
	}
	let gap = fit.placed[i].part.gap;
	if fit.slack <= gap {
		return false;
	}
	fit.slack -= gap;
	fit.placed[i].shown = true;
	true
}

// ───────────────────────── drawing and the probe ─────────────────────────

/// Width of `s` shaped in `family` at `size` and `weight`, from the text
/// system (shaped lines are cached per frame, so a redraw does not reshape).
pub(crate) fn text_width_in(
	window: &Window,
	family: &str,
	size: f32,
	weight: FontWeight,
	s: &str,
) -> f32 {
	let s = s.replace(['\n', '\r'], " ");
	let mut font = gpui::font(family.to_string());
	font.weight = weight;
	let run = gpui::TextRun {
		len: s.len(),
		font,
		color: gpui::black(),
		background_color: None,
		underline: None,
		strikethrough: None,
	};
	f32::from(
		window
			.text_system()
			.shape_line(s.into(), px(size), &[run], None)
			.width,
	)
}

/// `measure` for a role: its style from the text system.
pub(crate) fn role_measure<'a>(
	window: &'a Window,
	role: FitRole,
) -> impl Fn(&str) -> f32 + 'a {
	style_measure(window, role.style())
}

/// `measure` for a role whose text may be the viewer's own (see
/// [`FitRole::style_for`]).
pub(crate) fn role_measure_for<'a>(
	window: &'a Window,
	role: FitRole,
	own: bool,
) -> impl Fn(&str) -> f32 + 'a {
	style_measure(window, role.style_for(own))
}

/// `measure` in `style`.
pub(crate) fn style_measure<'a>(
	window: &'a Window,
	style: FitStyle,
) -> impl Fn(&str) -> f32 + 'a {
	move |s: &str| style.width(window, s)
}

/// Where a `TEXT_FIT` belongs: the scope is the id of the probe of the thing
/// that holds the text (`row:<key>`, `ref:<name>`, `log-filter-<key>`), the
/// role says which text of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FitId {
	pub scope: SharedString,
	pub role: FitRole,
}

impl FitId {
	pub(crate) fn new(scope: impl Into<SharedString>, role: FitRole) -> Self {
		Self {
			scope: scope.into(),
			role,
		}
	}
}

impl fmt::Display for FitId {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "text-fit:{}:{}", self.scope, self.role.key())
	}
}

/// What the probe saw of one text, against what the solver gave it.
#[derive(Clone, Debug, PartialEq)]
pub struct TextFit {
	pub role: FitRole,
	/// The style the floor and the counts were measured in: the cell's own.
	pub(crate) style: FitStyle,
	/// What gpui draws it in when that is not the cell's style (a weight or
	/// a size put on the box after it was rendered), as `family/size/weight`.
	pub style_drift: Option<String>,
	pub text: SharedString,
	/// The key's graphemes of a `KeyValue` text, else 0.
	pub key_graphemes: usize,
	/// The whole text's width in its box, and the rule's floor.
	pub natural: f32,
	pub floor: f32,
	/// The width the solver gave: `None` for a text that flexes (a header
	/// hint), `Some(0.)` for one it dropped.
	pub fit: Option<f32>,
	/// Logical px: where the box starts, how wide it was drawn, and how
	/// much of it no ancestor clips away.
	pub x: f32,
	pub drawn: f32,
	pub visible: f32,
	/// An ancestor clips part of the box.
	pub clipped: bool,
	/// The text shows an ellipsis of its own.
	pub cut: bool,
	/// Graphemes in sight, of how many (see [`shown_graphemes`]).
	pub shown: usize,
	pub total: usize,
}

impl TextFit {
	/// A text the solver dropped: nothing drawn.
	fn dropped(
		role: FitRole,
		style: FitStyle,
		text: SharedString,
		key_end: usize,
		natural: f32,
		floor: f32,
	) -> Self {
		let total = grapheme_count(&text);
		Self {
			role,
			style,
			style_drift: None,
			key_graphemes: key_graphemes(role, &text, key_end),
			text,
			natural,
			floor,
			fit: Some(0.),
			x: 0.,
			drawn: 0.,
			visible: 0.,
			clipped: false,
			cut: true,
			shown: 0,
			total,
		}
	}

	/// The solver gave a width and the drawn box differs by more than 1px:
	/// something between the solver and the screen changed it.
	pub fn drifts(&self) -> bool {
		self.fit.is_some_and(|fit| (self.drawn - fit).abs() > 1.)
	}

	/// The box is drawn in another style than the one its floor and counts
	/// were measured in: what the probe says is not what is on screen.
	pub fn style_drifts(&self) -> bool {
		self.style_drift.is_some()
	}

	/// The log line, for the id.
	pub(super) fn line(&self, id: &str) -> String {
		let fit = match self.fit {
			Some(f) => format!("{:.0}", f),
			None => "-".into(),
		};
		format!(
			"[APP:TEXT_FIT: id={id} rule={} natural={:.0} floor={:.0} fit={fit} drawn={:.0} visible={:.0} clipped={} cut={} shown={}/{}]",
			self.role.rule().name(),
			self.natural,
			self.floor,
			self.drawn,
			self.visible,
			u8::from(self.clipped),
			u8::from(self.cut),
			self.shown,
			self.total,
		)
	}

	pub(super) fn drift_line(&self, id: &str) -> String {
		format!(
			"[APP:TEXT_FIT_DRIFT: id={id} fit={:.0} drawn={:.0}]",
			self.fit.unwrap_or(0.),
			self.drawn
		)
	}

	pub(super) fn style_drift_line(&self, id: &str) -> String {
		format!(
			"[APP:TEXT_FIT_STYLE_DRIFT: id={id} measured={} drawn={}]",
			style_text(self.style.family, self.style.size, self.style.weight),
			self.style_drift.as_deref().unwrap_or("-")
		)
	}
}

fn style_text(family: &str, size: f32, weight: FontWeight) -> String {
	format!("{family}/{size}/{}", weight.0)
}

fn key_graphemes(role: FitRole, text: &str, key_end: usize) -> usize {
	match role.rule() {
		FitRule::KeyValue(_) => {
			grapheme_count(&text[..key_end.min(text.len())])
		}
		FitRule::Whole | FitRule::Readable(_) => 0,
	}
}

/// How a cell is given its width.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum FitWidth {
	/// The solver's width.
	Solved { fit: f32 },
	/// No width of its own: the box shrinks with its parent (a header hint
	/// that gives way to the buttons beside it).
	Flex,
}

/// One text that shares a row: the box with the role's font, ellipsis and
/// the probe. Dropped texts are not drawn at all: call [`record_dropped`].
pub(crate) struct FitCell {
	id: FitId,
	text: SharedString,
	key_end: usize,
	selector: Option<String>,
	leading: Option<AnyElement>,
	trailing: Option<AnyElement>,
	pad: f32,
	frame: f32,
	width: FitWidth,
	own: bool,
}

impl FitCell {
	pub(crate) fn new(id: FitId, text: impl Into<SharedString>) -> Self {
		Self {
			id,
			text: text.into(),
			key_end: 0,
			selector: None,
			leading: None,
			trailing: None,
			pad: 0.,
			frame: 0.,
			width: FitWidth::Flex,
			own: false,
		}
	}

	/// The text is the viewer's own: it is drawn, floored and probed in the
	/// role's own style ([`FitRole::style_for`]), never by a weight added to
	/// the box afterwards.
	pub(crate) fn own(mut self, own: bool) -> Self {
		self.own = own;
		self
	}

	/// Where a `KeyValue` text's key ends (a byte index of the text).
	pub(crate) fn key_end(mut self, i: usize) -> Self {
		self.key_end = i;
		self
	}

	/// The debug selector of the box (`log-subject:{ix}` and its family).
	pub(crate) fn selector(mut self, s: String) -> Self {
		self.selector = Some(s);
		self
	}

	/// An icon before the text (a label's glyph).
	pub(crate) fn leading(mut self, e: impl IntoElement) -> Self {
		self.leading = Some(e.into_any_element());
		self
	}

	/// An icon after the text (a chip's clear button or arrow).
	pub(crate) fn trailing(mut self, e: impl IntoElement) -> Self {
		self.trailing = Some(e.into_any_element());
		self
	}

	/// Padding left and right of the content (a chip's 6px).
	pub(crate) fn pad(mut self, px: f32) -> Self {
		self.pad = px;
		self
	}

	/// A border of this width on each side that the caller adds to the box
	/// after [`FitCell::render`] (an active overflow chip's ring). It sits
	/// outside what the probe's canvas fills, so the probe adds it back.
	pub(crate) fn frame(mut self, border: f32) -> Self {
		self.frame = border;
		self
	}

	/// The width the solver gave `p`.
	pub(crate) fn solved<P: Copy + Eq + fmt::Debug>(
		mut self,
		fit: &RowFit<P>,
		p: P,
	) -> Self {
		self.width = FitWidth::Solved { fit: fit.width(p) };
		self
	}

	/// A width that is not the solver's: an overflow chip drawn where the
	/// split found no room for it, a test's box.
	pub(crate) fn fixed_width(mut self, fit: f32) -> Self {
		self.width = FitWidth::Solved { fit };
		self
	}

	/// No width of its own.
	pub(crate) fn flex(mut self) -> Self {
		self.width = FitWidth::Flex;
		self
	}

	/// The box. A caller that needs a tooltip or a click adds `.id(..)` to
	/// it; the probe is a canvas inside, so the box keeps its children.
	pub(crate) fn render(
		self,
		probes: &Option<Probes>,
		window: &Window,
	) -> Div {
		let Self {
			id,
			text,
			key_end,
			selector,
			leading,
			trailing,
			pad,
			frame: border,
			width,
			own,
		} = self;
		let role = id.role;
		let style = role.style_for(own);
		// No `overflow_hidden` here: gpui clips to the box without its
		// padding, and the probe reads the clip of the ancestors, not the
		// cell's own. The text clips itself, and the solver gives a box
		// that holds the icons and padding.
		let mut root = div()
			.relative()
			.flex()
			.flex_row()
			.items_center()
			.font_family(style.family)
			.text_size(px(style.size))
			.font_weight(style.weight);
		root = match width {
			FitWidth::Solved { fit } => root.w(px(fit)).flex_shrink_0(),
			FitWidth::Flex => root.min_w_0(),
		};
		if pad > 0. {
			root = root.px(px(pad));
		}
		if leading.is_some() || trailing.is_some() {
			root = root.gap(px(LABEL_ICON_GAP));
		}
		if let Some(s) = selector {
			root = root.debug_selector(move || s);
		}
		if let Some(e) = leading {
			root = root.child(div().flex_shrink_0().child(e));
		}
		root = root.child(
			div()
				.min_w_0()
				.overflow_hidden()
				.line_clamp(1)
				.text_ellipsis()
				.child(text.clone()),
		);
		if let Some(e) = trailing {
			root = root.child(div().flex_shrink_0().child(e));
		}
		if let Some(probes) = probes {
			let frame = probes.0.clone();
			let measure = style_measure(window, style);
			let natural = natural_px(role, &text, &measure);
			let floor = floor_px(role, &text, key_end, &measure);
			let fit = match width {
				FitWidth::Solved { fit } => Some(fit),
				FitWidth::Flex => None,
			};
			let id = id.to_string();
			root = root.child(
				canvas(
					move |b, window, _| {
						// Across only: a row scrolled out of its list is not
						// clipped in width.
						let mask = window.content_mask().bounds;
						// The canvas fills the box inside the border.
						let (left, right) = (
							f32::from(b.left()) - border,
							f32::from(b.right()) + border,
						);
						let drawn = right - left;
						let visible_w = (right.min(f32::from(mask.right()))
							- left.max(f32::from(mask.left())))
						.clamp(0., drawn);
						// The shown count is read off the text system: do it
						// only when what it depends on changed.
						let known = frame.borrow().known_fit(&id).filter(|k| {
							k.text == text
								&& (k.visible - visible_w).abs() < 0.01
						});
						// What gpui draws it in: the style the box inherits
						// at this point, whoever set it.
						let painted = window.text_style();
						let painted_size = f32::from(
							painted.font_size.to_pixels(window.rem_size()),
						);
						let style_drift =
							((painted.font_family.as_ref() != style.family)
								|| (painted_size - style.size).abs() > 0.01
								|| painted.font_weight != style.weight)
								.then(|| {
									style_text(
										painted.font_family.as_ref(),
										painted_size,
										painted.font_weight,
									)
								});
						let (shown, total) = match known {
							Some(k)
								if k.style == style
									&& k.style_drift == style_drift =>
							{
								(k.shown, k.total)
							}
							_ => {
								// Counted in what is on screen, so a drift
								// cannot hide behind a kind measure.
								let measure = |s: &str| {
									text_width_in(
										window,
										painted.font_family.as_ref(),
										painted_size,
										painted.font_weight,
										s,
									)
								};
								shown_graphemes(
									role, &text, visible_w, &measure,
								)
							}
						};
						let shorter = fit.unwrap_or(drawn).min(visible_w);
						frame.borrow_mut().record_fit(
							&id,
							TextFit {
								role,
								style,
								style_drift,
								key_graphemes: key_graphemes(
									role, &text, key_end,
								),
								text: text.clone(),
								natural,
								floor,
								fit,
								x: left,
								drawn,
								visible: visible_w,
								clipped: visible_w < drawn - 0.5,
								cut: shorter < natural - 0.5,
								shown,
								total,
							},
						);
					},
					|_, _, _, _| {},
				)
				// From the box's own corner: an absolute child with no
				// offset starts after the padding, so a padded chip's probe
				// would stand 4 to 6px to the right of the chip and be
				// reported clipped where the chip is last on its bar.
				.absolute()
				.top_0()
				.left_0()
				.size_full(),
			);
		}
		root
	}
}

/// Records a text the solver dropped, so `TEXT_FIT` has a line for it
/// (`fit=0`) and the matrix can tell a dropped group from a missing one.
pub(crate) fn record_dropped(
	probes: &Option<Probes>,
	id: FitId,
	style: FitStyle,
	text: &str,
	natural: f32,
	floor: f32,
) {
	let Some(probes) = probes else { return };
	probes.0.borrow_mut().record_fit(
		&id.to_string(),
		TextFit::dropped(
			id.role,
			style,
			text.to_string().into(),
			0,
			natural,
			floor,
		),
	);
}

/// The stretch of a row that is not text: it takes what the texts leave, so
/// the fixed columns on the right stay on the right. The one `flex_1` a Log
/// row may have (`check_text_fit.sh`).
pub(crate) fn spacer() -> Div {
	div().flex_1().min_w_0()
}

#[cfg(test)]
mod tests {
	use super::*;

	/// 0.6em per char, as the test text system measures (12px: 7.2).
	fn noop(size: f32) -> impl Fn(&str) -> f32 {
		move |s: &str| s.chars().count() as f32 * 0.6 * size
	}

	// ── R1, R2: the rule table ──

	/// What ADR 0005's table says, written once more.
	const TABLE: [(FitRole, &str, FitRule, Presence); 19] = [
		(
			FitRole::Subject,
			"subject",
			FitRule::Readable(6),
			Presence::Always,
		),
		(
			FitRole::BranchLabel,
			"branch",
			FitRule::Readable(6),
			Presence::Group,
		),
		(
			FitRole::TagLabel,
			"tag",
			FitRule::Readable(6),
			Presence::Group,
		),
		(FitRole::RefMore, "more", FitRule::Whole, Presence::Group),
		(
			FitRole::Collapsed,
			"collapsed",
			FitRule::Whole,
			Presence::Optional,
		),
		(
			FitRole::Author,
			"author",
			FitRule::Readable(3),
			Presence::Column,
		),
		(FitRole::Date, "date", FitRule::Whole, Presence::Always),
		(FitRole::Hash, "hash", FitRule::Whole, Presence::Always),
		(
			FitRole::ChipRepo,
			"chip-repo",
			FitRule::KeyValue(4),
			Presence::Overflow,
		),
		(
			FitRole::ChipBranch,
			"chip-branch",
			FitRule::KeyValue(4),
			Presence::Overflow,
		),
		(
			FitRole::ChipUser,
			"chip-user",
			FitRule::KeyValue(4),
			Presence::Overflow,
		),
		(
			FitRole::ChipDate,
			"chip-date",
			FitRule::KeyValue(4),
			Presence::Overflow,
		),
		(
			FitRole::ChipMore,
			"chip-more",
			FitRule::Whole,
			Presence::Always,
		),
		(
			FitRole::RefHint,
			"ref-hint",
			FitRule::KeyValue(6),
			Presence::Always,
		),
		(
			FitRole::PaneGroup,
			"pane-group",
			FitRule::Readable(6),
			Presence::Always,
		),
		(
			FitRole::PaneRefName,
			"pane-name",
			FitRule::Readable(6),
			Presence::Always,
		),
		(
			FitRole::PaneRepo,
			"pane-repo",
			FitRule::Whole,
			Presence::Optional,
		),
		(
			FitRole::MenuRefName,
			"menu-name",
			FitRule::Readable(6),
			Presence::Always,
		),
		(
			FitRole::MenuRepo,
			"menu-repo",
			FitRule::Whole,
			Presence::Optional,
		),
	];

	#[test]
	fn every_fit_role_has_a_key_rule_and_font() {
		assert_eq!(FitRole::ALL.len(), TABLE.len());
		let mut keys = std::collections::HashSet::new();
		for (role, key, rule, presence) in TABLE {
			assert!(FitRole::ALL.contains(&role), "{role:?} is not in ALL");
			assert_eq!(role.key(), key);
			assert_eq!(FitRole::from_key(key), Some(role));
			assert_eq!(role.rule(), rule, "{role:?}");
			assert_eq!(role.presence(), presence, "{role:?}");
			assert!(keys.insert(key), "{key} is two roles' key");
			let style = role.style();
			assert!(!style.family.is_empty() && style.size > 0.);
			assert_eq!(style.weight, FontWeight::NORMAL, "{role:?}");
			// Only an author's name has an own style; no other role's text
			// can be the viewer's own.
			let own = role.style_for(true);
			if role == FitRole::Author {
				assert_eq!(own.weight, FontWeight::SEMIBOLD);
				assert_eq!((own.family, own.size), (style.family, style.size));
			} else {
				assert_eq!(own, style, "{role:?}");
			}
			// A rule names itself as the probe prints it, and reads back.
			assert_eq!(FitRule::parse(&rule.name()), Some(rule));
		}
		assert_eq!(
			FitRole::Hash.style(),
			FitStyle::regular(CODE_FONT, SMALL_TEXT)
		);
		assert_eq!(
			FitRole::Subject.style(),
			FitStyle::regular(UI_FONT, UI_TEXT)
		);
		assert_eq!(
			FitRole::BranchLabel.style(),
			FitStyle::regular(UI_FONT, SMALL_TEXT)
		);
		assert_eq!(FitRole::BranchLabel.chrome(), 17.);
		assert_eq!(FitRole::ChipUser.chrome(), 27.);
		// The More chip's padding (4 + 4), the gap and the 10px icon.
		assert_eq!(FitRole::ChipMore.chrome(), 21.);
	}

	/// The More chip's box holds its padding and icon around the text, so
	/// the graphemes shown and the cut are counted against the text's part
	/// of a clipped box, not the whole of it.
	#[test]
	fn the_more_chip_counts_its_text_without_the_chrome() {
		let m = noop(10.);
		let role = FitRole::ChipMore;
		let text = "More";
		let natural = natural_px(role, text, &m);
		assert_eq!(natural, 21. + m(text) + FIT_MARGIN);
		assert_eq!(shown_graphemes(role, text, natural, &m), (4, 4));
		// 8px short of the box leaves no room for the whole word.
		let (shown, total) = shown_graphemes(role, text, natural - 8., &m);
		assert_eq!(total, 4);
		assert!(shown < 4, "{shown}");
	}

	#[test]
	fn readable_floor_counts_graphemes() {
		let m = noop(10.);
		let role = FitRole::Subject; // Readable(6)
							   // A CJK character, a combining pair and a ZWJ family are one each.
		let cjk = "\u{6f22}".repeat(10);
		let combining = "e\u{301}".repeat(10);
		let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}".repeat(10);
		for text in [cjk, combining, family] {
			assert_eq!(grapheme_count(&text), 10);
			// Six graphemes, the ellipsis and the margin.
			let kept = format!("{}{ELLIPSIS}", prefix_graphemes(&text, 6));
			assert_eq!(
				floor_px(role, &text, 0, &m),
				m(&kept) + FIT_MARGIN,
				"{text}"
			);
		}
		// Not longer than six: the floor is the whole text.
		for text in ["abc", "abcdef", "e\u{301}e\u{301}e\u{301}"] {
			assert_eq!(floor_px(role, text, 0, &m), natural_px(role, text, &m));
		}
		// The floor never exceeds the natural width: seven graphemes of
		// width 1 cost less whole than six and the ellipsis.
		let seven = "iiiiiii";
		let narrow = |s: &str| s.chars().count() as f32;
		assert_eq!(
			floor_px(role, seven, 0, &narrow),
			natural_px(role, seven, &narrow)
		);
		// A label's chrome is in both.
		let label = FitRole::BranchLabel;
		assert_eq!(
			floor_px(label, "abcdefgh", 0, &m),
			17. + m("abcdef\u{2026}") + 1.
		);
	}

	#[test]
	fn a_key_value_floor_keeps_the_key_whole() {
		let m = noop(10.);
		let role = FitRole::ChipUser; // KeyValue(4)
		let text = "User: someone@example.com";
		let key_end = "User: ".len();
		let want = 27. + m("User: some\u{2026}") + 1.;
		assert_eq!(floor_px(role, text, key_end, &m), want);
		// A short value is whole.
		assert_eq!(
			floor_px(role, "User: me", key_end, &m),
			natural_px(role, "User: me", &m)
		);
		// No value, no key_end: the whole key.
		assert_eq!(floor_px(role, "User", 4, &m), natural_px(role, "User", &m));
	}

	#[test]
	fn shown_graphemes_and_rule_holds_agree_with_the_floor() {
		let m = noop(10.);
		let role = FitRole::Subject;
		let text = "abcdefghijklmnopqrstuvwxyz";
		let natural = natural_px(role, text, &m);
		// Whole text in sight.
		assert_eq!(shown_graphemes(role, text, natural, &m), (26, 26));
		assert_eq!(shown_graphemes(role, text, natural + 50., &m), (26, 26));
		// At the floor: six graphemes.
		let floor = floor_px(role, text, 0, &m);
		assert_eq!(shown_graphemes(role, text, floor, &m), (6, 26));
		assert!(rule_holds(role.rule(), 6, 26, 0));
		assert!(!rule_holds(role.rule(), 5, 26, 0));
		// A hair under the floor's margin: still six; a char less: five.
		assert_eq!(shown_graphemes(role, text, floor - 1., &m), (6, 26));
		assert_eq!(shown_graphemes(role, text, floor - 1.5, &m), (5, 26));
		// Nothing fits.
		assert_eq!(shown_graphemes(role, text, 1., &m), (0, 26));
		// A text shorter than the rule needs only shows itself.
		assert!(rule_holds(FitRule::Readable(6), 3, 3, 0));
		assert!(rule_holds(FitRule::Whole, 9, 9, 0));
		assert!(!rule_holds(FitRule::Whole, 8, 9, 0));
		assert!(rule_holds(FitRule::KeyValue(4), 10, 30, 6));
		assert!(!rule_holds(FitRule::KeyValue(4), 9, 30, 6));
		assert!(rule_holds(FitRule::KeyValue(4), 8, 8, 6));
	}

	#[test]
	fn gpui_may_cut_inside_a_grapheme_and_shown_stays_on_the_safe_side() {
		// gpui truncates char by char: a ZWJ family is three chars (and two
		// joiners) to it. `shown` counts graphemes, so it counts fewer than
		// gpui keeps, never more.
		let m = noop(10.);
		let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
		let text = family.repeat(4);
		let role = FitRole::Subject;
		let (shown, total) = shown_graphemes(role, &text, 30., &m);
		assert_eq!(total, 4);
		// In 30px chars (6px each) 5 chars fit with the ellipsis; a family is
		// 5 chars: 0 whole graphemes.
		assert_eq!(shown, 0);
	}

	// ── U1–U7: the solver ──

	#[derive(Clone, Copy, Debug, PartialEq, Eq)]
	enum P {
		A,
		B,
		C,
		D,
	}

	fn part(id: P, natural: f32, floor: f32, gap: f32) -> Part<P> {
		Part::new(id, natural, floor, gap)
	}

	fn shown_total(fit: &RowFit<P>, parts: &[Part<P>]) -> f32 {
		parts
			.iter()
			.filter(|p| fit.shown(p.id))
			.map(|p| fit.width(p.id) + p.gap)
			.sum()
	}

	#[test]
	fn fit_row_stays_within_avail_and_natural() {
		let mut checked = 0;
		for avail in (0..=700).step_by(7) {
			for (na, nb, nc, nd) in [
				(80., 140., 60., 400.),
				(20., 20., 20., 20.),
				(300., 10., 120., 50.),
			] {
				for gap in [0., 6., 8.] {
					let parts = [
						part(P::A, na, na * 0.4, gap),
						part(P::B, nb, nb * 0.5, gap),
						part(P::C, nc, nc, gap),
						part(P::D, nd, nd * 0.2, gap),
					];
					let claims = [
						Claim::Atomic(vec![P::C]),
						Claim::Grow(P::D, nd * 0.2),
						Claim::Atomic(vec![P::A, P::B]),
						Claim::ShortestFirst(vec![P::A, P::B]),
						Claim::Grow(P::D, 160.),
						Claim::Rest(P::D),
					];
					let fit = fit_row(avail as f32, &parts, &claims);
					for p in &parts {
						assert!(
							fit.width(p.id) <= p.natural + 1e-3,
							"{avail} {p:?}: {}",
							fit.width(p.id)
						);
					}
					let used = shown_total(&fit, &parts);
					assert!(
						used <= avail as f32 + 1e-3,
						"{avail}: {used} > avail"
					);
					assert!((avail as f32 - used - fit.slack()).abs() < 1e-3);
					checked += 1;
				}
			}
		}
		assert!(checked > 200);
	}

	#[test]
	fn an_atomic_claim_is_whole_or_absent() {
		let parts = [
			part(P::A, 100., 40., 8.),
			part(P::B, 100., 40., 8.),
			part(P::C, 50., 50., 0.),
		];
		let claims =
			[Claim::Atomic(vec![P::A, P::B]), Claim::Atomic(vec![P::C])];
		// Room for both floors and gaps (96) and C: both at their floors.
		let fit = fit_row(146., &parts, &claims);
		assert_eq!((fit.width(P::A), fit.width(P::B)), (40., 40.));
		assert!(fit.shown(P::A) && fit.shown(P::B) && fit.shown(P::C));
		assert_eq!(fit.slack(), 0.);
		// A pixel short for the pair: neither, and C still gets its place.
		let fit = fit_row(95., &parts, &claims);
		assert!(fit.dropped(P::A) && fit.dropped(P::B));
		assert!(!fit.shown(P::A) && !fit.shown(P::B));
		assert_eq!((fit.width(P::A), fit.width(P::B)), (0., 0.));
		assert!(fit.shown(P::C) && !fit.dropped(P::C));
		assert_eq!(fit.slack(), 45.);
	}

	#[test]
	fn a_later_claim_gets_nothing_while_an_earlier_grow_is_short() {
		let parts = [part(P::A, 200., 50., 0.), part(P::B, 100., 100., 8.)];
		let claims = [Claim::Grow(P::A, 200.), Claim::Atomic(vec![P::B])];
		// A takes everything it can: B finds no room.
		let fit = fit_row(150., &parts, &claims);
		assert_eq!(fit.width(P::A), 150.);
		assert!(fit.dropped(P::B));
		// A is whole: B gets its place.
		let fit = fit_row(320., &parts, &claims);
		assert_eq!(fit.width(P::A), 200.);
		assert!(fit.shown(P::B) && fit.width(P::B) == 100.);
	}

	#[test]
	fn shortest_first_keeps_the_shorter_whole() {
		let parts = [part(P::A, 140., 40., 0.), part(P::B, 60., 40., 8.)];
		let claims = [
			Claim::Atomic(vec![P::A, P::B]),
			Claim::ShortestFirst(vec![P::A, P::B]),
		];
		// 88 pays the floors and the gap; 60 more: B (shorter) first.
		let fit = fit_row(88. + 20., &parts, &claims);
		assert_eq!(fit.width(P::B), 60.);
		assert_eq!(fit.width(P::A), 40.);
		let fit = fit_row(88. + 10., &parts, &claims);
		assert_eq!(fit.width(P::B), 50.);
		assert_eq!(fit.width(P::A), 40.);
		// Declared order does not matter: the shorter natural is served first.
		let swapped = [
			Claim::Atomic(vec![P::B, P::A]),
			Claim::ShortestFirst(vec![P::B, P::A]),
		];
		let fit = fit_row(88. + 10., &parts, &swapped);
		assert_eq!(fit.width(P::B), 50.);
	}

	#[test]
	fn no_slack_while_a_grow_or_rest_part_is_cut() {
		for avail in 0..=300 {
			let parts = [part(P::A, 120., 30., 8.), part(P::B, 200., 20., 8.)];
			let claims = [
				Claim::Atomic(vec![P::A]),
				Claim::Grow(P::B, 20.),
				Claim::ShortestFirst(vec![P::A]),
				Claim::Rest(P::B),
			];
			let fit = fit_row(avail as f32, &parts, &claims);
			if fit.shown(P::B) && fit.width(P::B) < fit.natural(P::B) - 1e-3 {
				assert!(fit.slack() < 1e-3, "{avail}: {}", fit.slack());
			}
		}
	}

	#[test]
	fn only_shown_parts_pay_their_gap() {
		let parts = [part(P::A, 50., 50., 20.), part(P::B, 50., 50., 20.)];
		// A dropped part pays nothing: B gets all of its room.
		let claims = [Claim::Atomic(vec![P::A]), Claim::Atomic(vec![P::B])];
		let fit = fit_row(70., &parts, &claims);
		assert!(fit.shown(P::A) && fit.dropped(P::B));
		assert_eq!(fit.slack(), 0.);
		// A Grow with no room for its gap is skipped, not dropped.
		let claims = [Claim::Grow(P::A, 50.)];
		let fit = fit_row(20., &parts, &claims);
		assert!(!fit.shown(P::A) && !fit.dropped(P::A));
		assert_eq!(fit.slack(), 20.);
	}

	#[test]
	fn a_failed_atomic_part_stays_dropped() {
		let parts = [part(P::A, 100., 80., 8.), part(P::B, 100., 20., 8.)];
		let claims = [
			Claim::Atomic(vec![P::A]),
			Claim::Grow(P::A, 100.),
			Claim::ShortestFirst(vec![P::A, P::B]),
			Claim::Rest(P::A),
			Claim::Atomic(vec![P::B]),
		];
		// 60 cannot pay A's 88: A is dropped and later claims leave it alone,
		// while B still fits in its 28.
		let fit = fit_row(60., &parts, &claims);
		assert!(fit.dropped(P::A));
		assert_eq!(fit.width(P::A), 0.);
		assert!(!fit.shown(P::A));
		assert!(fit.shown(P::B) && fit.width(P::B) == 20.);
	}
}

#[cfg(test)]
mod render_tests {
	use super::*;
	use crate::ui::probe_frame_end;
	use gpui::{Context, Render, TestAppContext};

	/// A 100px box in a 40px one: the ancestor clips it.
	struct Clipped {
		probes: Option<Probes>,
	}

	impl Render for Clipped {
		fn render(
			&mut self,
			window: &mut Window,
			_: &mut Context<Self>,
		) -> impl IntoElement {
			div()
				.size_full()
				.child(
					div().w(px(40.)).overflow_hidden().child(
						FitCell::new(
							FitId::new("t", FitRole::Subject),
							"abcdefghij",
						)
						.fixed_width(100.)
						.render(&self.probes, window),
					),
				)
				.children(probe_frame_end(&self.probes))
		}
	}

	/// G7: an ancestor that clips a text is seen even where the text's box is
	/// the width the solver gave (`drawn == fit`).
	#[gpui::test]
	fn text_fit_reports_an_ancestor_clip(cx: &mut TestAppContext) {
		let probes = Probes::for_test();
		let shared = probes.clone();
		let (_, cx) = cx.add_window_view(|_, _| Clipped {
			probes: Some(probes),
		});
		cx.run_until_parked();
		let f = shared
			.text_fit("text-fit:t:subject")
			.expect("the text was recorded");
		assert!((f.drawn - 100.).abs() < 0.5, "{f:?}");
		assert!((f.visible - 40.).abs() < 0.5, "{f:?}");
		assert!(f.clipped, "{f:?}");
		assert!(!f.drifts(), "{f:?}");
		assert_eq!(shared.text_fit_clipped(), ["text-fit:t:subject"]);
		assert!(shared.text_fit_drift().is_empty());
	}

	/// A padded chip as the last on a bar that clips: its box ends where the
	/// bar does.
	struct LastOnTheBar {
		probes: Option<Probes>,
	}

	impl Render for LastOnTheBar {
		fn render(
			&mut self,
			window: &mut Window,
			_: &mut Context<Self>,
		) -> impl IntoElement {
			div()
				.size_full()
				.child(
					div()
						.w(px(200.))
						.flex()
						.flex_row()
						.overflow_hidden()
						.child(div().w(px(150.)).flex_shrink_0())
						.child(
							FitCell::new(
								FitId::new("t", FitRole::ChipMore),
								"ab",
							)
							.pad(6.)
							.fixed_width(50.)
							.render(&self.probes, window),
						),
				)
				.children(probe_frame_end(&self.probes))
		}
	}

	/// #229: a cell's probe covers the cell's own box: padding does not move
	/// it, so the last chip of a bar is not reported clipped by the bar that
	/// holds it exactly.
	#[gpui::test]
	fn a_padded_cell_is_probed_where_it_is_drawn(cx: &mut TestAppContext) {
		let probes = Probes::for_test();
		let shared = probes.clone();
		let (_, cx) = cx.add_window_view(|_, _| LastOnTheBar {
			probes: Some(probes),
		});
		cx.run_until_parked();
		let f = shared
			.text_fit("text-fit:t:chip-more")
			.expect("the text was recorded");
		assert!((f.x - 150.).abs() < 0.5, "{f:?}");
		assert!((f.drawn - 50.).abs() < 0.5, "{f:?}");
		assert!((f.visible - 50.).abs() < 0.5, "{f:?}");
		assert!(!f.clipped && !f.drifts(), "{f:?}");
	}

	/// A cell whose caller adds a border after `render` (an active overflow
	/// chip's ring).
	struct Ringed {
		probes: Option<Probes>,
	}

	impl Render for Ringed {
		fn render(
			&mut self,
			window: &mut Window,
			_: &mut Context<Self>,
		) -> impl IntoElement {
			div()
				.size_full()
				.child(
					div().w(px(200.)).flex().flex_row().child(
						FitCell::new(FitId::new("t", FitRole::ChipMore), "ab")
							.pad(4.)
							.frame(1.)
							.fixed_width(60.)
							.render(&self.probes, window)
							.border_1(),
					),
				)
				.children(probe_frame_end(&self.probes))
		}
	}

	/// #229: the border a caller adds sits outside the probe's canvas, so
	/// `frame` adds it back: the probe's box is the solver's width, not two
	/// pixels less (which `TEXT_FIT_DRIFT` reports).
	#[gpui::test]
	fn a_ringed_cell_is_probed_with_its_border(cx: &mut TestAppContext) {
		let probes = Probes::for_test();
		let shared = probes.clone();
		let (_, cx) = cx.add_window_view(|_, _| Ringed {
			probes: Some(probes),
		});
		cx.run_until_parked();
		let f = shared
			.text_fit("text-fit:t:chip-more")
			.expect("the text was recorded");
		assert!(f.x.abs() < 0.5, "{f:?}");
		assert!((f.drawn - 60.).abs() < 0.5, "{f:?}");
		assert!(!f.clipped && !f.drifts(), "{f:?}");
	}

	/// How a test box dresses the cell it holds.
	#[derive(Clone, Copy)]
	enum Dress {
		/// An ancestor that sets a weight and a size of its own.
		Ancestor,
		/// A weight put on the box after `render`: the way the viewer's own
		/// author used to be drawn.
		AfterRender,
		/// The cell told it is the viewer's own.
		Own,
		/// Nothing.
		Plain,
	}

	struct Dressed {
		probes: Option<Probes>,
		dress: Dress,
	}

	impl Render for Dressed {
		fn render(
			&mut self,
			window: &mut Window,
			_: &mut Context<Self>,
		) -> impl IntoElement {
			let cell = FitCell::new(
				FitId::new("t", FitRole::Author),
				"Alexandra Konstantinopoulou",
			)
			.fixed_width(80.);
			let cell = match self.dress {
				Dress::Own => cell.own(true),
				_ => cell,
			};
			let drawn = cell.render(&self.probes, window);
			let drawn = match self.dress {
				Dress::AfterRender => drawn.font_weight(FontWeight::SEMIBOLD),
				_ => drawn,
			};
			let holder = div().w(px(200.));
			let holder = match self.dress {
				Dress::Ancestor => {
					holder.font_weight(FontWeight::BOLD).text_size(px(30.))
				}
				_ => holder,
			};
			div()
				.size_full()
				.child(holder.child(drawn))
				.children(probe_frame_end(&self.probes))
		}
	}

	fn dressed(cx: &mut TestAppContext, dress: Dress) -> TextFit {
		let probes = Probes::for_test();
		let shared = probes.clone();
		let (_, cx) = cx.add_window_view(|_, _| Dressed {
			probes: Some(probes),
			dress,
		});
		cx.run_until_parked();
		shared
			.text_fit("text-fit:t:author")
			.expect("the text was recorded")
	}

	/// A cell pins its role's weight and size on its box: nothing an
	/// ancestor sets reaches the text, so the floor measured in the role's
	/// style is the text's.
	#[gpui::test]
	fn a_cell_is_drawn_in_its_roles_style_whatever_an_ancestor_sets(
		cx: &mut TestAppContext,
	) {
		for dress in [Dress::Plain, Dress::Ancestor] {
			let f = dressed(cx, dress);
			assert!(!f.style_drifts(), "{f:?}");
			assert_eq!(f.style, FitRole::Author.style());
		}
	}

	/// The viewer's own name is drawn in the weight the cell was told, and
	/// the probe measures and counts in that weight.
	#[gpui::test]
	fn an_own_cell_is_drawn_and_probed_in_semibold(cx: &mut TestAppContext) {
		let f = dressed(cx, Dress::Own);
		assert!(!f.style_drifts(), "{f:?}");
		assert_eq!(f.style.weight, FontWeight::SEMIBOLD);
		assert_eq!(f.style, FitRole::Author.style_for(true));
	}

	/// A weight added to the box after `render` is no longer a silent
	/// mismatch between what is measured and what is drawn: the probe reads
	/// what gpui draws and says so.
	#[gpui::test]
	fn a_weight_put_on_the_box_after_render_is_a_style_drift(
		cx: &mut TestAppContext,
	) {
		let f = dressed(cx, Dress::AfterRender);
		assert!(f.style_drifts(), "{f:?}");
		assert!(
			f.style_drift
				.as_deref()
				.is_some_and(|d| d.ends_with("/600")),
			"{f:?}"
		);
		assert!(f
			.style_drift_line("text-fit:t:author")
			.starts_with("[APP:TEXT_FIT_STYLE_DRIFT: id=text-fit:t:author"));
	}
}
