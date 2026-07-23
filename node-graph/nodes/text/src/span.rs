//! Text span segmentation for inline LaTeX math (`$...$` / `$$...$$`).
//!
//! Splits a Text node's content string into ordinary text and math spans. Math spans
//! are later baked to a `Vector` (via MathJax/typst+mitex, per ADR-006) and spliced back into the text
//! flow by the flow concatenator (P1). This module is intentionally **pure** — no Parley,
//! font, or renderer dependency — so it can be unit-tested in isolation and reused by the
//! frontend for live preview.
//!
//! Delimiter rules (v1):
//! - `$$...$$` — display math, forced onto its own block by the concatenator.
//! - `$...$`   — inline math, flows within the surrounding line.
//! - `\$`      — an escaped dollar; treated as a literal `$` (never a delimiter).
//! - An opener with no matching closer degrades gracefully to literal text (no panic, no
//!   swallowed characters).
//! - Display and inline closer detection are mutually exclusive: a `$$` never closes an
//!   inline span and a lone `$` never closes a display span. Malformed input such as
//!   `$x$$y$` therefore yields one inline span with content `x$$y` rather than toggling.

/// A contiguous segment of a Text node's content string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextSpan {
	/// Ordinary text, shaped by Parley via the existing `to_path` pipeline.
	Plain(String),
	/// Inline math `$...$`, flows within the surrounding line.
	InlineMath(String),
	/// Display math `$$...$$`, forced onto its own block by the concatenator.
	DisplayMath(String),
}

impl TextSpan {
	/// Whether this span is a math span (inline or display).
	pub fn is_math(&self) -> bool {
		!matches!(self, TextSpan::Plain(_))
	}

	/// The raw content of the span (without delimiters for math spans).
	pub fn content(&self) -> &str {
		match self {
			TextSpan::Plain(s) | TextSpan::InlineMath(s) | TextSpan::DisplayMath(s) => s,
		}
	}
}

/// Splits `text` into plain and math spans. An empty input yields an empty `Vec`, matching
/// the existing `shape_text_item` early-return of an empty `List<Vector>`; otherwise at least
/// one span is returned.
pub fn parse_text_spans(text: &str) -> Vec<TextSpan> {
	let chars: Vec<(usize, char)> = text.char_indices().collect();
	let len = chars.len();

	let mut spans: Vec<TextSpan> = Vec::new();
	let mut plain = String::new();
	let mut i = 0;

	while i < len {
		let (_, c) = chars[i];

		// Escaped character: `\$` becomes a literal `$`; any other `\<x>` is kept verbatim.
		if c == '\\' {
			if i + 1 < len {
				let next = chars[i + 1].1;
				if next == '$' {
					plain.push('$');
				} else {
					plain.push('\\');
					plain.push(next);
				}
				i += 2;
				continue;
			}
			// A trailing backslash with nothing after it: keep it as plain text.
			plain.push('\\');
			i += 1;
			continue;
		}

	if c == '$' {
		let is_display = i + 1 < len && chars[i + 1].1 == '$';
		let open_len = if is_display { 2 } else { 1 };

		// A lone `$` that is the first character of a `$$` pair cannot open inline math
		// (display owns `$$`), so it degrades to a literal dollar. This is the mirror of the
		// closer rule: `$$` never participates in an inline span.
		if !is_display && i + 1 < len && chars[i + 1].1 == '$' {
			plain.push('$');
			i += 1;
			continue;
		}

		match find_closer(&chars, i + open_len, is_display) {
				Some(close_start) => {
					let open_end = chars[i + open_len].0;
					let close_byte = chars[close_start].0;
					let content = text[open_end..close_byte].to_string();
					push_plain(&mut plain, &mut spans);
					if is_display {
						spans.push(TextSpan::DisplayMath(content));
					} else {
						spans.push(TextSpan::InlineMath(content));
					}
					i = close_start + open_len;
				}
				None => {
					// No matching closer: the opener is a false start, emit literal `$`(s).
					for _ in 0..open_len {
						plain.push('$');
					}
					i += open_len;
				}
			}
			continue;
		}

		// Ordinary character.
		plain.push(c);
		i += 1;
	}

	push_plain(&mut plain, &mut spans);
	spans
}

/// Flushes the pending plain-text buffer into `spans` as a `Plain` span (if non-empty).
fn push_plain(plain: &mut String, spans: &mut Vec<TextSpan>) {
	if !plain.is_empty() {
		spans.push(TextSpan::Plain(std::mem::take(plain)));
	}
}

