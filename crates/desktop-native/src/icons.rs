//! Small vector icons painted with GPUI paths, so no icon font, emoji or
//! missing-glyph box is ever shown.

use gpui::{
	canvas, point, prelude::*, px, rgb, Bounds, PathBuilder, Pixels, Window,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Icon {
	File,
	Folder,
	FolderOpen,
	Repo,
	Branch,
	Commit,
	Search,
	Changes,
	Log,
	ChevronRight,
	ChevronDown,
	Diff,
	Warning,
	Check,
	Plus,
	Refresh,
	Back,
	SelectAll,
	SelectNone,
	More,
	Clear,
}

/// A `size`×`size` icon element in `color`.
pub fn icon(kind: Icon, size: f32, color: u32) -> impl IntoElement {
	canvas(
		|_, _, _| {},
		move |b, _, window, _| paint(window, kind, b, color),
	)
	.flex_shrink_0()
	.size(px(size))
}

fn p(b: &Bounds<Pixels>, x: f32, y: f32) -> gpui::Point<Pixels> {
	// Coordinates are in a 16×16 design grid scaled to the bounds.
	let s = f32::from(b.size.width) / 16.0;
	point(b.origin.x + px(x * s), b.origin.y + px(y * s))
}

fn stroke(
	window: &mut Window,
	b: &Bounds<Pixels>,
	color: u32,
	pts: &[(f32, f32)],
) {
	let s = f32::from(b.size.width) / 16.0;
	let mut pb = PathBuilder::stroke(px(1.3 * s.max(0.8)));
	let mut it = pts.iter();
	if let Some(&(x, y)) = it.next() {
		pb.move_to(p(b, x, y));
	}
	for &(x, y) in it {
		pb.line_to(p(b, x, y));
	}
	if let Ok(path) = pb.build() {
		window.paint_path(path, rgb(color));
	}
}

fn filled(
	window: &mut Window,
	b: &Bounds<Pixels>,
	color: u32,
	pts: &[(f32, f32)],
) {
	let mut pb = PathBuilder::fill();
	let mut it = pts.iter();
	if let Some(&(x, y)) = it.next() {
		pb.move_to(p(b, x, y));
	}
	for &(x, y) in it {
		pb.line_to(p(b, x, y));
	}
	pb.close();
	if let Ok(path) = pb.build() {
		window.paint_path(path, rgb(color));
	}
}

fn circle(
	window: &mut Window,
	b: &Bounds<Pixels>,
	color: u32,
	cx: f32,
	cy: f32,
	r: f32,
	fill_it: bool,
) {
	let steps = 12;
	let pts: Vec<(f32, f32)> = (0..=steps)
		.map(|i| {
			let a = i as f32 / steps as f32 * std::f32::consts::TAU;
			(cx + r * a.cos(), cy + r * a.sin())
		})
		.collect();
	if fill_it {
		filled(window, b, color, &pts);
	} else {
		stroke(window, b, color, &pts);
	}
}

fn paint(window: &mut Window, kind: Icon, b: Bounds<Pixels>, color: u32) {
	let b = &b;
	match kind {
		Icon::File => {
			// Page with a folded corner.
			stroke(
				window,
				b,
				color,
				&[
					(4., 2.),
					(10., 2.),
					(13., 5.),
					(13., 14.),
					(4., 14.),
					(4., 2.),
				],
			);
			stroke(window, b, color, &[(10., 2.), (10., 5.), (13., 5.)]);
		}
		Icon::Folder => {
			filled(
				window,
				b,
				color,
				&[
					(1.5, 4.),
					(6., 4.),
					(7.5, 5.5),
					(14.5, 5.5),
					(14.5, 13.),
					(1.5, 13.),
				],
			);
		}
		Icon::FolderOpen => {
			filled(
				window,
				b,
				color,
				&[
					(1.5, 4.),
					(6., 4.),
					(7.5, 5.5),
					(13., 5.5),
					(13., 7.),
					(1.5, 7.),
				],
			);
			filled(
				window,
				b,
				color,
				&[(3., 8.), (15., 8.), (13., 13.), (1.5, 13.)],
			);
		}
		Icon::Repo => {
			stroke(
				window,
				b,
				color,
				&[(3., 2.), (13., 2.), (13., 14.), (3., 14.), (3., 2.)],
			);
			stroke(window, b, color, &[(6., 5.), (10., 5.)]);
			stroke(window, b, color, &[(6., 8.), (10., 8.)]);
		}
		Icon::Branch => {
			circle(window, b, color, 5., 3.5, 1.8, false);
			circle(window, b, color, 5., 12.5, 1.8, false);
			circle(window, b, color, 11.5, 5.5, 1.8, false);
			stroke(window, b, color, &[(5., 5.3), (5., 10.7)]);
			stroke(window, b, color, &[(11.5, 7.3), (11.5, 8.5), (5.5, 10.5)]);
		}
		Icon::Commit => {
			circle(window, b, color, 8., 8., 3., false);
			stroke(window, b, color, &[(1., 8.), (5., 8.)]);
			stroke(window, b, color, &[(11., 8.), (15., 8.)]);
		}
		Icon::Search => {
			circle(window, b, color, 6.5, 6.5, 4., false);
			stroke(window, b, color, &[(9.5, 9.5), (14., 14.)]);
		}
		Icon::Changes => {
			stroke(window, b, color, &[(2., 4.), (14., 4.)]);
			stroke(window, b, color, &[(2., 8.), (10., 8.)]);
			stroke(window, b, color, &[(2., 12.), (14., 12.)]);
		}
		Icon::Log => {
			circle(window, b, color, 5., 3.5, 1.8, true);
			circle(window, b, color, 5., 12.5, 1.8, true);
			circle(window, b, color, 11., 8., 1.8, true);
			stroke(window, b, color, &[(5., 5.), (5., 11.)]);
			stroke(window, b, color, &[(5., 5.), (11., 6.5)]);
		}
		Icon::ChevronRight => {
			stroke(window, b, color, &[(6., 4.), (10., 8.), (6., 12.)])
		}
		Icon::ChevronDown => {
			stroke(window, b, color, &[(4., 6.), (8., 10.), (12., 6.)])
		}
		Icon::Diff => {
			stroke(window, b, color, &[(3., 5.), (7., 5.)]);
			stroke(window, b, color, &[(5., 3.), (5., 7.)]);
			stroke(window, b, color, &[(9., 11.), (13., 11.)]);
			stroke(window, b, color, &[(4., 14.), (12., 2.)]);
		}
		Icon::Warning => {
			stroke(
				window,
				b,
				color,
				&[(8., 2.), (15., 14.), (1., 14.), (8., 2.)],
			);
			stroke(window, b, color, &[(8., 6.), (8., 10.)]);
			stroke(window, b, color, &[(8., 11.8), (8., 12.4)]);
		}
		Icon::Check => {
			stroke(window, b, color, &[(3.5, 8.5), (6.5, 11.5), (12.5, 4.5)])
		}
		Icon::Plus => {
			stroke(window, b, color, &[(8., 3.), (8., 13.)]);
			stroke(window, b, color, &[(3., 8.), (13., 8.)]);
		}
		Icon::Refresh => {
			// Open circular arrow with an arrowhead at the top-right end.
			let pts: Vec<(f32, f32)> = (0..=10)
				.map(|i| {
					let a = -1.0 + i as f32 / 10.0 * 5.0;
					(8. + 5. * a.cos(), 8. + 5. * a.sin())
				})
				.collect();
			stroke(window, b, color, &pts);
			stroke(window, b, color, &[(13.2, 1.8), (12.7, 3.8), (10.6, 3.2)]);
		}
		Icon::Back => {
			stroke(window, b, color, &[(7., 3.), (2., 8.), (7., 13.)]);
			stroke(window, b, color, &[(2., 8.), (14., 8.)]);
		}
		Icon::SelectAll => {
			stroke(
				window,
				b,
				color,
				&[
					(2.5, 2.5),
					(13.5, 2.5),
					(13.5, 13.5),
					(2.5, 13.5),
					(2.5, 2.5),
				],
			);
			stroke(window, b, color, &[(5., 8.), (7., 10.5), (11., 5.5)]);
		}
		Icon::SelectNone => {
			stroke(
				window,
				b,
				color,
				&[
					(2.5, 2.5),
					(13.5, 2.5),
					(13.5, 13.5),
					(2.5, 13.5),
					(2.5, 2.5),
				],
			);
			stroke(window, b, color, &[(5., 8.), (11., 8.)]);
		}
		Icon::More => {
			// "Load more": arrow down onto a baseline.
			stroke(window, b, color, &[(8., 2.), (8., 10.)]);
			stroke(window, b, color, &[(4.5, 6.5), (8., 10.), (11.5, 6.5)]);
			stroke(window, b, color, &[(3., 13.5), (13., 13.5)]);
		}
		Icon::Clear => {
			stroke(window, b, color, &[(4., 4.), (12., 12.)]);
			stroke(window, b, color, &[(12., 4.), (4., 12.)]);
		}
	}
}
