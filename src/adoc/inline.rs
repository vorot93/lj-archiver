use crate::adoc::escape::composed_inline_escape_offsets;
use std::{
    mem,
    ops::{Deref, Range},
};

/// The buffer every inline emission path writes into.
///
/// One buffer holds the AsciiDoc text of one rendering scope — a paragraph's
/// inline children, a blockquote's block children, the document root's inline
/// run — together with the boundaries of the fragments that produced it. Each
/// append is one fragment: an authored run the run neutralizer already
/// inspected, or a renderer-generated construct the renderer meant to be live.
///
/// # Why the boundaries are the security boundary
///
/// Authored text used to be neutralized one run at a time, so a generated
/// fragment splitting an authored run left a construct that neither half held
/// and nothing ever re-examined (`<p>+++<i></i>&lt;script&gt;…+++</p>` emitted a
/// live passthrough). Re-running the neutralizers over the whole composed line
/// is not the fix either: that escapes the renderer's *own* `image:`, `xref:`
/// and `{lj-archiver-post-link-prefix}` output and breaks every archive link.
///
/// The rule this buffer enforces instead is:
///
/// > A construct one single fragment contains keeps exactly the meaning that
/// > fragment's producer gave it. A construct no single fragment contains
/// > belongs to no producer, and is escaped.
///
/// Both halves follow from one property rather than from a list of cases. The
/// renderer emits each of its own constructs as one fragment, so a generated
/// construct is always contained and can never be escaped — over-claiming is
/// structurally impossible, not merely untested. An authored construct that
/// survives to compose across a boundary is by definition not contained, so it
/// is always escaped, whichever side of the boundary it started on.
#[derive(Default)]
pub(crate) struct InlineBuffer {
    text: String,
    /// The emitted fragments, in order and covering `text` exactly.
    fragments: Vec<Range<usize>>,
    /// Offset just past the last finished block. Block bytes are verbatim —
    /// a listing keeps its source, a `++++` passthrough keeps its delimiters —
    /// so nothing is ever rewritten inside them and no construct may compose
    /// across them.
    block_end: usize,
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
        self.fragments.clear();
        self.block_end = 0;
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
        let start = self.text.len();
        append_inline_fragment(&mut self.text, fragment);
        self.close_fragment(start);
    }

    /// Append one emitted fragment without collapsing its leading spaces —
    /// what a rendered block child contributes, where the leading whitespace of
    /// the rendered text is already the child renderer's own decision.
    pub(crate) fn push_verbatim(&mut self, fragment: &str) {
        let start = self.text.len();
        self.text.push_str(fragment);
        self.close_fragment(start);
    }

    /// Insert renderer-owned bytes at `at` as a fragment of their own, leaving
    /// everything else in place.
    pub(crate) fn insert_at(&mut self, at: usize, fragment: &str) {
        self.splice_in(at, fragment);
        self.neutralize_composed_inline_syntax();
    }

    /// Replace everything from `at` onwards with `text`, verbatim, as one
    /// fragment.
    ///
    /// The line rewrites this serves — the description-list marker replacement
    /// and the `pass:[]` line guard — reason about the finished line as a
    /// whole, so the line they hand back is one producer's output and is
    /// tracked as one fragment.
    pub(crate) fn replace_from(&mut self, at: usize, text: &str) {
        self.truncate(at);
        let start = self.text.len();
        self.text.push_str(text);
        self.close_fragment(start);
    }

    /// Drop trailing whitespace, which every block boundary does before it
    /// opens its own blank line.
    pub(crate) fn trim_end_in_place(&mut self) {
        self.truncate(self.text.trim_end().len());
    }

    fn truncate(&mut self, length: usize) {
        self.text.truncate(length);
        self.fragments.retain(|fragment| fragment.start < length);
        for fragment in &mut self.fragments {
            fragment.end = fragment.end.min(length);
        }
        self.block_end = self.block_end.min(length);
    }

    fn close_fragment(&mut self, start: usize) {
        self.fragments.push(start..self.text.len());

        // A blank line ends the block. Whatever produced it — a rendered
        // paragraph, a listing, a `++++` passthrough, a collapsed `<br>` run —
        // its bytes are finished, and no inline construct may reach across it.
        // The scan back over one byte catches a `\n` that the previous fragment
        // ended with.
        if self.text[start.saturating_sub(1)..].contains("\n\n") {
            self.block_end = self.text.len();
        }

        self.neutralize_composed_inline_syntax();
    }

    /// Escape every inline construct on the current block that no single
    /// fragment contains. See the type documentation for why that predicate is
    /// the whole rule.
    fn neutralize_composed_inline_syntax(&mut self) {
        let window = self.block_end..self.text.len();
        if window.is_empty() {
            return;
        }

        let offsets =
            composed_inline_escape_offsets(&self.text, window, &|range| self.contains(range));

        // Descending, so an earlier offset is still the offset it was measured
        // at once a later one has been spliced in.
        for offset in offsets.into_iter().rev() {
            self.splice_in(offset, "\\");
        }
    }

    /// True when one single emitted fragment holds the whole of `range`.
    fn contains(&self, range: Range<usize>) -> bool {
        let index = self
            .fragments
            .partition_point(|fragment| fragment.end <= range.start);

        self.fragments
            .get(index)
            .is_some_and(|fragment| range.end <= fragment.end)
    }

    fn splice_in(&mut self, at: usize, fragment: &str) {
        let length = fragment.len();
        self.text.insert_str(at, fragment);

        let mut spliced = Vec::with_capacity(self.fragments.len() + 2);
        for existing in mem::take(&mut self.fragments) {
            if existing.end <= at {
                spliced.push(existing);
            } else if at <= existing.start {
                spliced.push(existing.start + length..existing.end + length);
            } else {
                // The insertion lands inside this fragment and splits it: the
                // inserted bytes are their own producer's, and neither half
                // keeps the other's company.
                spliced.push(existing.start..at);
                spliced.push(at + length..existing.end + length);
            }
        }

        let index = spliced.partition_point(|existing| existing.end <= at);
        spliced.insert(index, at..at + length);
        self.fragments = spliced;

        if at < self.block_end {
            self.block_end += length;
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Fragment containment is what decides every escape, so pin the predicate
    /// itself: a range inside one fragment is contained, a range that reaches
    /// across a boundary is not — including across a zero-length fragment,
    /// which is exactly what an empty generated wrapper (`<i></i>`) emits.
    #[test]
    fn only_a_range_inside_one_fragment_counts_as_contained() {
        let mut buffer = InlineBuffer::new();
        buffer.push("abc");
        buffer.push("");
        buffer.push("def");

        assert_eq!(&*buffer, "abcdef");
        for contained in [0..3, 1..3, 3..6, 4..6] {
            assert!(buffer.contains(contained.clone()), "{contained:?}");
        }
        for split in [0..4, 2..4, 0..6, 1..6] {
            assert!(!buffer.contains(split.clone()), "{split:?}");
        }
    }

    /// An insertion splits the fragment it lands in, so neither half keeps the
    /// other's company afterwards and the inserted bytes own themselves.
    #[test]
    fn an_insertion_splits_the_fragment_it_lands_in() {
        let mut buffer = InlineBuffer::new();
        buffer.push("abcdef");
        buffer.splice_in(3, "XY");

        assert_eq!(&*buffer, "abcXYdef");
        assert!(buffer.contains(0..3));
        assert!(buffer.contains(3..5));
        assert!(buffer.contains(5..8));
        assert!(!buffer.contains(2..4));
        assert!(!buffer.contains(0..8));
    }

    /// A blank line ends the block: block bytes are verbatim, so the scan
    /// window never reaches back into them.
    #[test]
    fn a_blank_line_ends_the_block_even_across_a_fragment_boundary() {
        let mut buffer = InlineBuffer::new();
        buffer.push("a\n");
        assert_eq!(buffer.block_end, 0);

        buffer.push("\nb");
        assert_eq!(buffer.block_end, buffer.len());
    }
}
