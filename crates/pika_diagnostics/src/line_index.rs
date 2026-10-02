use crate::Span;

/// Converts byte offsets into 1-based line and column numbers.
#[derive(Clone, Debug)]
pub struct LineIndex {
    /// Byte offset of the start of each line.
    line_starts: Vec<u32>,
    source: String,
}

impl LineIndex {
    /// Indexes the lines of `source`. Line breaks are `\n`, `\r\n` or `\r`.
    pub fn new(source: &str) -> Self {
        let mut line_starts = vec![0];
        let bytes = source.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'\n' => line_starts.push(offset(i + 1)),
                b'\r' if bytes.get(i + 1) != Some(&b'\n') => line_starts.push(offset(i + 1)),
                _ => {}
            }
            i += 1;
        }
        Self {
            line_starts,
            source: source.to_owned(),
        }
    }

    /// The 1-based line and column of the start of `span`. Columns count characters.
    pub fn line_col(&self, span: Span) -> (u32, u32) {
        let line = self
            .line_starts
            .partition_point(|&start| start <= span.start)
            - 1;
        let line_start = self.line_starts[line] as usize;
        let end = (span.start as usize).min(self.source.len());
        let column = self
            .source
            .get(line_start..end)
            .map_or(0, |text| text.chars().count());
        (offset(line + 1), offset(column + 1))
    }
}

fn offset(value: usize) -> u32 {
    u32::try_from(value).expect("source files are smaller than 4 GiB")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_and_columns() {
        let index = LineIndex::new("ab\ncé\r\nx\ry");
        assert_eq!(index.line_col(Span::empty(0)), (1, 1));
        assert_eq!(index.line_col(Span::empty(1)), (1, 2));
        assert_eq!(index.line_col(Span::empty(3)), (2, 1));
        assert_eq!(index.line_col(Span::empty(6)), (2, 3)); // after the two-byte `é`
        assert_eq!(index.line_col(Span::empty(8)), (3, 1));
        assert_eq!(index.line_col(Span::empty(10)), (4, 1));
    }
}
