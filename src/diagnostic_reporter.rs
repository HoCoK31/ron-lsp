#[cfg(feature = "cli")]
use ariadne::{Color, Label, Report, ReportKind, Source};
#[cfg(feature = "cli")]
use std::path::Path;
use tower_lsp::lsp_types::{Diagnostic as LspDiagnostic, DiagnosticSeverity};

/// A diagnostic that is independent of the output format (LSP or ariadne)
#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub line: u32,
    pub col_start: u32,
    pub col_end: u32,
    pub severity: Severity,
    pub message: String,
}

impl PartialEq for Diagnostic {
    fn eq(&self, other: &Self) -> bool {
        self.line == other.line && self.col_start == other.col_start
    }
}

impl Eq for Diagnostic {}

impl PartialOrd for Diagnostic {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Diagnostic {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Sort by line first, then by column
        match self.line.cmp(&other.line) {
            std::cmp::Ordering::Equal => self.col_start.cmp(&other.col_start),
            other => other,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    #[allow(dead_code)]
    Info,
}

impl Diagnostic {
    #[allow(dead_code)]
    pub fn error(line: u32, col_start: u32, col_end: u32, message: String) -> Self {
        Self {
            line,
            col_start,
            col_end,
            severity: Severity::Error,
            message,
        }
    }

    #[allow(dead_code)]
    pub fn warning(line: u32, col_start: u32, col_end: u32, message: String) -> Self {
        Self {
            line,
            col_start,
            col_end,
            severity: Severity::Warning,
            message,
        }
    }

    /// Convert to LSP diagnostic
    #[allow(dead_code)]
    pub fn to_lsp(&self) -> LspDiagnostic {
        use tower_lsp::lsp_types::{Position, Range};

        LspDiagnostic {
            range: Range::new(
                Position::new(self.line, self.col_start),
                Position::new(self.line, self.col_end),
            ),
            severity: Some(match self.severity {
                Severity::Error => DiagnosticSeverity::ERROR,
                Severity::Warning => DiagnosticSeverity::WARNING,
                Severity::Info => DiagnosticSeverity::INFORMATION,
            }),
            message: self.message.clone(),
            ..Default::default()
        }
    }

    /// Report using ariadne (for CLI output)
    #[cfg(feature = "cli")]
    pub fn report_ariadne(&self, file_path: &Path, source: &str) {
        let report_kind = match self.severity {
            Severity::Error => ReportKind::Error,
            Severity::Warning => ReportKind::Warning,
            Severity::Info => ReportKind::Advice,
        };

        let color = match self.severity {
            Severity::Error => Color::Red,
            Severity::Warning => Color::Yellow,
            Severity::Info => Color::Blue,
        };

        // Convert line/col to byte offset
        let byte_offset = get_byte_offset(source, self.line as usize, self.col_start as usize);
        let mut byte_end = get_byte_offset(source, self.line as usize, self.col_end as usize);

        // Ensure the label span is valid (start <= end)
        if byte_end <= byte_offset {
            byte_end = byte_offset + 1;
        }

        Report::build(report_kind, file_path.display().to_string(), byte_offset)
            .with_message(&self.message)
            .with_label(
                Label::new((file_path.display().to_string(), byte_offset..byte_end))
                    .with_message(&self.message)
                    .with_color(color),
            )
            .finish()
            .eprint((file_path.display().to_string(), Source::from(source)))
            .unwrap();
    }
}

/// Convert line/col position to byte offset in source
#[cfg(feature = "cli")]
fn get_byte_offset(source: &str, line: usize, col: usize) -> usize {
    let mut byte_offset = 0;
    for (i, line_text) in source.lines().enumerate() {
        if i == line {
            // Add the column offset
            return byte_offset + col.min(line_text.len());
        }
        // Add line length + newline
        byte_offset += line_text.len() + 1;
    }
    byte_offset
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "cli")]
    #[test]
    fn test_report_ariadne_does_not_panic_with_inverted_span() {
        // Bug fix: when col_start > col_end (e.g. from a multi-line LSP range
        // flattened to one line), ariadne panicked with "Label start is after its end".
        // The guard in report_ariadne should prevent this.
        let diag = Diagnostic {
            line: 0,
            col_start: 20,
            col_end: 5, // intentionally inverted
            severity: Severity::Error,
            message: "test inverted span".to_string(),
        };

        let source = "    some_field: SomeType(\n        nested: 1,\n    ),\n";
        let path = std::path::Path::new("test.ron");

        // This should not panic
        diag.report_ariadne(path, source);
    }

