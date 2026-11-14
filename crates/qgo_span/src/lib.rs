//! `qgo_span`: Source spans, file IDs, string interning, and rich Ariadne-style diagnostics.

use std::collections::HashMap;
use std::fmt;

/// Unique identifier for a source file in the [`SourceMap`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileId(pub u32);

/// Byte offset span `[start, end)` within a specific [`FileId`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span {
    pub file: FileId,
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub const fn new(file: FileId, start: u32, end: u32) -> Self {
        Self { file, start, end }
    }

    pub const fn dummy() -> Self {
        Self {
            file: FileId(0),
            start: 0,
            end: 0,
        }
    }

    pub fn merge(self, other: Span) -> Span {
        debug_assert_eq!(self.file, other.file);
        Span {
            file: self.file,
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }

    pub fn len(self) -> usize {
        (self.end.saturating_sub(self.start)) as usize
    }

    pub fn is_empty(self) -> bool {
        self.len() == 0
    }
}

/// Wrapper pairing a node `T` with its source [`Span`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spanned<T> {
    pub node: T,
    pub span: Span,
}

impl<T> Spanned<T> {
    pub const fn new(node: T, span: Span) -> Self {
        Self { node, span }
    }
}

/// Interned string symbol handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Symbol(pub u32);

/// Lightweight string interner (analogous to `lasso::Rodeo`).
#[derive(Debug, Default, Clone)]
pub struct Interner {
    map: HashMap<String, Symbol>,
    strings: Vec<String>,
}

impl Interner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get_or_intern(&mut self, s: &str) -> Symbol {
        if let Some(&sym) = self.map.get(s) {
            return sym;
        }
        let sym = Symbol(self.strings.len() as u32);
        let owned = s.to_owned();
        self.strings.push(owned.clone());
        self.map.insert(owned, sym);
        sym
    }

    pub fn resolve(&self, sym: Symbol) -> &str {
        self.strings
            .get(sym.0 as usize)
            .map(String::as_str)
            .unwrap_or("<invalid-symbol>")
    }
}

/// A loaded source file with precomputed line-start byte offsets.
#[derive(Debug, Clone)]
pub struct SourceFile {
    pub id: FileId,
    pub name: String,
    pub src: String,
    line_starts: Vec<u32>,
}

impl SourceFile {
    pub fn new(id: FileId, name: impl Into<String>, src: impl Into<String>) -> Self {
        let name = name.into();
        let src = src.into();
        let mut line_starts = vec![0];
        for (idx, b) in src.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push((idx + 1) as u32);
            }
        }
        Self {
            id,
            name,
            src,
            line_starts,
        }
    }

    /// Returns 1-indexed `(line, column)` for a byte offset.
    pub fn line_col(&self, offset: u32) -> (usize, usize) {
        let clamped = offset.min(self.src.len() as u32);
        let line_idx = match self.line_starts.binary_search(&clamped) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        };
        let line_start = self.line_starts[line_idx];
        let col = self.src[line_start as usize..clamped as usize]
            .chars()
            .count()
            + 1;
        (line_idx + 1, col)
    }

    /// Returns the 1-indexed line slice (without trailing newline).
    pub fn line_text(&self, line_1_based: usize) -> &str {
        if line_1_based == 0 || line_1_based > self.line_starts.len() {
            return "";
        }
        let idx = line_1_based - 1;
        let start = self.line_starts[idx] as usize;
        let end = if idx + 1 < self.line_starts.len() {
            (self.line_starts[idx + 1] as usize).saturating_sub(1)
        } else {
            self.src.len()
        };
        let slice = &self.src[start..end];
        slice.strip_suffix('\r').unwrap_or(slice)
    }
}

/// Multi-file source map.
#[derive(Debug, Default, Clone)]
pub struct SourceMap {
    files: Vec<SourceFile>,
}

impl SourceMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_file(&mut self, name: impl Into<String>, src: impl Into<String>) -> FileId {
        let id = FileId(self.files.len() as u32);
        self.files.push(SourceFile::new(id, name, src));
        id
    }

    pub fn get_file(&self, id: FileId) -> Option<&SourceFile> {
        self.files.get(id.0 as usize)
    }
}

