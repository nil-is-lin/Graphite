//! Inline LaTeX math baking for the Text node (P2: real engine with graceful fallback).
//!
//! Pipeline (see ADR-006): `parse_text_spans` splits the content into `Plain` / `InlineMath`
//! / `DisplayMath` spans; math spans are baked into vector geometry; the spans are then
//! concatenated back into the reading flow by a lightweight flow concatenator that advances
//! `x` between spans and forces `DisplayMath` onto its own line.
//!
//! Engine selection (runtime, behind the single `bake_math_span` seam):
//! - web / desktop webview → MathJax (frontend, feeds the cache via `cache_baked_math_svg`).
//! - native (headless CLI) with `typst` on PATH → `typst` + `@preview/mitex` subprocess
//!   (`typst compile --format svg`), parsed to `Vector` here in Rust.
//! - otherwise → placeholder bake (raw LaTeX source shaped as ordinary text); never panics.
//!
//! All branches converge on the same `Vector` output, so `math_bake` / the concatenator / the
//! three renderers (`SvgRender` / `TikzRender` / raster) are untouched regardless of engine.

use super::span::{parse_text_spans, TextSpan};
use super::{TypesettingConfig, bounding_box, shape_text_item};
use core_types::blending::BlendMode;
use core_types::list::{Item, List, NodeIdPath};
use core_types::{
	ATTR_BLEND_MODE, ATTR_EDITOR_LAYER_PATH, ATTR_FONT, ATTR_FONT_SIZE, ATTR_LETTER_SPACING, ATTR_LETTER_TILT, ATTR_LINE_HEIGHT, ATTR_MAX_HEIGHT, ATTR_MAX_WIDTH, ATTR_OPACITY,
	ATTR_OPACITY_FILL, ATTR_TEXT_ALIGN, ATTR_TRANSFORM,
};
use glam::{DAffine2, DVec2};
use graphene_resource::Resource;
use std::collections::HashMap;
use std::fmt::Write;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use vector_types::kurbo::{BezPath, Point};
use vector_types::Vector;

/// Derived bake cache: `source` → the SVG path `d` strings produced by the bake engine
/// (KaTeX on the frontend, or `typst`+`mitex` natively). The raw paths are engine-agnostic and
/// font-size-independent; `bake_math_span` normalizes them to a `Vector` at the *requested*
/// font size on each use, so one entry serves every text node regardless of its font size (this
/// also fixes the pre-refactor bug where geometry was cached at the first-seen font size only).
/// Populated by the native `typst` bake and by the frontend KaTeX feed (`cache_baked_math_svg`),
/// and persisted into the document via `export_cache`/`import_cache` (P4).
static MATH_CACHE: OnceLock<Mutex<HashMap<String, Vec<String>>>> = OnceLock::new();

fn math_cache() -> &'static Mutex<HashMap<String, Vec<String>>> {
	MATH_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Shapes a text item that may contain LaTeX math (`$...$` / `$$...$$`) into vector geometry,
/// splicing baked math spans back into the reading flow.
///
/// Mirrors `shape_text_item`'s contract: it reads the item's font and typesetting attributes,
/// applies the item's transform / blending / opacity to the produced vectors, and returns one
/// `List<Vector>` per shaped unit (`separate_glyphs` controls per-glyph splitting of plain text).
///
/// Fast path: text without `$` is delegated straight to `shape_text_item` — zero behavior change
/// and zero parsing cost for ordinary text.
pub fn shape_text_item_with_math(item: &Item<String>, separate_glyphs: bool) -> List<Vector> {
	let text = item.element();
	if !text.contains('$') {
		return shape_text_item(item, separate_glyphs);
	}

	// Outer attributes applied once to the concatenated result (spans are shaped without them).
	let transform = item.attribute_cloned_or_default::<DAffine2>(ATTR_TRANSFORM);
	let layer_path = item.attribute::<NodeIdPath>(ATTR_EDITOR_LAYER_PATH).cloned();
	let blend_mode = item.attribute::<BlendMode>(ATTR_BLEND_MODE).copied();
	let opacity = item.attribute::<f64>(ATTR_OPACITY).copied();
	let opacity_fill = item.attribute::<f64>(ATTR_OPACITY_FILL).copied();

	let line_advance = {
		let (_, typesetting) = read_typographic(item);
		typesetting.font_size * typesetting.line_height_ratio
	};

	let spans = parse_text_spans(text);
	let mut result: Vec<Item<Vector>> = Vec::new();
	let mut cursor_x = 0.0;
	let mut line_y = 0.0;

	for span in spans {
		match span {
			TextSpan::Plain(content) => {
				let (mut shaped, width) = shape_span(item, &content, separate_glyphs);
				translate_all(&mut shaped, cursor_x, line_y);
				result.append(&mut shaped);
				cursor_x += width;
			}
			TextSpan::InlineMath(content) => {
				let (mut shaped, width) = bake_math_span(item, &content);
				translate_all(&mut shaped, cursor_x, line_y);
				result.append(&mut shaped);
				cursor_x += width;
			}
			TextSpan::DisplayMath(content) => {
				// Force the display math onto its own line: advance to a new line, place it, then
				// advance again so following text starts on the next line. (v1: left-aligned;
				// centering is a later refinement.)
				line_y += line_advance;
				let (mut shaped, _) = bake_math_span(item, &content);
				translate_all(&mut shaped, 0.0, line_y);
				result.append(&mut shaped);
				line_y += line_advance;
				cursor_x = 0.0;
			}
		}
	}

	// Apply outer attributes once, mirroring `shape_text_item`.
	for item in &mut result {
		if transform != DAffine2::IDENTITY {
			let local = item.attribute_cloned_or_default::<DAffine2>(ATTR_TRANSFORM);
			item.set_attribute(ATTR_TRANSFORM, transform * local);
		}
		if let Some(layer_path) = &layer_path {
			item.set_attribute(ATTR_EDITOR_LAYER_PATH, layer_path.clone());
		}
		if let Some(blend_mode) = blend_mode {
			item.set_attribute(ATTR_BLEND_MODE, blend_mode);
		}
		if let Some(opacity) = opacity {
			item.set_attribute(ATTR_OPACITY, opacity);
		}
		if let Some(opacity_fill) = opacity_fill {
			item.set_attribute(ATTR_OPACITY_FILL, opacity_fill);
		}
	}

	let mut list = List::new();
	for item in result {
		list.push(item);
	}
	list
}

