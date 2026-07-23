//! Native TikZ (LaTeX `tikzpicture`) renderer — a sibling of [`crate::renderer::SvgRender`].
//!
//! Unlike SVG, TikZ is not a lossless superset of Graphite's model. This renderer
//! implements the core geometry (Bézier paths, affine transforms, solid fills/strokes,
//! 2-stop gradients, opacity, dash, even-odd fill rule) and **degrades gracefully** for
//! features TikZ cannot represent (raster images, blend modes, clips, multi-stop gradients,
//! non-centered stroke alignment) by emitting `% …` comments. See `docs/adr/ADR-005-tikz-export.md`.

use core_types::color::{Color, SRGBA8};
use glam::DAffine2;
use kurbo::{Affine, BezPath, PathEl};
use std::fmt::Write;

/// Mutable state used whilst rendering to TikZ.
///
/// Transforms and opacity are accumulated on stacks and **baked into the path
/// coordinates** (mirroring how `SvgRender` bakes `applied_stroke_transform`). This
/// keeps the emitted TikZ free of `cm`/scope-transform fiddliness and correctly
/// handles translate/rotate/scale/skew. Because coordinates are baked, the complete LaTeX
/// document wrapper is applied exactly once (see [`TikzRenderOutput::to_document`]) — never
/// during the intermediate phase, so we never nest `tikzpicture` environments and the
/// exported `.tex` compiles standalone.
pub struct TikzRender {
	pub tikz: Vec<String>,
	transform_stack: Vec<DAffine2>,
	opacity_stack: Vec<f64>,
	/// Union of every recorded extent (in baked document coordinates, 1 unit = 1pt in the
	/// exported `tikzpicture`). Used by [`TikzRenderOutput::to_document`] to scale the picture
	/// down to fit the page when the artwork is larger than the page — e.g. a 1920×1080
	/// artboard at `x=1pt,y=1pt` would otherwise overflow A4 and force a blank leading page.
	/// `None` until the first [`TikzRender::expand_rect`] call.
	bounds: Option<(f64, f64, f64, f64)>, // (min_x, min_y, max_x, max_y)
}

impl TikzRender {
	pub fn new() -> Self {
		Self {
			tikz: Vec::new(),
			transform_stack: vec![DAffine2::IDENTITY],
			opacity_stack: vec![1.],
			bounds: None,
		}
	}

	/// Emit a raw TikZ line (one path / rect command).
	pub fn push(&mut self, line: impl Into<String>) {
		self.tikz.push("\t".to_string());
		self.tikz.push(line.into());
		self.tikz.push('\n'.to_string());
	}

	/// Emit a degradation note as a TikZ comment (may be multi-line).
	pub fn comment(&mut self, msg: &str) {
		for line in msg.lines() {
			self.tikz.push(format!("\t% {line}\n"));
		}
	}

	pub fn push_transform(&mut self, transform: DAffine2) {
		let current = *self.transform_stack.last().unwrap();
		self.transform_stack.push(current * transform);
	}

	pub fn pop_transform(&mut self) {
		self.transform_stack.pop();
	}

	pub fn current_transform(&self) -> DAffine2 {
		*self.transform_stack.last().unwrap()
	}

	pub fn push_opacity(&mut self, opacity: f64) {
		let current = *self.opacity_stack.last().unwrap();
		self.opacity_stack.push(current * opacity);
	}

	pub fn pop_opacity(&mut self) {
		self.opacity_stack.pop();
	}

	pub fn current_opacity(&self) -> f64 {
		*self.opacity_stack.last().unwrap()
	}

	/// Record an axis-aligned rectangle (in baked document coordinates) so the exported
	/// `tikzpicture` can be scaled to fit the page. Multiple calls union their extents,
	/// so a document with several artboards accumulates the full picture bounds.
	///
	/// Only the artboard background rectangles are currently recorded — they dominate the
	/// picture extent and are what overflow the page in practice. Content that overflows an
	/// artboard is a rare edge case this does not shrink for.
	pub fn expand_rect(&mut self, x0: f64, y0: f64, x1: f64, y1: f64) {
		let (min_x, min_y, max_x, max_y) = (x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1));
		self.bounds = Some(match self.bounds {
			Some((a, b, c, d)) => (a.min(min_x), b.min(min_y), c.max(max_x), d.max(max_y)),
			None => (min_x, min_y, max_x, max_y),
		});
	}
}

