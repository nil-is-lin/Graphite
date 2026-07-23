# ADR-005: TikZ (LaTeX) Vector Export

## Status
Accepted

## Context
Users — especially in academic / paper-authoring workflows — want to export Graphite artwork as TikZ (`tikzpicture`) code so it can be `\input` directly into LaTeX documents, keeping vector crispness and editability that a rasterized PNG cannot provide.

Graphite's document model is Bézier-curve + affine-transform based (`Vector` / `Graphic` / `Artboard`, backed by `glam::DAffine2` and `vector_types::subpath::BezierHandles`). TikZ is also a path + control-point + transform language, so the mapping is nearly 1:1 for the core geometry. Today only SVG / raster export exists (`SvgRender` in `node-graph/libraries/rendering/src/renderer.rs`), and there is **no** SVG importer and **no** TikZ/LaTeX code anywhere in the Rust source.

The question was whether to integrate "svg2tikz" (an external SVG→TikZ converter). Two strategies were weighed:

- **A. Native Rust `TikzRender`** (chosen) — a sibling of `SvgRender` that walks the same `Graphic`/`Vector`/`Subpath` tree but emits `\draw`/`\fill`. Zero external dependencies, fast, and structurally identical to the existing SVG path, so it is easy to maintain and easy to revert.
- **B. Wrap the `svg2tikz` Python tool** — export SVG first, then shell out to Python. Reuses a mature library but adds a Python dependency + process boundary, and inherits `svg2tikz`'s own coverage limits. Also requires a working SVG importer, which Graphite lacks.

Native Rust was selected. The feature is a **document → TikZ export**, not "import SVG then convert".

## Decision
Add a `TikzRender` (`node-graph/libraries/rendering/src/tikz.rs`) as a sibling of `SvgRender`, driven by a new `render_tikz` method on the existing `Render` trait. Thread a `Tikz` variant through the export pipeline:

- `graphene_std::application_io::ExportFormat::Tikz`
- `rendering::RenderOutputTypeRequest::Tikz`
- `graph_craft::RenderOutputType::Tikz { tikz }` (no `image_data` — TikZ never embeds rasters, so the always-empty field carried over from `Svg` was dropped)
- `rendering::RenderIntermediateType::Tikz(Arc<TikzRenderOutput>)`
- `graphene_cli::FileType::Tikz` (`.tex` extension) + direct `.tex` writer
- Editor `FileType::Tikz` + export-dialog entry + executor mapping