/// Bakes a math span's source into vector geometry, returning the geometry plus its horizontal
/// advance width (used by the concatenator to advance `x`).
///
/// Engine order (see ADR-006): cache → native `typst`+`mitex` subprocess → placeholder (source
/// shaped as text). The signature is the integration seam; swapping engines never touches callers.
pub fn bake_math_span(item: &Item<String>, source: &str) -> (Vec<Item<Vector>>, f64) {
	let font_size = {
		let (_, typesetting) = read_typographic(item);
		typesetting.font_size.max(1.0)
	};
	// Cache hit: normalize the stored raw SVG paths to this node's font size.
	if let Some(paths) = math_cache().lock().unwrap().get(source).cloned()
		&& let Some(normalized) = normalize_paths(&paths, font_size)
	{
		return normalized;
	}
	// Cache miss: try the native `typst`+`mitex` bake, store the raw paths, then normalize.
	if let Some(paths) = typst_mitex_bake(source) {
		math_cache().lock().unwrap().insert(source.to_string(), paths.clone());
		if let Some(normalized) = normalize_paths(&paths, font_size) {
			return normalized;
		}
	}
	// Fallback: shape the raw LaTeX source as ordinary text. Always renders something.
	let (shaped, width) = shape_span(item, source, false);
	(shaped, width)
}

/// Normalizes parsed vector geometry into the span's local frame: left edge at x=0, scaled to
/// `font_size` tall, vertically centered near the x-height so it sits on the text line. The input
/// paths are already in SVG y-down space (whether from MathJax/KaTeX or native `typst`), which is
/// the SAME y-down convention `PathBuilder` uses for regular text (`origin.y - y`). We therefore
/// must NOT flip y here — flipping would mirror the glyphs upside-down relative to the rest of the
/// text. The y-axis is preserved; only x is scaled/translated and y is scaled/translated.
fn normalize_to_font_size(vectors: Vec<Vector>, font_size: f64) -> Option<(Vec<Item<Vector>>, f64)> {
	if vectors.is_empty() {
		return None;
	}
	let (mut min, mut max) = (DVec2::splat(f64::INFINITY), DVec2::splat(f64::NEG_INFINITY));
	for v in &vectors {
		if let Some([a, b]) = v.bounding_box() {
			min = min.min(a);
			max = max.max(b);
		}
	}
	if !min.is_finite() || !max.is_finite() {
		return None;
	}
	let height = (max.y - min.y).max(1e-6);
	let scale = font_size / height;
	let width = (max.x - min.x).max(1e-6) * scale;
	let center_y_graphite = scale * (-(min.y + max.y) / 2.0);
	let transform =
		DAffine2::from_translation(DVec2::new(-scale * min.x, 0.35 * font_size - center_y_graphite)) * DAffine2::from_scale(DVec2::new(scale, scale));
	let mut out = Vec::with_capacity(vectors.len());
	for mut v in vectors {
		v.transform(transform);
		out.push(Item::new_from_element(v));
	}
	Some((out, width))
}

/// Real bake via the `typst` CLI + `@preview/mitex`. Writes a tiny `.typ` document that imports
/// mitex and typesets the LaTeX source, compiles it to SVG, then extracts the SVG `<path d>` data.
/// Returns the raw path strings (engine-agnostic, font-size-independent); `bake_math_span`
/// normalizes them to a `Vector` at the requesting node's font size. Returns `None` if `typst` is
/// unavailable or the compile/path-extract fails — the caller then falls back to the placeholder.
fn typst_mitex_bake(source: &str) -> Option<Vec<String>> {
	if !typst_available() {
		return None;
	}

	let typ_path = unique_tmp("typ");
	let svg_path = unique_tmp("svg");
	let content = format!("#import \"@preview/mitex:0.2.7\": mi\n#mi(\"{}\")\n", escape_typst_string(source));
	if std::fs::write(&typ_path, content).is_err() {
		return None;
	}

	let status = Command::new(typst_binary()?).arg("compile").arg(&typ_path).arg(&svg_path).output();
	let _ = std::fs::remove_file(&typ_path);

	let svg = match status {
		Ok(out) if out.status.success() => std::fs::read_to_string(&svg_path).ok()?,
		_ => {
			let _ = std::fs::remove_file(&svg_path);
			return None;
		}
	};
	let _ = std::fs::remove_file(&svg_path);

	// Return the raw SVG path `d` strings; normalization to a `Vector` happens in
	// `bake_math_span` at the requesting node's font size (font-size-independent cache).
	let mut paths: Vec<String> = Vec::new();
	for d in extract_path_d(&svg) {
		paths.push(d);
	}
	if paths.is_empty() { None } else { Some(paths) }
}

/// Feed pre-baked geometry from the frontend KaTeX path (P2b). The frontend runs KaTeX, extracts
/// the SVG `<path d>` strings, and passes them here keyed by the LaTeX `source`. They are stored
/// as raw paths in the source-hash cache (font-size-independent); `bake_math_span` normalizes them
/// to a `Vector` at the requesting node's font size on use. Returns `false` if no path was
/// supplied.
pub fn cache_baked_math_svg(source: &str, _font_size: f64, svg_paths: &[String]) -> bool {
	if svg_paths.is_empty() {
		return false;
	}
	math_cache().lock().unwrap().insert(source.to_string(), svg_paths.to_vec());
	true
}

