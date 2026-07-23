// Inline LaTeX math baking for the Text tool (P2b/P3, ADR-006).
//
// Mirrors the Rust `text_nodes::span::parse_text_spans` parser so the cache keys produced here
// exactly match the ones the Rust render path looks up (the math content *without* its `$`
// delimiters). For each math span we render the LaTeX to SVG via MathJax (the only browser-side
// engine that emits real `<path d>` geometry — KaTeX only emits HTML/webfont glyphs, which cannot
// be baked into Graphite's `Vector` type), extract the path data, and feed it into Rust's
// source-hash cache via `editor.cacheBakedMath(...)`. The Rust side normalizes the geometry to the
// text's font size, so we only forward the raw path data plus the font size.
//
// MathJax is loaded lazily (dynamic import of its self-contained `tex-svg.js` browser build) the
// first time math is encountered, so the ~1.8 MB engine is never downloaded unless the user
// actually types `$...$`.

import type { EditorWrapper } from "/wrapper/pkg/graphite_wasm_wrapper";

// MathJax v4's browser API (populated once the engine finishes loading). v4 removed the v3
// `tex2svg`/`tex2svgPromise` globals; conversion now goes through the document handler:
// `MathJax.startup.document.convertPromise(tex, { display })` returns the `mjx-container`
// (wrapping an `<svg>`). We set `svg.fontCache: "none"` so glyphs are emitted as inline
// `<path d="...">` rather than `<defs>`+`<use>` references, which our `extractPathD` can parse.
interface MathJaxGlobal {
	startup: {
		promise: Promise<unknown>;
		document: {
			convertPromise(tex: string, options?: { display?: boolean }): Promise<HTMLElement>;
		};
	};
}
declare global {
	interface Window {
		MathJax?: MathJaxGlobal;
	}
}

let mathjaxReady: Promise<void> | null = null;

/** Loads MathJax's SVG component on first use. Idempotent. */
function ensureMathJax(): Promise<void> {
	if (mathjaxReady) return mathjaxReady;

	// Configure before the engine boots so it doesn't try to typeset the whole page.
	window.MathJax = {
		svg: { fontCache: "none" },
		startup: { typeset: false },
	} as unknown as MathJaxGlobal;

	// MathJax's self-contained browser build ships no type declarations; we drive it via the
	// `window.MathJax` global populated once the chunk executes.
	// @ts-ignore - no typings for the dynamically-imported engine chunk
	mathjaxReady = import("mathjax/tex-svg.js")
		.then(() => window.MathJax!.startup.promise)
		.then(() => undefined);
	return mathjaxReady;
}

/** Renders `tex` to an SVG string via MathJax v4. Returns "" on failure. */
async function latexToSvg(tex: string, display: boolean): Promise<string> {
	try {
		await ensureMathJax();
		// v4 conversion API: the document handler's `convertPromise` returns the `mjx-container`
		// (an `<mjx-container>` wrapping an `<svg>`). `display` selects block vs inline layout.
		const node = await window.MathJax!.startup.document.convertPromise(tex, { display });
		// Extract the `<svg>` (or fall back to the container itself if it is the svg).
		const svg = node.querySelector ? node.querySelector("svg") : null;
		const el = svg ?? node;
		return el && "outerHTML" in el ? el.outerHTML : "";
	} catch {
		return "";
	}
}

type SpanKind = "plain" | "inline" | "display";
interface TextSpan {
	kind: SpanKind;
	content: string;
}

/** Minimal view of the editor wrapper surface we use, so we don't depend on the generated types. */
interface MathBakeEditor {
	cacheBakedMath(source: string, font_size: number, svg_paths: string[]): void;
}

/**
 * Splits `text` into plain / inline-math / display-math spans. A faithful TypeScript port of
 * `text_nodes::span::parse_text_spans` (delimiter rules documented there).
 */