impl Default for TikzRender {
	fn default() -> Self {
		Self::new()
	}
}

/// Owned result of a TikZ render, mirroring [`crate::renderer::SvgRenderOutput`].
///
/// `tikz` holds the **raw body** (path commands). The complete, compilable LaTeX document
/// (with the `yscale=-1` flip that makes Graphite's y-down coordinates render upright) is
/// assembled exactly once by [`TikzRenderOutput::to_document`], which the render node calls
/// after the two-phase render — never during the intermediate phase — so the exported `.tex`
/// file compiles standalone (e.g. with `tectonic file.tex`).
pub struct TikzRenderOutput {
	pub tikz: String,
	/// Union bounding box of the artwork in Graphite/document units (1 unit = 1pt in the
	/// exported `tikzpicture`). `None` when no bounds were recorded (e.g. artwork that never
	/// emits an artboard background rectangle).
	pub bounds: Option<(f64, f64, f64, f64)>,
}

impl TikzRenderOutput {
	/// Assemble the raw TikZ body into a complete, compilable standalone LaTeX document.
	///
	/// Graphite artwork coordinates are emitted verbatim; the TikZ base unit is set to
	/// `1pt` per Graphite unit (treating them as points) so typical artwork renders at a
	/// sensible size rather than roughly 100 cm. The `yscale=-1` scope flips Graphite's
	/// y-down coordinates upright.
	///
	/// **Page-fit scaling.** A `tikzpicture` is an unbreakable box: if its bounding box
	/// exceeds the page's text area, LaTeX shifts the *entire* picture to the next page,
	/// leaving a blank leading page. To avoid that, when the recorded artwork bounds
	/// (see [`TikzRender::expand_rect`]) are larger than a conservative A4/Letter text
	/// area we attach `scale=<s>` to the `tikzpicture` so the picture shrinks to fit.
	/// We only ever shrink (the scale is capped at 1.0), so artwork smaller than the page
	/// keeps its natural size. No extra LaTeX package is required, preserving standalone
	/// compilation under `tectonic` / `pdflatex`. The result compiles with
	/// `tectonic file.tex` / `pdflatex`.
	pub fn to_document(&self) -> String {
		// Conservative text-area bounds in points. `\documentclass{article}` (10pt) yields a
		// text area of roughly 345×550pt regardless of whether the engine defaults to A4 or
		// Letter; these constants sit safely inside both so the scaled picture fits on one page
		// without an "Overfull \hbox/\vbox" warning.
		const TEXT_WIDTH_PT: f64 = 340.0;
		const TEXT_HEIGHT_PT: f64 = 540.0;

		let scale_attr = self
			.bounds
			.and_then(|(min_x, min_y, max_x, max_y)| {
				let w = (max_x - min_x).abs();
				let h = (max_y - min_y).abs();
				if w <= 0.0 || h <= 0.0 {
					return None;
				}
				let s = (TEXT_WIDTH_PT / w).min(TEXT_HEIGHT_PT / h).min(1.0);
				(s < 1.0).then_some(s)
			})
			.map(|s| format!(",scale={s:.4}"))
			.unwrap_or_default();

		format!(
			"% TikZ export generated by Graphite\n\
			 \\documentclass{{article}}\n\
			 \\usepackage{{tikz}}\n\
			 \\begin{{document}}\n\
			 \\centering\n\
			 \\begin{{tikzpicture}}[x=1pt,y=1pt{scale_attr}]\n\
			 \\begin{{scope}}[yscale=-1]\n\
			 {}\\end{{scope}}\n\
			 \\end{{tikzpicture}}\n\
			 \\end{{document}}\n",
			self.tikz
		)
	}
}

impl From<TikzRender> for TikzRenderOutput {
	fn from(val: TikzRender) -> Self {
		Self { tikz: val.tikz.concat(), bounds: val.bounds }
	}
}

/// Convert a Graphite linear-light [`Color`] to a TikZ `rgb,255:…` color spec (sRGB gamma-encoded).
pub fn tikz_color(color: Color) -> String {
	let SRGBA8 { red, green, blue, .. } = color.into();
	format!("{{rgb,255:red,{red};green,{green};blue,{blue}}}")
}

