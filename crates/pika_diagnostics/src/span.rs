use std::fmt;
use std::ops::Range;

/// A half-open byte range `start..end` into a source file.
///
/// Offsets are `u32`, so source files are limited to 4 GiB.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, PartialOrd, Ord)]
pub struct Span {
    /// Byte offset of the first byte in the span.
    pub start: u32,
    /// Byte offset one past the last byte in the span.
    pub end: u32,
}

impl Span {
    /// Creates a span from `start` to `end`.
    ///
    /// # Panics
    ///
    /// Panics if `start > end`.
    pub fn new(start: u32, end: u32) -> Self {
        assert!(start <= end, "invalid span {start}..{end}");
        Self { start, end }
    }

    /// Creates an empty span at `offset`.
    pub fn empty(offset: u32) -> Self {
        Self::new(offset, offset)
    }

    /// Length of the span in bytes.
    pub fn len(self) -> u32 {
        self.end - self.start
    }

    /// Returns true if the span covers no bytes.
    pub fn is_empty(self) -> bool {
        self.start == self.end
    }

    /// The smallest span covering both `self` and `other`.
    #[must_use]
    pub fn to(self, other: Span) -> Span {
        Span::new(self.start.min(other.start), self.end.max(other.end))
    }

    /// The span as a `usize` range, for slicing source text.
    pub fn range(self) -> Range<usize> {
        self.start as usize..self.end as usize
    }
}

impl fmt::Debug for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}..{}", self.start, self.end)
    }
}