/// Parses SVG path `d` strings into `Vector`s, skipping any that fail to parse.
fn paths_to_vectors(paths: &[String]) -> Vec<Vector> {
	paths.iter().filter_map(|d| svg_path_to_bezpath(d).map(Vector::from_bezpath)).collect()
}

/// Normalizes raw SVG path `d` strings into the span's local frame at `font_size` (see
/// `normalize_to_font_size`), returning the geometry plus its horizontal advance width.
fn normalize_paths(paths: &[String], font_size: f64) -> Option<(Vec<Item<Vector>>, f64)> {
	let vectors = paths_to_vectors(paths);
	normalize_to_font_size(vectors, font_size)
}

/// Snapshots the entire source-hash bake cache (source → raw SVG path strings) for persistence
/// into a document (P4). The raw paths are font-size-independent, so they normalize correctly on
/// reload at whatever font size the consuming text node uses.
pub fn export_cache() -> HashMap<String, Vec<String>> {
	math_cache().lock().unwrap().clone()
}

/// Merges persisted bake entries (from a loaded document) back into the process-wide cache (P4).
/// Existing entries are preserved; only new sources are inserted, so concurrently loaded documents
/// do not clobber each other's entries.
pub fn import_cache(entries: &HashMap<String, Vec<String>>) {
	let mut cache = math_cache().lock().unwrap();
	for (source, paths) in entries {
		cache.entry(source.clone()).or_insert_with(|| paths.clone());
	}
}

/// Resolves the `typst` executable. When a packaged app (e.g. `Graphite.app`) is launched from
/// the Finder rather than a shell, `/opt/homebrew/bin` (homebrew) and `/usr/local/bin` are absent
/// from `PATH`, so a bare `Command::new("typst")` fails even though typst is installed. We probe
/// the common install locations first, then fall back to a `PATH` lookup.
fn typst_binary() -> Option<PathBuf> {
	for candidate in ["/opt/homebrew/bin/typst", "/usr/local/bin/typst", "/usr/bin/typst"] {
		if std::path::Path::new(candidate).exists() {
			return Some(PathBuf::from(candidate));
		}
	}
	// Fall back to PATH-based discovery.
	if Command::new("typst").arg("--version").output().map(|o| o.status.success()).unwrap_or(false) {
		return Some(PathBuf::from("typst"));
	}
	None
}

/// Cheap, cached probe for the `typst` CLI.
fn typst_available() -> bool {
	static AVAIL: OnceLock<Mutex<Option<bool>>> = OnceLock::new();
	let cell = AVAIL.get_or_init(|| Mutex::new(None));
	let mut guard = cell.lock().unwrap();
	if let Some(ok) = *guard {
		return ok;
	}
	let ok = typst_binary().is_some();
	*guard = Some(ok);
	ok
}

/// Extracts visible glyph path `d` strings from an SVG string using `usvg`.
///
/// This resolves `<use>` references, applies ancestor transforms, and skips invisible or
/// non-rendering subtrees (clip paths, masks, patterns). It also skips the full-page white
/// background rectangle emitted by `typst compile --format svg`, which would otherwise be
/// baked into a solid black box once the text layer's fill is applied.
fn extract_path_d(svg: &str) -> Vec<String> {
	let options = usvg::Options::default();
	let tree = match usvg::Tree::from_str(svg, &options) {
		Ok(tree) => tree,
		Err(_) => return Vec::new(),
	};
	let view_size = tree.size();
	let view_area = view_size.width() as f64 * view_size.height() as f64;
	let mut out = Vec::new();
	collect_paths(tree.root(), view_area, &mut out);
	out
}

/// Recursively collects path data from `usvg` nodes.
fn collect_paths(group: &usvg::Group, view_area: f64, out: &mut Vec<String>) {
	for node in group.children() {
		match node {
			usvg::Node::Path(path) => {
				if !path.is_visible() || !has_visible_paint(path) {
					continue;
				}
				// Skip the typst page-background rectangle.
				if is_background_rect(path, view_area) {
					continue;
				}
				if let Some(d) = path_to_svg_d(path) {
					out.push(d);
				}
			}
			usvg::Node::Group(group) => collect_paths(group, view_area, out),
			_ => {}
		}
	}
}

/// Detects the full-page white background rectangle emitted by `typst`.
fn is_background_rect(path: &usvg::Path, view_area: f64) -> bool {
	let fill = match path.fill() {
		Some(fill) => fill,
		None => return false,
	};
	let color = match fill.paint() {
		usvg::Paint::Color(c) => c,
		_ => return false,
	};
	if color.red != 255 || color.green != 255 || color.blue != 255 {
		return false;
	}
	let bbox = path.abs_bounding_box();
	let area = bbox.width() as f64 * bbox.height() as f64;
	area > view_area * 0.75
}

/// Returns `true` if the path has a renderable fill or stroke.
/// Invisible paths (e.g. a transparent page background with no paint) are ignored so they do
/// not distort the math bounding box.
fn has_visible_paint(path: &usvg::Path) -> bool {
	fn paint_is_visible(paint: &usvg::Paint, opacity: usvg::Opacity) -> bool {
		if opacity.get() <= 0.0 {
			return false;
		}
		// Gradients/patterns are always considered visible; colors are visible if opaque enough.
		match paint {
			usvg::Paint::Color(_) => opacity.get() > 1.0e-6,
			_ => true,
		}
	}
	if let Some(fill) = path.fill()
		&& paint_is_visible(fill.paint(), fill.opacity())
	{
		return true;
	}
	if let Some(stroke) = path.stroke()
		&& paint_is_visible(stroke.paint(), stroke.opacity())
		&& stroke.width().get() > 0.0
	{
		return true;
	}
	false
}

