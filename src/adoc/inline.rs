use crate::adoc::escape::{
    composed_inline_escape_offsets, should_preserve_unwrapped_inline_output,
};
use std::{
    mem,
    ops::{Deref, Range},
};

/// Which producer owns a run of rendered bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Origin {
    /// Text that came out of the post's own HTML — a plain-text run, a macro
    /// label, an image's alt text, the interior of a `*bold*` wrapper. The only
    /// place an escape may ever be inserted.
    Authored,
    /// Syntax the renderer composed: macro names and delimiters, sanitized
    /// targets, the `link=` attribute, guards, finished blocks. Never rewritten.
    Generated,
}

/// Rendered inline output together with the origin of every byte of it.
///
/// A rendered fragment is almost never uniformly one or the other: an anchor is
/// a generated `target[` prefix, an authored label and a generated `]`; a
/// linked image is a generated `image:target[`, an authored alt and a generated
/// `,link=target]`. Carrying that structure — rather than treating a whole
/// fragment as opaque — is what keeps an escape out of the renderer's own
/// attribute lists while still allowing one inside a `*bold*` wrapper, whose
/// interior is authored.
#[derive(Clone, Debug, Default)]
pub(crate) struct Spanned {
    text: String,
    /// Origin-tagged spans, in order, covering `text` exactly.
    spans: Vec<(Range<usize>, Origin)>,
}

impl Spanned {
    pub(crate) fn authored(text: impl Into<String>) -> Self {
        Self::uniform(text.into(), Origin::Authored)
    }

    pub(crate) fn generated(text: impl Into<String>) -> Self {
        Self::uniform(text.into(), Origin::Generated)
    }

    fn uniform(text: String, origin: Origin) -> Self {
        let spans = if text.is_empty() {
            Vec::new()
        } else {
            vec![(0..text.len(), origin)]
        };

        Self { text, spans }
    }

    pub(crate) fn push_authored(&mut self, text: &str) {
        self.push_run(text, Origin::Authored);
    }

    pub(crate) fn push_generated(&mut self, text: &str) {
        self.push_run(text, Origin::Generated);
    }

    fn push_run(&mut self, text: &str, origin: Origin) {
        if text.is_empty() {
            return;
        }

        let start = self.text.len();
        self.text.push_str(text);
        self.spans.push((start..self.text.len(), origin));
    }

    pub(crate) fn push_spanned(&mut self, other: &Self) {
        let offset = self.text.len();
        self.text.push_str(&other.text);
        self.spans.extend(
            other
                .spans
                .iter()
                .map(|(range, origin)| (range.start + offset..range.end + offset, *origin)),
        );
    }

    /// The sub-slice `range`, keeping each byte's origin.
    pub(crate) fn slice(&self, range: Range<usize>) -> Self {
        let mut sliced = Self {
            text: self.text[range.clone()].to_string(),
            spans: Vec::new(),
        };

        for (span, origin) in &self.spans {
            let start = span.start.max(range.start);
            let end = span.end.min(range.end);
            if start < end {
                sliced
                    .spans
                    .push((start - range.start..end - range.start, *origin));
            }
        }

        sliced
    }

    /// `str::trim`, keeping origins.
    pub(crate) fn trim(&self) -> Self {
        let trimmed = self.text.trim();
        if trimmed.is_empty() {
            return Self::default();
        }

        let start = trimmed.as_ptr() as usize - self.text.as_ptr() as usize;
        self.slice(start..start + trimmed.len())
    }
}

impl Deref for Spanned {
    type Target = str;

    fn deref(&self) -> &str {
        &self.text
    }
}

/// Wraps rendered inline output in renderer-owned delimiters, keeping the
/// wrapped text's own origins so an authored interior stays escapable.
pub(crate) fn render_inline_wrapper(text: &Spanned, prefix: &str, suffix: &str) -> Spanned {
    if should_preserve_unwrapped_inline_output(text) {
        return text.clone();
    }

    render_inline_wrapper_with_label(text, &text.trim(), prefix, suffix)
}

pub(crate) fn render_inline_wrapper_with_label(
    text: &Spanned,
    label: &Spanned,
    prefix: &str,
    suffix: &str,
) -> Spanned {
    let mut output = Spanned::default();

    if label.is_empty() {
        return output;
    }

    if text.starts_with(' ') {
        output.push_generated(" ");
    }

    output.push_generated(prefix);
    output.push_spanned(label);
    output.push_generated(suffix);

    if text.ends_with(' ') {
        output.push_generated(" ");
    }

    output
}

