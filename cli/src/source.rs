use color_eyre::eyre::{Context, Result};
use std::path::Path;

pub fn read(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).wrap_err_with(|| format!("cannot read '{}'", path.display()))
}

pub struct Loc {
    pub line: usize,
    pub col: usize,
    /// Byte range of the line the offset falls on.
    pub line_span: std::ops::Range<usize>,
}

/// 1-based line and column for a byte offset.
///
/// Column counts characters, not bytes: source is UTF-8 and identifiers may be, so a
/// byte column would point into the middle of a `é` and mis-align the caret.
pub fn locate(src: &str, offset: usize) -> Loc {
    let offset = offset.min(src.len());
    let start = src[..offset].rfind('\n').map_or(0, |i| i + 1);
    let end = src[offset..].find('\n').map_or(src.len(), |i| offset + i);
    Loc {
        line: src[..start].bytes().filter(|b| *b == b'\n').count() + 1,
        col: src[start..offset].chars().count() + 1,
        line_span: start..end,
    }
}

/// Render one diagnostic against its source line.
///
/// ```text
/// error: <msg>
///  --> file.neon:9:20
///   |
/// 9 | mu type F = null | (F) -> i64
///   |                    ^
/// ```
pub fn render(path: &Path, src: &str, span: std::ops::Range<usize>, msg: &str) -> String {
    let loc = locate(src, span.start);
    let text = &src[loc.line_span.clone()];
    let gutter = loc.line.to_string();
    let pad = " ".repeat(gutter.len());

    // The span may run past the line; a caret is only ever drawn on one.
    let end = span.end.min(loc.line_span.end);
    let width = src[span.start..end.max(span.start)].chars().count().max(1);

    let mut out = String::new();
    out.push_str(&format!("error: {msg}\n"));
    out.push_str(&format!("{pad} --> {}:{}:{}\n", path.display(), loc.line, loc.col));
    out.push_str(&format!("{pad}  |\n"));
    out.push_str(&format!("{gutter} | {}\n", text.trim_end_matches('\r')));
    out.push_str(&format!("{pad}  | {}{}\n", " ".repeat(loc.col - 1), "^".repeat(width)));
    out
}