/// Convert a kurbo [`BezPath`] (already in the target coordinate space) to a TikZ path body.
///
/// Maps [`PathEl::MoveTo`] → coordinate, [`PathEl::LineTo`] → `--`,
/// [`PathEl::QuadTo`] → `.. controls ..`, [`PathEl::CurveTo`] → `.. controls .. and ..`,
/// and [`PathEl::ClosePath`] → `-- cycle`.
pub fn bezpath_to_tikz(path: &BezPath) -> String {
	let mut s = String::new();
	for el in path.elements() {
		match el {
			PathEl::MoveTo(p) => write!(s, "({:.3},{:.3}) ", p.x, p.y).unwrap(),
			PathEl::LineTo(p) => write!(s, "-- ({:.3},{:.3}) ", p.x, p.y).unwrap(),
			PathEl::QuadTo(c, p) => write!(s, ".. controls ({:.3},{:.3}) .. ({:.3},{:.3}) ", c.x, c.y, p.x, p.y).unwrap(),
			PathEl::CurveTo(c1, c2, p) => write!(s, ".. controls ({:.3},{:.3}) and ({:.3},{:.3}) .. ({:.3},{:.3}) ", c1.x, c1.y, c2.x, c2.y, p.x, p.y).unwrap(),
			PathEl::ClosePath => write!(s, "-- cycle ").unwrap(),
		}
	}
	s
}

/// Helper so callers can apply a baked [`DAffine2`] to a [`BezPath`] using kurbo's affine form.
pub fn apply_affine_to_bezpath(path: &mut BezPath, transform: DAffine2) {
	path.apply_affine(Affine::new(transform.to_cols_array()));
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A 1920×1080 artboard (the reported blank-page case) must be scaled down so it fits
	/// on one page rather than overflowing A4 and forcing a blank leading page.
	#[test]
	fn oversized_artboard_is_scaled_to_fit_page() {
		let mut render = TikzRender::new();
		// Mirror `Artboard::render_tikz`: record the background rectangle bounds.
		render.expand_rect(0.0, 0.0, 1920.0, 1080.0);
		let doc = TikzRenderOutput::from(render).to_document();

		// The scale must shrink the picture (1920pt > 430pt text width → scale ≈ 0.224).
		let scale: f64 = doc
			.lines()
			.find_map(|line| {
				let rest = line.trim().strip_prefix("\\begin{tikzpicture}[x=1pt,y=1pt,scale=")?;
				rest.trim_end_matches(']').parse::<f64>().ok()
			})
			.expect("expected a scale=<s> option on the tikzpicture for an oversized artboard");
		assert!((0.16..0.19).contains(&scale), "scale {scale} should sit near 340/1920 ≈ 0.177");

		// The y-flip scope and standalone wrapper must be preserved.
		assert!(doc.contains("\\begin{scope}[yscale=-1]"), "y-flip scope must be preserved");
		assert!(doc.contains("\\begin{document}"), "standalone wrapper must be preserved");
	}

	/// Artwork smaller than the page must keep its natural size (no up-scaling).
	#[test]
	fn small_artwork_is_not_scaled_up() {
		let mut render = TikzRender::new();
		render.expand_rect(0.0, 0.0, 100.0, 100.0);
		let doc = TikzRenderOutput::from(render).to_document();

		assert!(!doc.contains(",scale="), "small artwork must not be scaled up; got:\n{doc}");
		assert!(doc.contains("\\begin{tikzpicture}[x=1pt,y=1pt]"), "natural-size tikzpicture options must be unchanged");
	}

	/// Artwork that records no bounds (e.g. a bare graphic list, as in the renderer test)
	/// must produce identical output to before the page-fit change.
	#[test]
	fn no_bounds_means_no_scale_and_unchanged_output() {
		let render = TikzRender::new();
		let doc = TikzRenderOutput::from(render).to_document();

		assert!(!doc.contains(",scale="), "with no bounds there must be no scale option");
		assert!(doc.contains("\\begin{tikzpicture}[x=1pt,y=1pt]"), "default tikzpicture options must be unchanged");
		assert!(doc.contains("\\centering"), "centering should still be emitted");
	}
}