/// The buffer every inline emission path writes into.
///
/// One buffer holds the AsciiDoc text of one rendering scope — a paragraph's
/// inline children, a blockquote's block children, the document root's inline
/// run — together with the origin-tagged spans that composed it.
///
/// # Why the spans are the security boundary
///
/// Authored text used to be neutralized one run at a time, so a generated
/// fragment splitting an authored run left a construct that neither half held
/// and nothing ever re-examined (`<p>+++<i></i>&lt;script&gt;…+++</p>` emitted a
/// live passthrough). Re-running the neutralizers over the whole composed line
/// is not the fix either: that escapes the renderer's *own* `image:`, `xref:`
/// and `{lj-archiver-post-link-prefix}` output and breaks every archive link.
///
/// The rule this buffer enforces has two clauses, and both are needed:
///
/// > An escape may only ever be inserted at an offset whose origin is
/// > [`Origin::Authored`]; and a construct that one single span holds outright
/// > keeps the meaning that span's producer gave it, so it is left alone.
///
/// The first clause is what bounds the damage: no byte the renderer composed is
/// ever rewritten, so a `link=` attribute value, a macro name and a sanitized
/// target cannot be corrupted no matter what authored text abuts them. It is a
/// property of *where* the escape may land, not a claim about which constructs
/// the scanners recognize — a construct whose vocabulary the scanners do not
/// cover is still missed, and the five scanners are the whole guarantee there.
///
/// The second clause is what keeps an authored run's own already-neutralized
/// output — and the `pass:[:]` a description-list rewrite leaves inside it —
/// from being escaped a second time.
#[derive(Default)]
pub(crate) struct InlineBuffer {
    text: String,
    /// The emitted spans, in order and covering `text` exactly.
    spans: Vec<(Range<usize>, Origin)>,
    /// Offset just past the last finished block. Block bytes are verbatim —
    /// a listing keeps its source, a `++++` passthrough keeps its delimiters —
    /// so nothing is ever rewritten inside them and no construct may compose
    /// across them.
    block_end: usize,
    /// Where the `+++` pair scan resumes. Every pair before it is settled, so
    /// an append only has to look at what it could not have seen — without
    /// this the block-wide pair scan makes a long paragraph quadratic.
    passthrough_from: usize,
}

impl InlineBuffer {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn into_spanned(self) -> Spanned {
        Spanned {
            text: self.text,
            spans: self.spans,
        }
    }

    pub(crate) fn clear(&mut self) {
        self.text.clear();
        self.spans.clear();
        self.block_end = 0;
        self.passthrough_from = 0;
    }

    /// Byte offset at which the current logical line starts.
    pub(crate) fn line_start(&self) -> usize {
        self.text.rfind('\n').map_or(0, |index| index + 1)
    }

    /// The current logical line: everything after the last line break.
    pub(crate) fn line(&self) -> &str {
        &self.text[self.line_start()..]
    }

    /// Append one emitted fragment, keeping its origins.
    ///
    /// A fragment's leading spaces collapse into the space or line break
    /// already ending the buffer, which is what keeps ` +\n` hard breaks and
    /// wrapper-retained spaces from doubling up.
    pub(crate) fn push(&mut self, fragment: &Spanned) {
        let skipped = self.collapsed_leading_spaces(fragment);
        self.push_spanned(&fragment.slice(skipped..fragment.len()));
    }

    /// Append one emitted fragment without collapsing its leading spaces —
    /// what a rendered block child contributes, where the leading whitespace of
    /// the rendered text is already the child renderer's own decision.
    pub(crate) fn push_verbatim(&mut self, fragment: &Spanned) {
        self.push_spanned(fragment);
    }

    pub(crate) fn push_authored(&mut self, text: &str) {
        self.push(&Spanned::authored(text));
    }

    pub(crate) fn push_generated(&mut self, text: &str) {
        self.push(&Spanned::generated(text));
    }

    fn collapsed_leading_spaces(&self, fragment: &Spanned) -> usize {
        if !(self.text.ends_with(' ') || self.text.ends_with('\n')) {
            return 0;
        }

        fragment.bytes().take_while(|byte| *byte == b' ').count()
    }

    fn push_spanned(&mut self, fragment: &Spanned) {
        let start = self.text.len();
        self.text.push_str(fragment);
        self.spans.extend(
            fragment
                .spans
                .iter()
                .map(|(range, origin)| (range.start + start..range.end + start, *origin)),
        );
        self.close_append(start);
    }

    /// Insert renderer-owned bytes at `at` as a span of their own, leaving
    /// everything else in place.
    pub(crate) fn insert_at(&mut self, at: usize, fragment: &str) {
        self.splice_in(at, fragment, Origin::Generated);
        self.neutralize_composed_inline_syntax();
    }

