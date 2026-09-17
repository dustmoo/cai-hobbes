//! LaTeX-ish inline symbol/arrow rendering for chat prose.
//!
//! Models frequently write bare LaTeX macros in prose (`\to`, `\Rightarrow`,
//! `$A \to B$`) without a full math renderer to display them. This module
//! substitutes a small, well-known table of macros (arrows plus a handful of
//! common operators) with their Unicode equivalents.
//!
//! This is deliberately *not* a LaTeX/math parser: it only recognizes a flat
//! table of macros and a few delimiter conventions (`$...$`, `\(...\)`,
//! `$$...$$`). Anything more complex (e.g. `\frac{1}{2}`) is left completely
//! untouched, including its delimiters, so unrenderable math stays visibly
//! unrendered rather than half-mangled.
//!
//! **Call-site contract**: this module only operates on already-extracted
//! prose *text* nodes. Callers (see `markdown_renderer.rs`) must never run
//! it over code spans, fenced code blocks, or link/image URLs.

/// `(macro, unicode replacement)`. One flat table so it's trivial to extend.
/// Matching sorts this by macro length (longest first) so e.g.
/// `\longrightarrow` is matched before `\rightarrow` or `\to`.
const MACROS: &[(&str, &str)] = &[
    // Arrows
    (r"\longrightarrow", "\u{2192}"), // →
    (r"\rightarrow", "\u{2192}"),
    (r"\to", "\u{2192}"),
    (r"\longleftarrow", "\u{2190}"), // ←
    (r"\leftarrow", "\u{2190}"),
    (r"\gets", "\u{2190}"),
    (r"\longleftrightarrow", "\u{2194}"), // ↔
    (r"\leftrightarrow", "\u{2194}"),
    (r"\Rightarrow", "\u{21D2}"), // ⇒
    (r"\implies", "\u{21D2}"),
    (r"\Leftarrow", "\u{21D0}"), // ⇐
    (r"\impliedby", "\u{21D0}"),
    (r"\Leftrightarrow", "\u{21D4}"), // ⇔
    (r"\iff", "\u{21D4}"),
    (r"\longmapsto", "\u{21A6}"), // ↦
    (r"\mapsto", "\u{21A6}"),
    (r"\uparrow", "\u{2191}"),      // ↑
    (r"\downarrow", "\u{2193}"),    // ↓
    (r"\updownarrow", "\u{2195}"),  // ↕
    (r"\nearrow", "\u{2197}"),      // ↗
    (r"\searrow", "\u{2198}"),      // ↘
    (r"\swarrow", "\u{2199}"),      // ↙
    (r"\nwarrow", "\u{2196}"),      // ↖
    (r"\hookrightarrow", "\u{21AA}"), // ↪
    (r"\hookleftarrow", "\u{21A9}"), // ↩
    // Common operators seen alongside arrows
    (r"\times", "\u{00D7}"),  // ×
    (r"\cdot", "\u{00B7}"),   // ·
    (r"\pm", "\u{00B1}"),     // ±
    (r"\leq", "\u{2264}"),    // ≤
    (r"\geq", "\u{2265}"),    // ≥
    (r"\neq", "\u{2260}"),    // ≠
    (r"\approx", "\u{2248}"), // ≈
    (r"\equiv", "\u{2261}"),  // ≡
    (r"\infty", "\u{221E}"),  // ∞
    (r"\ldots", "\u{2026}"),  // …
    (r"\cdots", "\u{22EF}"),  // ⋯
];

/// Sorted once and cached: this runs per text run on every markdown
/// re-render, and streaming re-renders the whole document on every chunk, so
/// resorting the table from scratch on each call (as this used to) turned
/// into thousands of redundant sorts on a long message — enough to peg the
/// UI thread and stall painting while the stream kept running in the
/// background.
fn sorted_macros() -> &'static [(&'static str, &'static str)] {
    static SORTED: std::sync::OnceLock<Vec<(&'static str, &'static str)>> =
        std::sync::OnceLock::new();
    SORTED.get_or_init(|| {
        let mut v: Vec<(&'static str, &'static str)> = MACROS.to_vec();
        // Longest macro name first, so a shorter macro that happens to be a
        // prefix of a longer one never wins the match.
        v.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
        v
    })
}

/// Tries to match a macro at the very start of `s`. Returns the byte length
/// consumed and the Unicode replacement on success.
///
/// A macro only matches when it is not immediately followed by an ASCII
/// alphabetic character — this is what keeps `\tomorrow` from being read as
/// `\to` + `morrow`.
fn match_macro_at(s: &str, macros: &[(&'static str, &'static str)]) -> Option<(usize, &'static str)> {
    for (m, rep) in macros {
        if let Some(rest) = s.strip_prefix(m) {
            let next_is_alpha = rest
                .chars()
                .next()
                .map(|c| c.is_ascii_alphabetic())
                .unwrap_or(false);
            if !next_is_alpha {
                return Some((m.len(), rep));
            }
        }
    }
    None
}

