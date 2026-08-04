use std::ops::Deref;

/// The buffer every inline emission path writes into.
///
/// One buffer holds the AsciiDoc text of one rendering scope — a paragraph's
/// inline children, a blockquote's block children, the document root's inline
/// run. Routing every write through it keeps the emission path in one place
/// instead of spread across ad-hoc `String` manipulation at each call site.
#[derive(Default)]
pub(crate) struct InlineBuffer {
    text: String,
}

impl InlineBuffer {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn into_string(self) -> String {
        self.text
    }

    pub(crate) fn clear(&mut self) {
        self.text.clear();
    }

    /// Byte offset at which the current logical line starts.
    pub(crate) fn line_start(&self) -> usize {
        self.text.rfind('\n').map_or(0, |index| index + 1)
    }

    /// The current logical line: everything after the last line break.
    pub(crate) fn line(&self) -> &str {
        &self.text[self.line_start()..]
    }

    /// Append one emitted fragment.
    ///
    /// A fragment's leading spaces collapse into the space or line break
    /// already ending the buffer, which is what keeps ` +\n` hard breaks and
    /// wrapper-retained spaces from doubling up.
    pub(crate) fn push(&mut self, fragment: &str) {
        append_inline_fragment(&mut self.text, fragment);
    }

    /// Append one emitted fragment without collapsing its leading spaces —
    /// what a rendered block child contributes, where the leading whitespace of
    /// the rendered text is already the child renderer's own decision.
    pub(crate) fn push_verbatim(&mut self, fragment: &str) {
        self.text.push_str(fragment);
    }

    /// Insert renderer-owned bytes at `at`, leaving everything else in place.
    pub(crate) fn insert_at(&mut self, at: usize, fragment: &str) {
        self.text.insert_str(at, fragment);
    }

    /// Replace everything from `at` onwards with `text`, verbatim.
    pub(crate) fn replace_from(&mut self, at: usize, text: &str) {
        self.text.truncate(at);
        self.text.push_str(text);
    }

    /// Drop trailing whitespace, which every block boundary does before it
    /// opens its own blank line.
    pub(crate) fn trim_end_in_place(&mut self) {
        self.text.truncate(self.text.trim_end().len());
    }
}

impl Deref for InlineBuffer {
    type Target = str;

    fn deref(&self) -> &str {
        &self.text
    }
}

pub(crate) fn append_inline_fragment(output: &mut String, fragment: &str) {
    let mut characters = fragment.chars().peekable();

    if output.ends_with(' ') || output.ends_with('\n') {
        while matches!(characters.peek(), Some(' ')) {
            characters.next();
        }
    }

    output.extend(characters);
}