    /// Replace everything from `at` onwards with authored `text`, verbatim, as
    /// one span.
    ///
    /// The line rewrites this serves — the description-list marker replacement
    /// and the `pass:[]` line guard — reason about the finished line as a
    /// whole, so the line they hand back is one producer's output and is
    /// tracked as one span.
    pub(crate) fn replace_from(&mut self, at: usize, text: &str) {
        self.truncate(at);
        let start = self.text.len();
        self.text.push_str(text);
        if !text.is_empty() {
            self.spans.push((start..self.text.len(), Origin::Authored));
        }
        self.close_append(start);
    }

    /// Drop trailing whitespace, which every block boundary does before it
    /// opens its own blank line.
    pub(crate) fn trim_end_in_place(&mut self) {
        self.truncate(self.text.trim_end().len());
    }

    fn truncate(&mut self, length: usize) {
        self.text.truncate(length);
        self.spans.retain(|(range, _)| range.start < length);
        for (range, _) in &mut self.spans {
            range.end = range.end.min(length);
        }
        self.block_end = self.block_end.min(length);
        self.passthrough_from = self.passthrough_from.min(length);
    }

    fn close_append(&mut self, start: usize) {
        // A blank line ends the block. Whatever produced it — a rendered
        // paragraph, a listing, a `++++` passthrough, a collapsed `<br>` run —
        // its bytes are finished, and no inline construct may reach across it.
        // The scan back over one byte catches a `\n` that the previous append
        // ended with.
        if self.text[start.saturating_sub(1)..].contains("\n\n") {
            self.block_end = self.text.len();
            self.passthrough_from = self.text.len();
        }

        self.neutralize_composed_inline_syntax();
    }

    /// Escape every inline construct on the current line — and every `+++` pair
    /// on the current block — that the rule in this type's documentation says
    /// belongs to no producer.
    fn neutralize_composed_inline_syntax(&mut self) {
        // Attribute references, macros and passthrough macros are all
        // line-scoped by construction: a reference needs its `}` before the
        // line break, and a macro target stops at whitespace. An inline `+++`
        // pair is not — it legitimately spans the hard breaks inside one
        // paragraph — so it gets its own resumable window.
        let line = self.line_start().max(self.block_end)..self.text.len();
        let pairs = self.passthrough_from.max(self.block_end)..self.text.len();
        if line.is_empty() && pairs.is_empty() {
            return;
        }

        #[cfg(test)]
        record_scanned_bytes(line.len() + pairs.len());

        let escapes = composed_inline_escape_offsets(
            &self.text,
            line,
            pairs,
            &|range| self.holds(range),
            &|offset| self.origin(offset) == Some(Origin::Authored),
        );

        self.passthrough_from = escapes.pairs_resume_from;

        if escapes.offsets.is_empty() {
            return;
        }

        // Descending, so an earlier offset is still the offset it was measured
        // at once a later one has been spliced in.
        for offset in escapes.offsets.into_iter().rev() {
            self.splice_in(offset, "\\", Origin::Generated);
        }

        // A splice moved every later byte, so the resumable pair window can no
        // longer be trusted; the next scan settles it again from the block.
        self.passthrough_from = self.block_end;
    }

    /// True when one single emitted span holds the whole of `range`.
    fn holds(&self, range: Range<usize>) -> bool {
        self.span_at(range.start)
            .is_some_and(|(span, _)| range.end <= span.end)
    }

    /// The origin of the byte at `offset`.
    fn origin(&self, offset: usize) -> Option<Origin> {
        self.span_at(offset).map(|(_, origin)| origin)
    }

    fn span_at(&self, offset: usize) -> Option<(Range<usize>, Origin)> {
        let index = self.spans.partition_point(|(range, _)| range.end <= offset);

        self.spans.get(index).cloned()
    }