/// Diagnostic severity level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Note,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Severity::Error => write!(f, "error"),
            Severity::Warning => write!(f, "warning"),
            Severity::Note => write!(f, "note"),
        }
    }
}

/// Annotated span label inside a [`Diagnostic`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label {
    pub span: Span,
    pub message: String,
    pub is_primary: bool,
}

impl Label {
    pub fn primary(span: Span, message: impl Into<String>) -> Self {
        Self {
            span,
            message: message.into(),
            is_primary: true,
        }
    }

    pub fn secondary(span: Span, message: impl Into<String>) -> Self {
        Self {
            span,
            message: message.into(),
            is_primary: false,
        }
    }
}

/// Structured compiler diagnostic with error code and Ariadne-style rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: Severity,
    pub code: String,
    pub message: String,
    pub labels: Vec<Label>,
    pub notes: Vec<String>,
}

impl Diagnostic {
    pub fn error(code: impl Into<String>, message: impl Into<String>, span: Span) -> Self {
        let msg = message.into();
        Self {
            severity: Severity::Error,
            code: code.into(),
            message: msg.clone(),
            labels: vec![Label::primary(span, msg)],
            notes: Vec::new(),
        }
    }

    pub fn warning(code: impl Into<String>, message: impl Into<String>, span: Span) -> Self {
        let msg = message.into();
        Self {
            severity: Severity::Warning,
            code: code.into(),
            message: msg.clone(),
            labels: vec![Label::primary(span, msg)],
            notes: Vec::new(),
        }
    }

    pub fn with_label(mut self, label: Label) -> Self {
        self.labels.push(label);
        self
    }

    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.notes.push(note.into());
        self
    }

    pub fn primary_span(&self) -> Option<Span> {
        self.labels
            .iter()
            .find(|l| l.is_primary)
            .or_else(|| self.labels.first())
            .map(|l| l.span)
    }

    /// Renders a rich human-readable diagnostic report with source gutters and carets.
    pub fn render(&self, sm: &SourceMap) -> String {
        let mut out = String::new();
        out.push_str(&format!("{}[{}]: {}\n", self.severity, self.code, self.message));

        for label in &self.labels {
            if let Some(file) = sm.get_file(label.span.file) {
                let (line, col) = file.line_col(label.span.start);
                let line_str = file.line_text(line);
                let caret_len = label.span.len().max(1).min(line_str.len().saturating_sub(col - 1).max(1));
                let marker = if label.is_primary { '^' } else { '-' };
                let carets: String = std::iter::repeat(marker).take(caret_len).collect();
                let pad: String = std::iter::repeat(' ').take(col.saturating_sub(1)).collect();
                out.push_str(&format!("  --> {}:{}:{}\n", file.name, line, col));
                out.push_str("   |\n");
                out.push_str(&format!("{:>3} | {}\n", line, line_str));
                out.push_str(&format!("   | {}{} {}\n", pad, carets, label.message));
            }
        }

        for note in &self.notes {
            out.push_str(&format!("   = note: {}\n", note));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_interner_and_sourcemap() {
        let mut interner = Interner::new();
        let s1 = interner.get_or_intern("qalloc");
        let s2 = interner.get_or_intern("measure");
        let s3 = interner.get_or_intern("qalloc");
        assert_eq!(s1, s3);
        assert_ne!(s1, s2);
        assert_eq!(interner.resolve(s1), "qalloc");

        let mut sm = SourceMap::new();
        let fid = sm.add_file("bell.qg", "package main\nfunc Bell() unitary {\n    q := qalloc()\n}\n");
        let file = sm.get_file(fid).expect("file exists");
        assert_eq!(file.line_col(0), (1, 1));
        assert_eq!(file.line_col(13), (2, 1));

        let diag = Diagnostic::error(
            "E0401",
            "entangled ancilla `q` leaked at end of unitary function",
            Span::new(fid, 39, 40),
        )
        .with_note("add `defer adj ... -> q` to uncompute local ancilla");
        let rendered = diag.render(&sm);
        assert!(rendered.contains("error[E0401]"));
        assert!(rendered.contains("bell.qg:3:5"));
    }
}