/// Substitutes macros over a flat run of text (no delimiter awareness).
/// Returns the substituted text and whether any `\macro`-shaped sequence
/// was left unresolved (i.e. a backslash immediately followed by an ASCII
/// letter that didn't match any known macro, or matched but was rejected
/// because it was followed by more letters).
fn substitute_plain(s: &str, macros: &[(&'static str, &'static str)]) -> (String, bool) {
    let mut out = String::with_capacity(s.len());
    let mut residual = false;
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < s.len() {
        if s[i..].starts_with("\\\\") {
            // Escaped backslash: emit both chars verbatim, never a macro.
            out.push_str("\\\\");
            i += 2;
            continue;
        }
        if bytes[i] == b'\\' {
            if let Some((len, rep)) = match_macro_at(&s[i..], macros) {
                out.push_str(rep);
                i += len;
                continue;
            }
            let rest = &s[i + 1..];
            let next_is_alpha = rest
                .chars()
                .next()
                .map(|c| c.is_ascii_alphabetic())
                .unwrap_or(false);
            if next_is_alpha {
                residual = true;
            }
            out.push('\\');
            i += 1;
            continue;
        }
        let ch = s[i..].chars().next().expect("i < s.len()");
        out.push(ch);
        i += ch.len_utf8();
    }
    (out, residual)
}