    #[cfg(feature = "cli")]
    #[test]
    fn test_report_ariadne_does_not_panic_with_zero_width_span() {
        let diag = Diagnostic {
            line: 0,
            col_start: 10,
            col_end: 10, // zero width
            severity: Severity::Error,
            message: "test zero width span".to_string(),
        };

        let source = "    some_field: value,\n";
        let path = std::path::Path::new("test.ron");

        // This should not panic
        diag.report_ariadne(path, source);
    }

    #[cfg(feature = "cli")]
    #[test]
    fn test_get_byte_offset() {
        let source = "line0\nline1\nline2\n";
        assert_eq!(get_byte_offset(source, 0, 0), 0);
        assert_eq!(get_byte_offset(source, 0, 3), 3);
        assert_eq!(get_byte_offset(source, 1, 0), 6);
        assert_eq!(get_byte_offset(source, 1, 2), 8);
        assert_eq!(get_byte_offset(source, 2, 0), 12);
    }

    #[cfg(feature = "cli")]
    #[test]
    fn test_get_byte_offset_clamps_column_and_line() {
        let source = "abc\ndefgh\nij\n";

        // A column within the line is added to the line start offset
        assert_eq!(get_byte_offset(source, 1, 3), 7);
        // A column past the end of the line is clamped to the line length
        assert_eq!(get_byte_offset(source, 1, 99), 9);
        // A line past the end of the source yields the source length
        assert_eq!(get_byte_offset(source, 99, 0), source.len());
    }

    #[test]
    fn test_error_and_warning_constructors() {
        let error = Diagnostic::error(2, 3, 5, "unexpected token".to_string());
        assert_eq!((error.line, error.col_start, error.col_end), (2, 3, 5));
        assert_eq!(error.severity, Severity::Error);
        assert_eq!(error.message, "unexpected token");

        let warning = Diagnostic::warning(7, 0, 1, "unknown field".to_string());
        assert_eq!(
            (warning.line, warning.col_start, warning.col_end),
            (7, 0, 1)
        );
        assert_eq!(warning.severity, Severity::Warning);
        assert_eq!(warning.message, "unknown field");
    }

    #[test]
    fn test_to_lsp_maps_range_severity_and_message() {
        use tower_lsp::lsp_types::Position;

        let error = Diagnostic::error(2, 3, 5, "unexpected token".to_string()).to_lsp();
        assert_eq!(error.range.start, Position::new(2, 3));
        assert_eq!(error.range.end, Position::new(2, 5));
        assert_eq!(error.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(error.message, "unexpected token");

        let warning = Diagnostic::warning(0, 0, 4, "unknown field".to_string()).to_lsp();
        assert_eq!(warning.severity, Some(DiagnosticSeverity::WARNING));
        assert_eq!(warning.range.start, Position::new(0, 0));

        let info = Diagnostic {
            line: 1,
            col_start: 1,
            col_end: 2,
            severity: Severity::Info,
            message: "note".to_string(),
        }
        .to_lsp();
        assert_eq!(info.severity, Some(DiagnosticSeverity::INFORMATION));
        assert_eq!(info.range.end, Position::new(1, 2));
    }

    #[test]
    fn test_equality_is_based_on_position_only() {
        // Two diagnostics are equal when they start at the same line and column,
        // regardless of their span end, severity or message.
        let error = Diagnostic::error(3, 4, 8, "first message".to_string());
        let warning = Diagnostic::warning(3, 4, 99, "other message".to_string());
        assert_eq!(error, warning);
        assert_ne!(
            error,
            Diagnostic::error(3, 5, 8, "first message".to_string())
        );
        assert_ne!(
            error,
            Diagnostic::error(4, 4, 8, "first message".to_string())
        );
    }

    #[test]
    fn test_ordering_is_by_line_then_column() {
        let earlier_column = Diagnostic::error(1, 2, 3, "a".to_string());
        let later_column = Diagnostic::error(1, 5, 6, "b".to_string());
        let later_line = Diagnostic::error(4, 0, 1, "c".to_string());

        assert_eq!(earlier_column.cmp(&later_column), std::cmp::Ordering::Less);
        assert_eq!(
            earlier_column.partial_cmp(&later_column),
            Some(std::cmp::Ordering::Less)
        );
        assert_eq!(later_line.cmp(&later_column), std::cmp::Ordering::Greater);
        // Neither the message nor the span end affects the order
        let same_position = Diagnostic::error(1, 2, 3, "different message".to_string());
        assert_eq!(
            earlier_column.cmp(&same_position),
            std::cmp::Ordering::Equal
        );

        // The CLI sorts diagnostics by line and column before printing them
        let mut diagnostics = [
            later_line.clone(),
            later_column.clone(),
            earlier_column.clone(),
        ];
        diagnostics.sort();
        assert_eq!(diagnostics, [earlier_column, later_column, later_line]);
    }
}
