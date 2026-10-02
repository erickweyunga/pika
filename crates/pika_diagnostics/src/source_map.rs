//! The source files of a program, in one space of byte offsets.
//!
//! Each file is given a base offset, and a [`Span`] in it covers `base + start..base + end`,
//! so a span alone tells which file it is in. A gap of one byte separates files, so that the
//! empty span at the end of a file belongs to it.

use crate::{LineIndex, Span};

/// Identifies a file in a [`SourceMap`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileId(pub u32);

/// A source file and where it starts in the offsets of the program.
#[derive(Debug)]
pub struct SourceFile {
    /// The name shown in diagnostics and panic reports, usually a path.
    pub name: String,
    /// The text.
    pub text: String,
    /// The offset of its first byte.
    pub base: u32,
    lines: LineIndex,
}

/// The source files of a program.
#[derive(Debug, Default)]
pub struct SourceMap {
    files: Vec<SourceFile>,
}

/// A position in a source file, for reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Location {
    /// The file.
    pub file: FileId,
    /// The 1-based line.
    pub line: u32,
    /// The 1-based column, in characters.
    pub column: u32,
}

impl SourceMap {
    /// Adds a file after the others; returns its id.
    ///
    /// # Panics
    ///
    /// Panics if the files of the program add up to 4 GiB or more.
    pub fn add(&mut self, name: impl Into<String>, text: impl Into<String>) -> FileId {
        let text = text.into();
        let base = self.files.last().map_or(0, |last| {
            last.base
                + u32::try_from(last.text.len()).expect("source files are smaller than 4 GiB")
                + 1
        });
        let id = FileId(u32::try_from(self.files.len()).expect("fewer than 2^32 files"));
        self.files.push(SourceFile {
            name: name.into(),
            lines: LineIndex::new(&text),
            text,
            base,
        });
        id
    }

    /// The file `id`.
    pub fn file(&self, id: FileId) -> &SourceFile {
        &self.files[id.0 as usize]
    }

    /// Every file, in order.
    pub fn files(&self) -> impl Iterator<Item = (FileId, &SourceFile)> {
        (0u32..).map(FileId).zip(&self.files)
    }

    /// The file that `span` is in.
    ///
    /// # Panics
    ///
    /// Panics if the map has no files.
    pub fn file_of(&self, span: Span) -> FileId {
        let index = self
            .files
            .partition_point(|file| file.base <= span.start)
            .saturating_sub(1);
        FileId(u32::try_from(index).expect("fewer than 2^32 files"))
    }

    /// `span` within its file, as offsets from the file's start.
    pub fn local(&self, span: Span) -> (FileId, Span) {
        let file = self.file_of(span);
        let base = self.file(file).base;
        (
            file,
            Span::new(
                span.start.saturating_sub(base),
                span.end.saturating_sub(base),
            ),
        )
    }

    /// The file, line and column of the start of `span`.
    pub fn locate(&self, span: Span) -> Location {
        let (file, local) = self.local(span);
        let (line, column) = self.file(file).lines.line_col(local);
        Location { file, line, column }
    }
}