/// Serializes a `usvg` path (with its absolute transform already applied) to an SVG `d` string.
fn path_to_svg_d(path: &usvg::Path) -> Option<String> {
	let ts = path.abs_transform();
	let mut d = String::new();
	for seg in path.data().segments() {
		match seg {
			usvg::tiny_skia_path::PathSegment::MoveTo(p) => {
				let (x, y) = transform_point(ts, p);
				write!(&mut d, "M {x:.3} {y:.3} ").ok()?;
			}
			usvg::tiny_skia_path::PathSegment::LineTo(p) => {
				let (x, y) = transform_point(ts, p);
				write!(&mut d, "L {x:.3} {y:.3} ").ok()?;
			}
			usvg::tiny_skia_path::PathSegment::QuadTo(p1, p2) => {
				let (x1, y1) = transform_point(ts, p1);
				let (x2, y2) = transform_point(ts, p2);
				write!(&mut d, "Q {x1:.3} {y1:.3} {x2:.3} {y2:.3} ").ok()?;
			}
			usvg::tiny_skia_path::PathSegment::CubicTo(p1, p2, p3) => {
				let (x1, y1) = transform_point(ts, p1);
				let (x2, y2) = transform_point(ts, p2);
				let (x3, y3) = transform_point(ts, p3);
				write!(&mut d, "C {x1:.3} {y1:.3} {x2:.3} {y2:.3} {x3:.3} {y3:.3} ").ok()?;
			}
			usvg::tiny_skia_path::PathSegment::Close => {
				write!(&mut d, "Z ").ok()?;
			}
		}
	}
	// Trim trailing space; ignore degenerate paths.
	d.pop();
	if d.is_empty() { None } else { Some(d) }
}

/// Applies a `usvg` absolute transform to a `tiny-skia` point, faithfully (no extra flip).
///
/// `typst compile --format svg` embeds each glyph in a `matrix(1 0 0 -1 ...)` group, which
/// converts typst's typography-y-up outlines into SVG's y-down space. So after this function the
/// coordinates are y-down — the SAME convention as the MathJax/KaTeX SVG paths fed from the
/// frontend. `normalize_to_font_size` deliberately does NOT re-flip y, so baked math stays in
/// y-down space to match regular shaped text produced by `PathBuilder` (which emits y-down via
/// `origin.y - y`). Flipping y here would double-flip and mirror the glyphs.
fn transform_point(ts: usvg::Transform, p: usvg::tiny_skia_path::Point) -> (f64, f64) {
	let x = (ts.sx * p.x + ts.kx * p.y + ts.tx) as f64;
	let y = (ts.ky * p.x + ts.sy * p.y + ts.ty) as f64;
	(x, y)
}

/// Escapes a string for embedding inside a Typst string literal (`#mi("...")`).
fn escape_typst_string(s: &str) -> String {
	let mut out = String::with_capacity(s.len() + 2);
	for c in s.chars() {
		match c {
			'\\' => out.push_str("\\\\"),
			'"' => out.push_str("\\\""),
			other => out.push(other),
		}
	}
	out
}

