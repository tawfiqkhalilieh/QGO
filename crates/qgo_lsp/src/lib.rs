//! `qgo_lsp`: Language Server Protocol (JSON-RPC 2.0) engine for QGo.
//! Publishes real-time syntax, Purity, and Quantum Linearity diagnostics (`E0400`, `E0401`, `E0301`)
//! and provides hover type/purity inspection.

use qgo_hir::check_module;
use qgo_span::SourceMap;
use qgo_syntax::parse_source;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspDiagnostic {
    pub code: String,
    pub message: String,
    pub line: usize,
    pub column: usize,
}

/// Validates a QGo document and returns structured LSP diagnostics with 1-based line/column positions.
pub fn analyze_document(uri: &str, text: &str) -> Vec<LspDiagnostic> {
    let mut sm = SourceMap::new();
    let fid = sm.add_file(uri, text);
    let (ast, mut diags) = parse_source(fid, text);
    let (_, hir_diags) = check_module(&ast);
    diags.extend(hir_diags);

    let file = sm.get_file(fid).expect("file exists");
    diags
        .into_iter()
        .map(|d| {
            let (line, column) = d
                .primary_span()
                .map(|s| file.line_col(s.start))
                .unwrap_or((1, 1));
            LspDiagnostic {
                code: d.code,
                message: d.message,
                line,
                column,
            }
        })
        .collect()
}

/// Formats an LSP `textDocument/publishDiagnostics` JSON-RPC 2.0 notification payload.
pub fn publish_diagnostics_json(uri: &str, text: &str) -> String {
    let diags = analyze_document(uri, text);
    let items: Vec<String> = diags
        .iter()
        .map(|d| {
            let l0 = d.line.saturating_sub(1);
            let c0 = d.column.saturating_sub(1);
            format!(
                "{{\"range\":{{\"start\":{{\"line\":{},\"character\":{}}},\"end\":{{\"line\":{},\"character\":{}}}}},\"severity\":1,\"code\":\"{}\",\"source\":\"qgo-lsp\",\"message\":\"{}\"}}",
                l0,
                c0,
                l0,
                c0 + 1,
                d.code,
                d.message.replace('"', "\\\"")
            )
        })
        .collect();
    format!(
        "{{\"jsonrpc\":\"2.0\",\"method\":\"textDocument/publishDiagnostics\",\"params\":{{\"uri\":\"{}\",\"diagnostics\":[{}]}}}}" ,
        uri,
        items.join(",")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lsp_reports_ancilla_leak_diagnostic() {
        let src = "package main\nfunc Leak() unitary {\n    q := qalloc()\n    H(q)\n}\n";
        let diags = analyze_document("file:///test.qg", src);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "E0401");
        assert_eq!(diags[0].line, 3);
    }
}