The output is a **complete, compilable standalone LaTeX document** (not just a `tikzpicture` snippet). `TikzRenderOutput::to_document()` assembles the raw path body into `\documentclass{article}` + `\usepackage{tikz}` + `\begin{document}` + `\begin{tikzpicture}[x=1pt,y=1pt]` (the `x=1pt,y=1pt` base unit treats Graphite units as points so typical artwork renders at a sensible size rather than ~100 cm) + a `yscale=-1` scope (so Graphite's y-down coordinates render upright, matching SVG orientation) + `\end{tikzpicture}` + `\end{document}`. This means the exported `.tex` compiles directly with `tectonic file.tex` / `pdflatex` with no manual wrapping. (If a user instead wants a bare `tikzpicture` to `\input` into their own document, that is a future option — the raw body is still available via `TikzRender`.)

## Consequences

### What becomes easier
- Academic users can drop Graphite vector art into LaTeX with full editability.
- No external runtime (no Python, no `svg2tikz`) in the export path.
- The implementation mirrors `SvgRender`, so future render features can be ported method-by-method.

### What becomes harder / degraded (by design)
TikZ is **not** a lossless superset of Graphite's model. The following are handled via **graceful degradation** (emitted as `% …` comments, or approximated), never silent:

| Feature | Treatment in Phase 1 |
|---|---|
| Paths / Bézier curves / affine transforms | ✅ Full (baked into coordinates) |
| Solid-color fills & strokes | ✅ Full (incl. opacity, dash, cap/join) |
| Group / artboard transforms | ✅ Full (transform stack baked into coords) |
| Even-odd fill rule (branching) | ✅ `even odd rule` |
| Stroke alignment (inside/outside) | ⚠️ Approximated as centered; emits a comment |
| Linear / radial gradients | ✅ 2-stop linear (`\shade` + `shading angle`, y-flip corrected) & radial (`inner/outer color`); multi-stop → solid first-stop fallback + comment |
| Text | ✅ Filled glyph outlines (same bézier path the SVG/Vello renderers use); default black fill, layer opacity/transform honored |
| Raster layers (images) | ⚠️ Skipped; comment emitted (TikZ would need `\includegraphics` + external assets) |
| Blend modes | ⚠️ Ignored; comment emitted |
| Clip / mask paths | ❌ Not represented; degraded |

### Editor / frontend gap — RESOLVED
The desktop/web editor returns `RenderOutput` to the TypeScript frontend, which performs the file save. This is now wired: `editor/src/node_graph_executor.rs::process_export` gained a `Tikz => "tex"` file extension and a `RenderOutputType::Tikz` arm that dispatches the existing `FrontendMessage::TriggerSaveFile` (the same handler the SVG/byte path already uses, so **no new TS message type** was needed). The frontend `downloadFile` in `frontend/src/utility-functions/files.ts` now maps `.tex` → `text/plain` so the downloaded blob has the correct MIME type. The export dialog already listed TikZ.

## Redundancy removed during implementation
- **Single document wrap.** The wrapper was originally applied in *both* the intermediate and final render phases, producing nested `\begin{tikzpicture}`. Now the raw body is stored in `TikzRenderOutput` and wrapped exactly once by `TikzRenderOutput::to_document()` (which emits the full `\documentclass`…`\end{document}` shell) in the final `render` step.
- **No-op `wrap_with_transform`.** TikZ bakes all transforms into path coordinates during traversal, so the `SvgRender::wrap_with_transform` call site had nothing to do; the empty method and its call were deleted.
- **Always-empty `image_data`.** `RenderOutputType::Tikz` / `TikzRenderOutput` no longer carry the `image_data` map (TikZ degrades rasters to comments, so it was never populated).

## Implementation phases
- **Phase 1 (done):** `render_tikz` implemented for `List<Vector>` (core geometry, solid fills/strokes, transforms, opacity, dash, even-odd). CLI `.tex` export works end-to-end.
- **Phase 2 (done):** 2-stop linear (`\shade` + `shading angle`) and radial (`inner/outer color`) gradient shading; multi-stop falls back to a solid first-stop fill. Text rendered as filled glyph outlines.
- **Phase 3 (done):** Editor-frontend `.tex` save handler wired via `TriggerSaveFile` (no new TS message); `.tex` MIME in `downloadFile`. Pattern fills / blend-mode approximations remain as documented degradations (TikZ has no first-class equivalent).

## Reversibility
Every change is additive (new enum variants + a new sibling renderer). Removing TikZ export means deleting `tikz.rs` and the `Tikz` variants — no existing SVG/raster behavior is touched. Low risk, fully reversible.

## Verification
Output correctness is verified by **compiling the generated `.tex` with [Tectonic](https://tectonic-typesetting.github.io/)** (self-contained TeX Live, zero system LaTeX dependency). A unit test in `node-graph/libraries/rendering/src/renderer.rs` (`tikz_export_tests::tikz_export_produces_compilable_document`) calls `List<Vector>::render_tikz` directly on hand-built scenes — a solid fill + dashed stroke, a 2-stop linear gradient fill (`\shade`), and a gradient stroke (approximated solid) — wraps the body in a minimal `\documentclass{article}\usepackage{tikz}` document, and writes `/tmp/tikz_verify.tex`. That file is then compiled with `tectonic /tmp/tikz_verify.tex`; a clean exit (PDF produced, no TeX errors) proves the emitted TikZ is syntactically valid. This exercises the renderer without the GPU/display the CLI's full pipeline requires, so it runs in any environment.