/// Parses SVG path `d` data into a `kurbo::BezPath` using kurbo's core builder API (kurbo 0.13
/// has no `from_svg` feature, so we build it directly). Handles M/L/H/V/C/S/Q/T/A/Z in absolute
/// and relative form with implicit command repetition. Arc (`A`) is approximated by a straight
/// line to its endpoint in v1; quadratic (`Q`/`T`) is promoted to a cubic. Returns `None` on a
/// malformed path.
fn svg_path_to_bezpath(d: &str) -> Option<BezPath> {
	let chars: Vec<char> = d.chars().collect();
	let read_num = |chars: &[char], i: &mut usize| -> Option<f64> {
		while *i < chars.len() && (chars[*i].is_whitespace() || chars[*i] == ',') {
			*i += 1;
		}
		let start = *i;
		if *i >= chars.len() {
			return None;
		}
		if chars[*i] == '+' || chars[*i] == '-' {
			*i += 1;
		}
		while *i < chars.len() && chars[*i].is_ascii_digit() {
			*i += 1;
		}
		if *i < chars.len() && chars[*i] == '.' {
			*i += 1;
			while *i < chars.len() && chars[*i].is_ascii_digit() {
				*i += 1;
			}
		}
		if *i < chars.len() && (chars[*i] == 'e' || chars[*i] == 'E') {
			*i += 1;
			if *i < chars.len() && (chars[*i] == '+' || chars[*i] == '-') {
				*i += 1;
			}
			while *i < chars.len() && chars[*i].is_ascii_digit() {
				*i += 1;
			}
		}
		if *i == start {
			return None;
		}
		chars[start..*i].iter().collect::<String>().parse::<f64>().ok()
	};

	let mut b = BezPath::new();
	let mut i = 0usize;
	let mut cmd: Option<char> = None;
	let (mut cx, mut cy) = (0.0_f64, 0.0_f64);
	let (mut sx, mut sy) = (0.0_f64, 0.0_f64);
	let mut prev_c2: Option<(f64, f64)> = None;
	let mut prev_qc: Option<(f64, f64)> = None;

	loop {
		while i < chars.len() && (chars[i].is_whitespace() || chars[i] == ',') {
			i += 1;
		}
		if i >= chars.len() {
			break;
		}
		let letter = chars[i];
		if letter.is_ascii_alphabetic() {
			cmd = Some(letter);
			i += 1;
		} else if cmd.is_none() {
			return None;
		}
		let c = cmd?;
		match c {
			'M' => {
				let x = read_num(&chars, &mut i)?;
				let y = read_num(&chars, &mut i)?;
				b.move_to(Point::new(x, y));
				(cx, cy) = (x, y);
				(sx, sy) = (x, y);
				cmd = Some('L');
			}
			'm' => {
				let x = read_num(&chars, &mut i)?;
				let y = read_num(&chars, &mut i)?;
				let (nx, ny) = (cx + x, cy + y);
				b.move_to(Point::new(nx, ny));
				(cx, cy) = (nx, ny);
				(sx, sy) = (nx, ny);
				cmd = Some('l');
			}
			'L' => {
				let x = read_num(&chars, &mut i)?;
				let y = read_num(&chars, &mut i)?;
				b.line_to(Point::new(x, y));
				(cx, cy) = (x, y);
				prev_c2 = None;
				prev_qc = None;
			}
			'l' => {
				let x = read_num(&chars, &mut i)?;
				let y = read_num(&chars, &mut i)?;
				let (nx, ny) = (cx + x, cy + y);
				b.line_to(Point::new(nx, ny));
				(cx, cy) = (nx, ny);
				prev_c2 = None;
				prev_qc = None;
			}
			'H' => {
				let x = read_num(&chars, &mut i)?;
				b.line_to(Point::new(x, cy));
				cx = x;
				prev_c2 = None;
				prev_qc = None;
			}
			'h' => {
				let x = read_num(&chars, &mut i)?;
				let nx = cx + x;
				b.line_to(Point::new(nx, cy));
				cx = nx;
				prev_c2 = None;
				prev_qc = None;
			}
			'V' => {
				let y = read_num(&chars, &mut i)?;
				b.line_to(Point::new(cx, y));
				cy = y;
				prev_c2 = None;
				prev_qc = None;
			}
			'v' => {
				let y = read_num(&chars, &mut i)?;
				let ny = cy + y;
				b.line_to(Point::new(cx, ny));
				cy = ny;
				prev_c2 = None;
				prev_qc = None;
			}
			'C' => {
				let x1 = read_num(&chars, &mut i)?;
				let y1 = read_num(&chars, &mut i)?;
				let x2 = read_num(&chars, &mut i)?;
				let y2 = read_num(&chars, &mut i)?;
				let x = read_num(&chars, &mut i)?;
				let y = read_num(&chars, &mut i)?;
				b.curve_to(Point::new(x1, y1), Point::new(x2, y2), Point::new(x, y));
				prev_c2 = Some((x2, y2));
				prev_qc = None;
				(cx, cy) = (x, y);
			}
			'c' => {
				let x1 = read_num(&chars, &mut i)? + cx;
				let y1 = read_num(&chars, &mut i)? + cy;
				let x2 = read_num(&chars, &mut i)? + cx;
				let y2 = read_num(&chars, &mut i)? + cy;
				let x = read_num(&chars, &mut i)? + cx;
				let y = read_num(&chars, &mut i)? + cy;
				b.curve_to(Point::new(x1, y1), Point::new(x2, y2), Point::new(x, y));
				prev_c2 = Some((x2, y2));
				prev_qc = None;
				(cx, cy) = (x, y);
			}
			'S' => {
				let x2 = read_num(&chars, &mut i)?;
				let y2 = read_num(&chars, &mut i)?;
				let x = read_num(&chars, &mut i)?;
				let y = read_num(&chars, &mut i)?;
				let (x1, y1) = prev_c2.map(|(px, py)| (2.0 * cx - px, 2.0 * cy - py)).unwrap_or((cx, cy));
				b.curve_to(Point::new(x1, y1), Point::new(x2, y2), Point::new(x, y));
				prev_c2 = Some((x2, y2));
				prev_qc = None;
				(cx, cy) = (x, y);
			}
			's' => {
				let x2 = read_num(&chars, &mut i)? + cx;
				let y2 = read_num(&chars, &mut i)? + cy;
				let x = read_num(&chars, &mut i)? + cx;
				let y = read_num(&chars, &mut i)? + cy;
				let (x1, y1) = prev_c2.map(|(px, py)| (2.0 * cx - px, 2.0 * cy - py)).unwrap_or((cx, cy));
				b.curve_to(Point::new(x1, y1), Point::new(x2, y2), Point::new(x, y));
				prev_c2 = Some((x2, y2));
				prev_qc = None;
				(cx, cy) = (x, y);
			}
			'Q' => {
				let x1 = read_num(&chars, &mut i)?;
				let y1 = read_num(&chars, &mut i)?;
				let x = read_num(&chars, &mut i)?;
				let y = read_num(&chars, &mut i)?;
				let (c1x, c1y, c2x, c2y) = quad_to_cubic(cx, cy, x1, y1, x, y);
				b.curve_to(Point::new(c1x, c1y), Point::new(c2x, c2y), Point::new(x, y));
				prev_qc = Some((x1, y1));
				prev_c2 = None;
				(cx, cy) = (x, y);
			}
			'q' => {
				let x1 = read_num(&chars, &mut i)? + cx;
				let y1 = read_num(&chars, &mut i)? + cy;
				let x = read_num(&chars, &mut i)? + cx;
				let y = read_num(&chars, &mut i)? + cy;
				let (c1x, c1y, c2x, c2y) = quad_to_cubic(cx, cy, x1, y1, x, y);
				b.curve_to(Point::new(c1x, c1y), Point::new(c2x, c2y), Point::new(x, y));
				prev_qc = Some((x1, y1));
				prev_c2 = None;
				(cx, cy) = (x, y);
			}
			'T' => {
				let x = read_num(&chars, &mut i)?;
				let y = read_num(&chars, &mut i)?;
				let (x1, y1) = prev_qc.map(|(qx, qy)| (2.0 * cx - qx, 2.0 * cy - qy)).unwrap_or((cx, cy));
				let (c1x, c1y, c2x, c2y) = quad_to_cubic(cx, cy, x1, y1, x, y);
				b.curve_to(Point::new(c1x, c1y), Point::new(c2x, c2y), Point::new(x, y));
				prev_qc = Some((x1, y1));
				prev_c2 = None;
				(cx, cy) = (x, y);
			}
			't' => {
				let x = read_num(&chars, &mut i)? + cx;
				let y = read_num(&chars, &mut i)? + cy;
				let (x1, y1) = prev_qc.map(|(qx, qy)| (2.0 * cx - qx, 2.0 * cy - qy)).unwrap_or((cx, cy));
				let (c1x, c1y, c2x, c2y) = quad_to_cubic(cx, cy, x1, y1, x, y);
				b.curve_to(Point::new(c1x, c1y), Point::new(c2x, c2y), Point::new(x, y));
				prev_qc = Some((x1, y1));
				prev_c2 = None;
				(cx, cy) = (x, y);
			}
			'A' | 'a' => {
				let _rx = read_num(&chars, &mut i)?;
				let _ry = read_num(&chars, &mut i)?;
				let _rot = read_num(&chars, &mut i)?;
				let _large = read_num(&chars, &mut i)?;
				let _sweep = read_num(&chars, &mut i)?;
				let x = read_num(&chars, &mut i)?;
				let y = read_num(&chars, &mut i)?;
				let (nx, ny) = if c == 'a' { (cx + x, cy + y) } else { (x, y) };
				b.line_to(Point::new(nx, ny));
				(cx, cy) = (nx, ny);
				prev_c2 = None;
				prev_qc = None;
			}
			'Z' | 'z' => {
				b.close_path();
				(cx, cy) = (sx, sy);
				prev_c2 = None;
				prev_qc = None;
			}
			_ => return None,
		}
	}
	Some(b)
}