/// Searches `chars[start..]` for the opening index of a matching closer.
///
/// - For display math, the closer is the next unescaped `$$`.
/// - For inline math, the closer is the next unescaped single `$` that is *not* the first
///   character of a `$$` pair (so `$$` inside inline math never closes it).
///
/// Returns the index into `chars` of the first `$` of the closer, or `None` if none exists.
/// An unescaped backslash (`\`) and the character following it are skipped so that `\$` is
/// never treated as a delimiter.
fn find_closer(chars: &[(usize, char)], start: usize, is_display: bool) -> Option<usize> {
	let len = chars.len();
	let mut j = start;

	while j < len {
		let (_, c) = chars[j];

		if c == '\\' {
			// Skip the escaped character so `\$` is not a delimiter.
			j += if j + 1 < len { 2 } else { 1 };
			continue;
		}

		if c == '$' {
			if is_display {
				// Need a `$$` closer.
				if j + 1 < len && chars[j + 1].1 == '$' {
					return Some(j);
				}
				// A lone `$` while scanning for display does not close; skip it.
				j += 1;
				continue;
			} else {
				// Inline closer: a single `$` that is not the first of a `$$` pair.
				if j + 1 < len && chars[j + 1].1 == '$' {
					j += 2;
					continue;
				}
				return Some(j);
			}
		}

		j += 1;
	}

	None
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Returns the kind of each span as a short string for compact assertions.
	fn kinds(spans: &[TextSpan]) -> Vec<&'static str> {
		spans.iter().map(|s| match s {
			TextSpan::Plain(_) => "plain",
			TextSpan::InlineMath(_) => "inline",
			TextSpan::DisplayMath(_) => "display",
		}).collect()
	}

	#[test]
	fn empty_input_yields_empty_vec() {
		assert!(parse_text_spans("").is_empty());
	}

	#[test]
	fn no_math_is_single_plain() {
		let spans = parse_text_spans("hello world");
		assert_eq!(kinds(&spans), vec!["plain"]);
		assert_eq!(spans[0], TextSpan::Plain("hello world".into()));
	}

	#[test]
	fn inline_math_basic() {
		let spans = parse_text_spans("Mass is $E=mc^2$ here");
		assert_eq!(kinds(&spans), vec!["plain", "inline", "plain"]);
		assert_eq!(spans[0], TextSpan::Plain("Mass is ".into()));
		assert_eq!(spans[1], TextSpan::InlineMath("E=mc^2".into()));
		assert_eq!(spans[2], TextSpan::Plain(" here".into()));
	}

	#[test]
	fn display_math_basic() {
		let spans = parse_text_spans("Before $$x = \\frac{1}{2}$$ after");
		assert_eq!(kinds(&spans), vec!["plain", "display", "plain"]);
		assert_eq!(spans[0], TextSpan::Plain("Before ".into()));
		assert_eq!(spans[1], TextSpan::DisplayMath("x = \\frac{1}{2}".into()));
		assert_eq!(spans[2], TextSpan::Plain(" after".into()));
	}

	#[test]
	fn escaped_dollar_is_literal() {
		let spans = parse_text_spans("Cost is \\$5 and $a+b$");
		assert_eq!(kinds(&spans), vec!["plain", "inline"]);
		assert_eq!(spans[0], TextSpan::Plain("Cost is $5 and ".into()));
		assert_eq!(spans[1], TextSpan::InlineMath("a+b".into()));
	}

	#[test]
	fn unmatched_inline_becomes_literal() {
		let spans = parse_text_spans("price $100 only");
		assert_eq!(kinds(&spans), vec!["plain"]);
		assert_eq!(spans[0], TextSpan::Plain("price $100 only".into()));
	}

	#[test]
	fn unmatched_display_becomes_literal() {
		let spans = parse_text_spans("mood $$ great");
		assert_eq!(kinds(&spans), vec!["plain"]);
		assert_eq!(spans[0], TextSpan::Plain("mood $$ great".into()));
	}

	#[test]
	fn backslash_preserved_in_math() {
		let spans = parse_text_spans("$\\alpha^2$");
		assert_eq!(spans, vec![TextSpan::InlineMath("\\alpha^2".into())]);
	}

	#[test]
	fn escaped_dollar_inside_math_preserved() {
		let spans = parse_text_spans("$a\\$b$");
		assert_eq!(spans, vec![TextSpan::InlineMath("a\\$b".into())]);
	}

	#[test]
	fn inline_not_closed_by_double_dollar() {
		// v1 rule: `$$` never closes an inline span (mutually exclusive closers).
		let spans = parse_text_spans("$x$$y$");
		assert_eq!(kinds(&spans), vec!["inline"]);
		assert_eq!(spans[0], TextSpan::InlineMath("x$$y".into()));
	}

	#[test]
	fn consecutive_inline_math() {
		let spans = parse_text_spans("$x$ and $y$");
		assert_eq!(kinds(&spans), vec!["inline", "plain", "inline"]);
		assert_eq!(spans[0], TextSpan::InlineMath("x".into()));
		assert_eq!(spans[1], TextSpan::Plain(" and ".into()));
		assert_eq!(spans[2], TextSpan::InlineMath("y".into()));
	}

	#[test]
	fn mixed_inline_and_display() {
		let spans = parse_text_spans("See $a+b$ and $$c=d$$ end");
		assert_eq!(kinds(&spans), vec!["plain", "inline", "plain", "display", "plain"]);
		assert_eq!(spans[1], TextSpan::InlineMath("a+b".into()));
		assert_eq!(spans[3], TextSpan::DisplayMath("c=d".into()));
	}

	#[test]
	fn is_math_helper() {
		let spans = parse_text_spans("x $y$ $$z$$");
		assert!(!spans[0].is_math()); // "x "
		assert!(spans[1].is_math()); // inline "y"
		assert!(!spans[2].is_math()); // " "
		assert!(spans[3].is_math()); // display "z"
	}
}
