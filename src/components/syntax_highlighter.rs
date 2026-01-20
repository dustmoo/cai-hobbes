use lazy_static::lazy_static;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::html::{styled_line_to_highlighted_html, IncludeBackground};
use syntect::parsing::SyntaxSet;

lazy_static! {
    pub static ref SYNTAX_SET: SyntaxSet = SyntaxSet::load_defaults_newlines();
    pub static ref THEME_SET: ThemeSet = ThemeSet::load_defaults();
    pub static ref THEME: &'static Theme = &THEME_SET.themes["base16-ocean.dark"];
}

pub fn highlight_json(json: String) -> String {
    let syntax = SYNTAX_SET
        .find_syntax_by_extension("json")
        .unwrap_or_else(|| SYNTAX_SET.find_syntax_plain_text());
    let mut h = HighlightLines::new(syntax, &THEME);
    let mut html = String::new();

    for line in json.lines() {
        let regions = h.highlight_line(line, &SYNTAX_SET).unwrap_or_default();
        let html_line = styled_line_to_highlighted_html(&regions, IncludeBackground::No)
            .unwrap_or_else(|_| line.to_string());
        html.push_str(&html_line);
        html.push('\n');
    }

    if html.ends_with('\n') {
        html.pop();
    }
    html
}