/// Converts a quadratic Bézier (p0, control q, p1) into cubic control handles.
fn quad_to_cubic(p0x: f64, p0y: f64, qx: f64, qy: f64, p1x: f64, p1y: f64) -> (f64, f64, f64, f64) {
	let c1x = p0x + (2.0 / 3.0) * (qx - p0x);
	let c1y = p0y + (2.0 / 3.0) * (qy - p0y);
	let c2x = p1x + (2.0 / 3.0) * (qx - p1x);
	let c2y = p1y + (2.0 / 3.0) * (qy - p1y);
	(c1x, c1y, c2x, c2y)
}

/// A process-unique temp path under the system temp dir.
fn unique_tmp(ext: &str) -> PathBuf {
	static COUNTER: OnceLock<Mutex<u64>> = OnceLock::new();
	let n = {
		let mut c = COUNTER.get_or_init(|| Mutex::new(0)).lock().unwrap();
		*c += 1;
		*c
	};
	std::env::temp_dir().join(format!("graphite_math_{}_{}.{ext}", std::process::id(), n))
}

/// Shapes `content` as a sub-span and returns its geometry plus its horizontal advance width.
fn shape_span(item: &Item<String>, content: &str, separate_glyphs: bool) -> (Vec<Item<Vector>>, f64) {
	let (font, typesetting) = read_typographic(item);
	let sub = build_sub_item(item, content);
	let shaped: Vec<Item<Vector>> = shape_text_item(&sub, separate_glyphs).into_iter().collect();
	let width = bounding_box(content, &font, typesetting, false).x;
	(shaped, width)
}

/// Builds a sub-item carrying only `content` and the typographic attributes needed to shape it.
/// Outer attributes (transform, blending, opacity) are intentionally omitted — they are applied
/// once to the fully concatenated result by `shape_text_item_with_math`.
fn build_sub_item(item: &Item<String>, content: &str) -> Item<String> {
	let (font, typesetting) = read_typographic(item);
	let mut sub = Item::new_from_element(content.to_string());
	sub.set_attribute(ATTR_FONT, font);
	sub.set_attribute(ATTR_FONT_SIZE, typesetting.font_size);
	sub.set_attribute(ATTR_LINE_HEIGHT, typesetting.line_height_ratio);
	sub.set_attribute(ATTR_LETTER_SPACING, typesetting.letter_spacing);
	sub.set_attribute(ATTR_LETTER_TILT, typesetting.letter_tilt);
	sub.set_attribute(ATTR_MAX_WIDTH, typesetting.max_width);
	sub.set_attribute(ATTR_MAX_HEIGHT, typesetting.max_height);
	sub.set_attribute(ATTR_TEXT_ALIGN, typesetting.align);
	sub
}

/// Reads the font and typesetting configuration from a text item, mirroring `shape_text_item`.
fn read_typographic(item: &Item<String>) -> (Resource, TypesettingConfig) {
	let defaults = TypesettingConfig::default();
	let font: Resource = item.attribute_cloned_or_default(ATTR_FONT);
	let typesetting = TypesettingConfig {
		font_size: item.attribute_cloned_or(ATTR_FONT_SIZE, defaults.font_size),
		line_height_ratio: item.attribute_cloned_or(ATTR_LINE_HEIGHT, defaults.line_height_ratio),
		letter_spacing: item.attribute_cloned_or(ATTR_LETTER_SPACING, defaults.letter_spacing),
		letter_tilt: item.attribute_cloned_or(ATTR_LETTER_TILT, defaults.letter_tilt),
		max_width: item.attribute_cloned_or::<Option<f64>>(ATTR_MAX_WIDTH, defaults.max_width),
		max_height: item.attribute_cloned_or::<Option<f64>>(ATTR_MAX_HEIGHT, defaults.max_height),
		align: item.attribute_cloned_or(ATTR_TEXT_ALIGN, defaults.align),
	};
	(font, typesetting)
}

