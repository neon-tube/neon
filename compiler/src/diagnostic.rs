//! Diagnostic rendering, for every caller.
//!
//! Lives in the compiler rather than the CLI because `neon-cli` is a binary
//! crate: nothing can call into it, so a second copy of the renderer had to
//! exist for `compiler/tests`, and a drifting copy lets a test assert against
//! text no user ever sees.

use ariadne::{Color, Config, IndexType, Label, Report, ReportKind, Source};
use std::io::IsTerminal;
use std::ops::Range;
use std::path::Path;

/// Renders diagnostics against one source file.
pub struct Renderer<'a> {
    cache: (String, Source<&'a str>),
    src: &'a str,
    color: bool,
}

impl<'a> Renderer<'a> {
    /// Colour only when stderr is a terminal, so a redirect or a captured pipe
    /// reads as plain text and a substring match never sees an escape code.
    pub fn for_stderr(path: &Path, src: &'a str) -> Self {
        Self::new(path, src, std::io::stderr().is_terminal())
    }

    pub fn plain(path: &Path, src: &'a str) -> Self {
        Self::new(path, src, false)
    }

    fn new(path: &Path, src: &'a str, color: bool) -> Self {
        Renderer { cache: (path.display().to_string(), Source::from(src)), src, color }
    }

    pub fn render(&mut self, span: Range<usize>, msg: &str) -> String {
        let span = self.underline(span);
        let id = self.cache.0.clone();

        // The message goes on the report, not the label: it is the only string
        // an error carries, and ariadne would print it twice. An empty label
        // message is what earns the underline — ariadne draws none without one.
        let mut label = Label::new((id.clone(), span.clone())).with_message("");
        if self.color {
            label = label.with_color(Color::Red);
        }

        let report = Report::build(ReportKind::Error, (id, span))
            .with_config(
                // Spans are byte offsets. Ariadne counts characters unless told
                // otherwise, which puts the underline inside an `é`.
                Config::default().with_index_type(IndexType::Byte).with_color(self.color),
            )
            .with_message(msg)
            .with_label(label)
            .finish();

        let mut out = Vec::new();
        report.write(&mut self.cache, &mut out).expect("a Vec cannot fail to be written to");
        String::from_utf8(out).expect("ariadne emits UTF-8")
    }

    pub fn eprint(&mut self, span: Range<usize>, msg: &str) {
        eprint!("{}", self.render(span, msg));
    }

    /// A span that is empty, or that sits at end of input, underlines nothing.
    /// Widen it onto a real character first, as the caret always was.
    fn underline(&self, span: Range<usize>) -> Range<usize> {
        let start = span.start.min(self.src.len());
        let end = span.end.clamp(start, self.src.len());
        if start < end {
            return start..end;
        }
        match self.src[start..].chars().next() {
            Some(c) => start..start + c.len_utf8(),
            None => start - self.src[..start].chars().next_back().map_or(0, char::len_utf8)..start,
        }
    }
}