/// Substitutes known LaTeX macros (arrows, common operators) with their
/// Unicode equivalents in a run of prose text.
///
/// - `\to`, `\Rightarrow`, etc. are replaced directly, wherever they appear.
/// - `$...$` and `\(...\)` wrappers are stripped ONLY when everything inside
///   fully resolved (no `\macro` left after substitution) — otherwise the
///   whole delimited expression, delimiters included, is left byte-for-byte
///   untouched.
/// - `$$...$$` display math is always left completely untouched.
/// - An escaped backslash (`\\`) never starts a macro match.
///
/// Only apply this to prose text nodes — never to code spans, code blocks,
/// or link/image URLs.
pub fn render_latex_symbols(s: &str) -> String {
    let macros = sorted_macros();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i..].starts_with("$$") {
            if let Some(rel) = s[i + 2..].find("$$") {
                let end = i + 2 + rel + 2;
                out.push_str(&s[i..end]);
                i = end;
            } else {
                out.push_str(&s[i..]);
                break;
            }
            continue;
        }
        if s[i..].starts_with('$') {
            if let Some(rel) = s[i + 1..].find('$') {
                let close = i + 1 + rel;
                let inner = &s[i + 1..close];
                let (substituted, residual) = substitute_plain(inner, macros);
                if residual {
                    out.push_str(&s[i..=close]);
                } else {
                    out.push_str(&substituted);
                }
                i = close + 1;
            } else {
                out.push('$');
                i += 1;
            }
            continue;
        }
        if s[i..].starts_with("\\(") {
            if let Some(rel) = s[i + 2..].find("\\)") {
                let close = i + 2 + rel;
                let inner = &s[i + 2..close];
                let (substituted, residual) = substitute_plain(inner, macros);
                if residual {
                    out.push_str(&s[i..close + 2]);
                } else {
                    out.push_str(&substituted);
                }
                i = close + 2;
            } else {
                out.push_str("\\(");
                i += 2;
            }
            continue;
        }
        if s[i..].starts_with("\\\\") {
            out.push_str("\\\\");
            i += 2;
            continue;
        }
        if s.as_bytes()[i] == b'\\' {
            if let Some((len, rep)) = match_macro_at(&s[i..], macros) {
                out.push_str(rep);
                i += len;
                continue;
            }
            out.push('\\');
            i += 1;
            continue;
        }
        let ch = s[i..].chars().next().expect("i < s.len()");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arrow_families() {
        assert_eq!(render_latex_symbols(r"\to"), "\u{2192}");
        assert_eq!(render_latex_symbols(r"\rightarrow"), "\u{2192}");
        assert_eq!(render_latex_symbols(r"\longrightarrow"), "\u{2192}");
        assert_eq!(render_latex_symbols(r"\leftarrow"), "\u{2190}");
        assert_eq!(render_latex_symbols(r"\longleftarrow"), "\u{2190}");
        assert_eq!(render_latex_symbols(r"\gets"), "\u{2190}");
        assert_eq!(render_latex_symbols(r"\leftrightarrow"), "\u{2194}");
        assert_eq!(render_latex_symbols(r"\longleftrightarrow"), "\u{2194}");
        assert_eq!(render_latex_symbols(r"\Rightarrow"), "\u{21D2}");
        assert_eq!(render_latex_symbols(r"\implies"), "\u{21D2}");
        assert_eq!(render_latex_symbols(r"\Leftarrow"), "\u{21D0}");
        assert_eq!(render_latex_symbols(r"\impliedby"), "\u{21D0}");
        assert_eq!(render_latex_symbols(r"\Leftrightarrow"), "\u{21D4}");
        assert_eq!(render_latex_symbols(r"\iff"), "\u{21D4}");
        assert_eq!(render_latex_symbols(r"\mapsto"), "\u{21A6}");
        assert_eq!(render_latex_symbols(r"\longmapsto"), "\u{21A6}");
        assert_eq!(render_latex_symbols(r"\uparrow"), "\u{2191}");
        assert_eq!(render_latex_symbols(r"\downarrow"), "\u{2193}");
        assert_eq!(render_latex_symbols(r"\updownarrow"), "\u{2195}");
        assert_eq!(render_latex_symbols(r"\nearrow"), "\u{2197}");
        assert_eq!(render_latex_symbols(r"\searrow"), "\u{2198}");
        assert_eq!(render_latex_symbols(r"\swarrow"), "\u{2199}");
        assert_eq!(render_latex_symbols(r"\nwarrow"), "\u{2196}");
        assert_eq!(render_latex_symbols(r"\hookrightarrow"), "\u{21AA}");
        assert_eq!(render_latex_symbols(r"\hookleftarrow"), "\u{21A9}");
    }

    #[test]
    fn operators() {
        assert_eq!(render_latex_symbols(r"\times"), "\u{00D7}");
        assert_eq!(render_latex_symbols(r"\cdot"), "\u{00B7}");
        assert_eq!(render_latex_symbols(r"\pm"), "\u{00B1}");
        assert_eq!(render_latex_symbols(r"\leq"), "\u{2264}");
        assert_eq!(render_latex_symbols(r"\geq"), "\u{2265}");
        assert_eq!(render_latex_symbols(r"\neq"), "\u{2260}");
        assert_eq!(render_latex_symbols(r"\approx"), "\u{2248}");
        assert_eq!(render_latex_symbols(r"\equiv"), "\u{2261}");
        assert_eq!(render_latex_symbols(r"\infty"), "\u{221E}");
        assert_eq!(render_latex_symbols(r"\ldots"), "\u{2026}");
        assert_eq!(render_latex_symbols(r"\cdots"), "\u{22EF}");
    }

    #[test]
    fn longest_match_wins() {
        assert_eq!(
            render_latex_symbols(r"a \longrightarrow b \rightarrow c \to d"),
            "a \u{2192} b \u{2192} c \u{2192} d"
        );
    }

    #[test]
    fn word_boundary_guards_against_false_positives() {
        // `\tomorrow` must never be read as `\to` + `morrow`.
        assert_eq!(render_latex_symbols(r"\tomorrow"), r"\tomorrow");
        assert_eq!(
            render_latex_symbols(r"see you \tomorrow ok"),
            r"see you \tomorrow ok"
        );
    }

    #[test]
    fn dollar_delimiters_strip_when_fully_resolved() {
        assert_eq!(render_latex_symbols(r"$x \to y$"), "x \u{2192} y");
    }

    #[test]
    fn paren_delimiters_strip_when_fully_resolved() {
        assert_eq!(render_latex_symbols(r"\(x \to y\)"), "x \u{2192} y");
    }

    #[test]
    fn unrenderable_macro_leaves_dollar_expression_untouched() {
        assert_eq!(render_latex_symbols(r"$\frac{1}{2}$"), r"$\frac{1}{2}$");
    }

    #[test]
    fn display_math_left_entirely_alone() {
        assert_eq!(
            render_latex_symbols(r"$$\to \frac{1}{2}$$"),
            r"$$\to \frac{1}{2}$$"
        );
    }

    #[test]
    fn escaped_backslash_never_substitutes() {
        assert_eq!(render_latex_symbols(r"\\to"), r"\\to");
        assert_eq!(render_latex_symbols(r"a \\to b"), r"a \\to b");
    }

    #[test]
    fn plain_prose_is_unchanged() {
        let prose = "Hobbes is a local-first assistant that keeps history on device.";
        assert_eq!(render_latex_symbols(prose), prose);
    }

    #[test]
    fn mixed_prose_with_arrow() {
        assert_eq!(
            render_latex_symbols("Data flows A \\to B \\to C in the pipeline."),
            "Data flows A \u{2192} B \u{2192} C in the pipeline."
        );
    }
}