/// Translates every vector inside `items` by `(dx, dy)`.
fn translate_all(items: &mut [Item<Vector>], dx: f64, dy: f64) {
	if dx == 0.0 && dy == 0.0 {
		return;
	}
	let translation = DAffine2::from_translation(DVec2::new(dx, dy));
	for item in items.iter_mut() {
		item.element_mut().transform(translation);
	}
}

/// Merges a list of vectors into a single compound `Vector` by concatenating their stroke
/// bézier paths. Used by the single-`Vector` output path (`Text to Vector`), which collapses the
/// span list into one item. Each vector's `ATTR_TRANSFORM` is baked into its geometry first so the
/// merge does not drop positioning.
pub fn merge_vectors(vectors: List<Vector>) -> Vector {
	let mut bezpath = BezPath::new();
	for item in vectors.into_iter() {
		let transform = item.attribute_cloned_or_default::<DAffine2>(ATTR_TRANSFORM);
		let mut vector = item.into_element();
		if transform != DAffine2::IDENTITY {
			vector.transform(transform);
		}
		for span in vector.stroke_bezpath_iter() {
			for element in span.elements() {
				bezpath.push(*element);
			}
		}
	}
	Vector::from_bezpath(bezpath)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn item_with(text: &str) -> Item<String> {
		Item::new_from_element(text.to_string())
	}

	/// Overall horizontal extent of a vector list (max right edge across all spans).
	fn list_width(list: &List<Vector>) -> f64 {
		list.clone()
			.into_iter()
			.filter_map(|item| item.element().bounding_box())
			.map(|[min, max]| max.x - min.x)
			.fold(0.0, f64::max)
	}

	/// Overall vertical extent of a vector list (top to bottom across all spans).
	fn list_height(list: &List<Vector>) -> f64 {
		let min_y = list
			.clone()
			.into_iter()
			.filter_map(|item| item.element().bounding_box())
			.map(|[min, _]| min.y)
			.fold(f64::INFINITY, f64::min);
		let max_y = list
			.clone()
			.into_iter()
			.filter_map(|item| item.element().bounding_box())
			.map(|[_, max]| max.y)
			.fold(f64::NEG_INFINITY, f64::max);
		max_y - min_y
	}

	#[test]
	fn no_math_delegates_to_single_compound_vector() {
		let list = shape_text_item_with_math(&item_with("plain text only"), false);
		assert_eq!(list.len(), 1, "no-math fast path should yield exactly one vector");
	}

	#[test]
	fn inline_math_advances_x() {
		let whole = shape_text_item_with_math(&item_with("ab $cd$ ef"), true);
		let just_ab = shape_text_item_with_math(&item_with("ab "), true);
		let whole_w = list_width(&whole);
		let ab_w = list_width(&just_ab);
		assert!(whole_w > ab_w, "inline math should advance x: whole={whole_w} vs ab={ab_w}");
		assert!(whole.len() > 0, "result must contain geometry");
	}

	#[test]
	fn display_math_breaks_onto_its_own_line() {
		let list = shape_text_item_with_math(&item_with("Before $$x$$ after"), false);
		let height = list_height(&list);
		assert!(height > 40.0, "display math should span multiple lines, height={height}");
	}

	#[test]
	fn unmatched_dollar_degrades_to_plain_without_panic() {
		let list = shape_text_item_with_math(&item_with("price $100 only"), false);
		assert_eq!(list.len(), 1, "degraded literal text is a single plain span");
		assert!(list.len() > 0);
	}

	#[test]
	fn merge_vectors_produces_single_vector() {
		let list = shape_text_item_with_math(&item_with("ab $cd$ ef"), false);
		let merged = merge_vectors(list);
		assert!(merged.bounding_box().is_some(), "merged vector must retain geometry");
	}

	#[test]
	fn bake_math_span_never_panics_and_yields_geometry() {
		// Works whether or not `typst` is installed: with typst it real-bakes, without it falls
		// back to the placeholder (source-as-text). Either way: non-empty geometry, finite width.
		for src in ["\\frac{a}{b}", "E = mc^2", "\\sum_{i=1}^n x_i^2"] {
			let (list, width) = bake_math_span(&item_with("x"), src);
			assert!(!list.is_empty(), "bake must yield geometry for {src}");
			assert!(width.is_finite() && width > 0.0, "bake width must be finite/positive for {src}");
		}
	}

	#[test]
	fn real_bake_scales_to_font_size_when_typst_present() {
		if !typst_available() {
			return; // headless/no-typst environment: placeholder path is exercised elsewhere.
		}
		let (vec, width) = bake_math_span(&item_with("x"), "\\frac{a}{b}");
		assert!(!vec.is_empty(), "real bake must yield geometry");
		// Convert to `List` to reuse the height helper; height should track the default font size
		// (24px) with generous slack because mitex metrics differ from Parley's.
		let mut list = List::new();
		for v in vec {
			list.push(v);
		}
		let h = list_height(&list);
		assert!(h > 8.0 && h < 80.0, "real-baked math height {h} should track font size");
		assert!(width > 0.0, "real-baked math width must be positive");
	}

	#[test]
	fn normalize_preserves_y_down_orientation_no_vertical_flip() {
		// The desktop feeds MathJax/KaTeX SVG paths (y-down) through the cache, and
		// `bake_math_span` normalizes them. Regular text is y-down too (PathBuilder: `origin.y - y`).
		// Baked math must stay y-down — a y-flip here mirrors the glyphs upside-down ("倒着的").
		//
		// This feeds a synthetic y-down path: a vertical stem at x=0 from y=0..10 with a short tick
		// at the TOP (y=0, x=5..6). After a correct (non-flipping) normalize the tick must remain at
		// the TOP (min y). A vertical flip would push the tick to the BOTTOM (max y) and fail this.
		let source = "$orientation-guard-xyz$";
		let path = "M0 0 L0 10 M5 0 L6 0"; // y-down: tick at top (y=0), stem bottom at y=10
		assert!(cache_baked_math_svg(source, 24.0, &[path.to_string()]), "feed must succeed");

		let (vec, _) = bake_math_span(&item_with("x"), source);
		assert!(!vec.is_empty(), "bake must yield geometry");

		// Collect every anchor point across the produced vectors.
		let mut anchors = Vec::new();
		for item in &vec {
			for sp in item.element().stroke_bezier_paths() {
				for mg in sp.manipulator_groups() {
					anchors.push(mg.anchor);
				}
			}
		}
		assert!(!anchors.is_empty(), "must have anchor points");

		let min_y = anchors.iter().map(|p| p.y).fold(f64::INFINITY, f64::min);
		let max_y = anchors.iter().map(|p| p.y).fold(f64::NEG_INFINITY, f64::max);
		let span = (max_y - min_y).max(1.0);

		// The tick is the only feature at the maximum x (stem is at x=0, tick at x=5..6; scaling
		// only stretches x, so the tick stays the right-most anchor). It sits at the TOP (input
		// y=0). After a correct (non-flipping) normalize it must remain at min y.
		let tick = anchors.iter().max_by(|a, b| a.x.total_cmp(&b.x)).expect("tick anchor must exist");
		assert!((tick.y - min_y).abs() < 1e-6 * span, "tick must remain at the TOP (min y); math was vertically flipped (tick.y={}, min_y={}, max_y={})", tick.y, min_y, max_y);
	}

	#[test]
	fn extract_path_d_skips_clip_paths_and_background_rects() {
		let svg = r#"<svg><clipPath id="c"><path d="M0 0 L1 1"/></clipPath><path d="M2 2 L3 3"/><path d="M4 4 L5 5"/></svg>"#;
		let ds = extract_path_d(svg);
		assert_eq!(ds.len(), 2, "clip-path must be skipped");
		// `transform_point` applies the `usvg` transform faithfully (no extra flip), so a path
		// drawn at positive y in the fixture is emitted at positive y.
		assert!(ds[0].contains("M 2.000 2.000"), "visible path must be extracted");
		assert!(ds[1].contains("M 4.000 4.000"), "visible path must be extracted");
	}

	#[test]
	fn extract_path_d_skips_typst_white_page_rect() {
		// typst's SVG output contains a full-page white background rect before the glyphs.
		let svg = r##"<svg width="100" height="100" viewBox="0 0 100 100"><path fill="#ffffff" d="M0 0 H100 V100 H0 Z"/><path d="M10 10 L20 20"/></svg>"##;
		let ds = extract_path_d(svg);
		assert_eq!(ds.len(), 1, "white page rect must be skipped");
		// Faithful transform → positive y preserved.
		assert!(ds[0].contains("M 10.000 10.000"), "glyph path must be extracted");
	}

	#[test]
	fn extract_path_d_resolves_use_references_and_applies_transforms() {
		// typst places glyphs via <use> references into <defs><symbol>; usvg expands these.
		// `transform_point` applies the absolute transform faithfully: a <use x=20 y=30> becomes a
		// translation, so the symbol's local origin lands at (20, 30) in y-down SVG coordinates
		// (the real typst `matrix(1 0 0 -1)` flip is already part of the absolute transform and is
		// likewise applied faithfully — no extra negation here).
		let svg = r##"<svg width="100" height="100" viewBox="0 0 100 100" xmlns:xlink="http://www.w3.org/1999/xlink">
			<defs><symbol id="g0" overflow="visible"><path d="M0 0 L10 0 L5 8 Z"/></symbol></defs>
			<path fill="#ffffff" d="M0 0 H100 V100 H0 Z"/>
			<use xlink:href="#g0" x="20" y="30"/>
		</svg>"##;
		let ds = extract_path_d(svg);
		assert_eq!(ds.len(), 1, "only the resolved glyph path must remain");
		// The <use> places the symbol at x=20, y=30; the symbol path M0 0 L10 0 L5 8 is translated
		// to M 20.000 30.000 / L 30.000 30.000 / L 25.000 38.000.
		assert!(ds[0].contains("M 20.000 30.000"), "use x/y translation must be applied");
		assert!(ds[0].contains("L 30.000 30.000"), "use x/y translation must be applied");
		assert!(ds[0].contains("L 25.000 38.000"), "use x/y translation must be applied");
	}

	#[test]
	fn escape_typst_string_handles_quotes_and_backslashes() {
		assert_eq!(escape_typst_string(r#"a\b"c"#), r#"a\\b\"c"#);
	}

	#[test]
	fn svg_path_to_bezpath_parses_basic_shapes() {
		// Square: M0 0 L10 0 L10 10 Z -> bbox [0,0]..[10,10].
		let bez = svg_path_to_bezpath("M0 0 L10 0 L10 10 Z").expect("parse square");
		let v = Vector::from_bezpath(bez);
		let [min, max] = v.bounding_box().expect("bbox");
		assert_eq!((min.x, min.y, max.x, max.y), (0.0, 0.0, 10.0, 10.0));

		// Cubic relative form with implicit repeat: "m0 0 c1 1 2 1 3 0 c1 -1 2 -1 3 0".
		let bez = svg_path_to_bezpath("m0 0 c1 1 2 1 3 0 c1 -1 2 -1 3 0").expect("parse relative cubic");
		assert!(Vector::from_bezpath(bez).bounding_box().is_some());

		// Malformed: leading number without a command -> None.
		assert!(svg_path_to_bezpath("5 5").is_none());
	}
}