    fn splice_in(&mut self, at: usize, fragment: &str, origin: Origin) {
        let length = fragment.len();
        self.text.insert_str(at, fragment);

        let mut spliced = Vec::with_capacity(self.spans.len() + 2);
        for (existing, existing_origin) in mem::take(&mut self.spans) {
            if existing.end <= at {
                spliced.push((existing, existing_origin));
            } else if at <= existing.start {
                spliced.push((
                    existing.start + length..existing.end + length,
                    existing_origin,
                ));
            } else {
                // The insertion lands inside this span and splits it: the
                // inserted bytes are their own producer's, and neither half
                // keeps the other's company.
                spliced.push((existing.start..at, existing_origin));
                spliced.push((at + length..existing.end + length, existing_origin));
            }
        }

        let index = spliced.partition_point(|(existing, _)| existing.end <= at);
        spliced.insert(index, (at..at + length, origin));
        self.spans = spliced;

        if at < self.block_end {
            self.block_end += length;
        }
        if at < self.passthrough_from {
            self.passthrough_from += length;
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

/// Total bytes the composed scan has looked at, so a test can pin that the
/// renderer stays linear in body size rather than quadratic.
#[cfg(test)]
mod scan_budget {
    use std::cell::Cell;

    thread_local! {
        static SCANNED_BYTES: Cell<usize> = const { Cell::new(0) };
    }

    pub(super) fn record(bytes: usize) {
        SCANNED_BYTES.with(|scanned| scanned.set(scanned.get() + bytes));
    }

    pub(crate) fn take_scanned_bytes() -> usize {
        SCANNED_BYTES.with(|scanned| scanned.replace(0))
    }
}

#[cfg(test)]
use scan_budget::record as record_scanned_bytes;
#[cfg(test)]
pub(crate) use scan_budget::take_scanned_bytes;

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer_of(parts: &[(&str, Origin)]) -> InlineBuffer {
        let mut buffer = InlineBuffer::new();
        for (text, origin) in parts {
            let mut fragment = Spanned::default();
            match origin {
                Origin::Authored => fragment.push_authored(text),
                Origin::Generated => fragment.push_generated(text),
            }
            buffer.push(&fragment);
        }
        buffer
    }

    /// Span containment decides one clause of the rule, so pin the predicate:
    /// a range inside one span is held, a range that reaches across a boundary
    /// is not — including across a zero-length span, which is exactly what an
    /// empty generated wrapper (`<i></i>`) emits.
    #[test]
    fn only_a_range_inside_one_span_counts_as_held() {
        let buffer = buffer_of(&[
            ("abc", Origin::Authored),
            ("", Origin::Generated),
            ("def", Origin::Authored),
        ]);

        assert_eq!(&*buffer, "abcdef");
        for held in [0..3, 1..3, 3..6, 4..6] {
            assert!(buffer.holds(held.clone()), "{held:?}");
        }
        for split in [0..4, 2..4, 0..6, 1..6] {
            assert!(!buffer.holds(split.clone()), "{split:?}");
        }
    }

    /// The other clause: an offset carries the origin of the span it lands in,
    /// and that is what decides whether an escape may go there at all.
    #[test]
    fn an_offset_carries_the_origin_of_the_span_it_lands_in() {
        let buffer = buffer_of(&[
            ("gen", Origin::Generated),
            ("auth", Origin::Authored),
            ("gen2", Origin::Generated),
        ]);

        assert_eq!(buffer.origin(0), Some(Origin::Generated));
        assert_eq!(buffer.origin(2), Some(Origin::Generated));
        assert_eq!(buffer.origin(3), Some(Origin::Authored));
        assert_eq!(buffer.origin(6), Some(Origin::Authored));
        assert_eq!(buffer.origin(7), Some(Origin::Generated));
        assert_eq!(buffer.origin(11), None);
    }

    /// Wrapping keeps the wrapped text's origins, so a `*bold*` interior stays
    /// authored while its delimiters do not.
    #[test]
    fn a_wrapper_keeps_its_interior_authored_and_its_delimiters_generated() {
        let wrapped = render_inline_wrapper(&Spanned::authored("bold"), "*", "*");

        assert_eq!(&*wrapped, "*bold*");
        assert_eq!(
            wrapped.spans,
            vec![
                (0..1, Origin::Generated),
                (1..5, Origin::Authored),
                (5..6, Origin::Generated),
            ]
        );
    }

    /// An insertion splits the span it lands in, so neither half keeps the
    /// other's company afterwards and the inserted bytes own themselves.
    #[test]
    fn an_insertion_splits_the_span_it_lands_in() {
        let mut buffer = buffer_of(&[("abcdef", Origin::Authored)]);
        buffer.splice_in(3, "XY", Origin::Generated);

        assert_eq!(&*buffer, "abcXYdef");
        assert!(buffer.holds(0..3));
        assert!(buffer.holds(3..5));
        assert!(buffer.holds(5..8));
        assert!(!buffer.holds(2..4));
        assert_eq!(buffer.origin(3), Some(Origin::Generated));
        assert_eq!(buffer.origin(5), Some(Origin::Authored));
    }

    /// A blank line ends the block: block bytes are verbatim, so the scan
    /// window never reaches back into them.
    #[test]
    fn a_blank_line_ends_the_block_even_across_a_span_boundary() {
        let mut buffer = InlineBuffer::new();
        buffer.push_authored("a\n");
        assert_eq!(buffer.block_end, 0);

        buffer.push_authored("\nb");
        assert_eq!(buffer.block_end, buffer.len());
    }
}