export function parseTextSpans(text: string): TextSpan[] {
	const chars = Array.from(text);
	const len = chars.length;
	const spans: TextSpan[] = [];
	let plain = "";
	let i = 0;

	const pushPlain = () => {
		if (plain.length > 0) {
			spans.push({ kind: "plain", content: plain });
			plain = "";
		}
	};

	const findCloser = (start: number, isDisplay: boolean): number | undefined => {
		let j = start;
		while (j < len) {
			const c = chars[j];
			if (c === "\\") {
				j += j + 1 < len ? 2 : 1;
				continue;
			}
			if (c === "$") {
				if (isDisplay) {
					if (j + 1 < len && chars[j + 1] === "$") return j;
					j += 1;
					continue;
				} else {
					if (j + 1 < len && chars[j + 1] === "$") {
						j += 2;
						continue;
					}
					return j;
				}
			}
			j += 1;
		}
		return undefined;
	};

	while (i < len) {
		const c = chars[i];

		if (c === "\\") {
			if (i + 1 < len) {
				const next = chars[i + 1];
				if (next === "$") plain += "$";
				else {
					plain += "\\";
					plain += next;
				}
				i += 2;
				continue;
			}
			plain += "\\";
			i += 1;
			continue;
		}

		if (c === "$") {
			const isDisplay = i + 1 < len && chars[i + 1] === "$";
			const openLen = isDisplay ? 2 : 1;

			// A lone `$` that is really the first char of a `$$` pair cannot open inline math.
			if (!isDisplay && i + 1 < len && chars[i + 1] === "$") {
				plain += "$";
				i += 1;
				continue;
			}

			const closer = findCloser(i + openLen, isDisplay);
			if (closer !== undefined) {
				const openEnd = i + openLen;
				const content = text.slice(openEnd, closer);
				pushPlain();
				spans.push({ kind: isDisplay ? "display" : "inline", content });
				i = closer + openLen;
			} else {
				for (let k = 0; k < openLen; k++) plain += "$";
				i += openLen;
			}
			continue;
		}

		plain += c;
		i += 1;
	}

	pushPlain();
	return spans;
}

/**
 * Extracts `d="..."` attributes from an SVG string, skipping `<clipPath>` subtrees. Mirrors the
 * Rust `extract_path_d` helper so frontend- and native-baked geometry come from the same source.
 */
function extractPathD(svg: string): string[] {
	const bytes = svg;
	const out: string[] = [];
	let inClip = false;
	let i = 0;
	const lower = (s: string, from: number, needle: string) => s.slice(from, from + needle.length).toLowerCase() === needle;

	while (i < bytes.length) {
		if (lower(bytes, i, "<clippath")) inClip = true;
		else if (lower(bytes, i, "</clippath>")) inClip = false;
		else if (!inClip && lower(bytes, i, "<path")) {
			const d = readAttr(bytes, i, "d");
			if (d !== undefined) out.push(d);
		}
		i += 1;
	}
	return out;
}

/** Reads `name="value"` starting at tag position `start`, handling escaped quotes. */
function readAttr(s: string, start: number, name: string): string | undefined {
	const needle = `${name}="`;
	const idx = s.indexOf(needle, start);
	if (idx === -1) return undefined;
	let j = idx + needle.length;
	let result = "";
	while (j < s.length) {
		const ch = s[j];
		if (ch === "\\") {
			result += ch;
			if (j + 1 < s.length) result += s[j + 1];
			j += 2;
			continue;
		}
		if (ch === '"') break;
		result += ch;
		j += 1;
	}
	return result;
}

/**
 * Bakes every math span in `text` via MathJax and feeds the resulting SVG path geometry into
 * Rust's cache. Failures (unsupported macro, engine error) are ignored — the Rust render path
 * falls back to the placeholder bake, so editing never breaks.
 */
export async function bakeMath(editor: EditorWrapper, text: string, fontSize: number): Promise<void> {
	if (!text.includes("$")) return;

	const spans = parseTextSpans(text);
	await Promise.all(
		spans
			.filter((span) => span.kind !== "plain")
			.map(async (span) => {
				// Cache key is the math content *without* delimiters, matching Rust's lookup key.
				const svg = await latexToSvg(span.content, span.kind === "display");
				const paths = extractPathD(svg);
				if (paths.length === 0) return;
				(editor as unknown as MathBakeEditor).cacheBakedMath(span.content, fontSize, paths);
			})
	);
}

/** HTML-escapes plain text before it is embedded in the preview's `innerHTML`. */
function escapeHtml(s: string): string {
	return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

/**
 * Builds an HTML preview of `text` with math spans typeset by MathJax (P3, ADR-006) — the same
 * engine used for the bake, so the preview faithfully reflects what gets rendered on the canvas.
 * Plain text is HTML-escaped; math spans become inline MathJax SVGs. Returns "" if math is
 * present but the engine has not loaded yet (the caller re-invokes on the next edit).
 */
export async function renderMathPreview(text: string): Promise<string> {
	const spans = parseTextSpans(text);
	const parts = await Promise.all(
		spans.map(async (span) => {
			if (span.kind === "plain") return escapeHtml(span.content);

			const svg = await latexToSvg(span.content, span.kind === "display");
			if (svg) return `<span class="mathjax-preview">${svg}</span>`;

			// On render failure / engine not ready, show the escaped raw source so editing never breaks.
			const delim = span.kind === "display" ? "$$" : "$";
			return escapeHtml(`${delim}${span.content}${delim}`);
		})
	);
	return parts.join("");
}
