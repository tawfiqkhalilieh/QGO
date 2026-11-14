//! `qgo_lexer`: Tokenizer and Pass 1 Go Automatic Semicolon Insertion (ASI) filter.

use qgo_span::{Diagnostic, FileId, Span};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenKind {
    // Keywords
    Package,
    Import,
    Func,
    Type,
    Struct,
    Const,
    Var,
    Return,
    If,
    Else,
    For,
    Break,
    Continue,
    Defer,
    Adj,
    Ctrl,
    Unitary,
    QFree,
    QAlloc,
    Measure,
    True,
    False,

    // Built-in type keywords
    Bool,
    Int,
    Int64,
    Uint,
    Float64,
    StringTy,
    Qubit,
    QReg,
    QInt,
    QFixed,

    // Identifiers and literals
    Ident(String),
    IntLit(i64),
    FloatLit(String),
    StringLit(String),

    // Operators & punctuation
    ColonEq,    // :=
    Arrow,      // ->
    PlusEq,     // +=
    MinusEq,    // -=
    PlusPlus,   // ++
    MinusMinus, // --
    Shl,        // <<
    Shr,        // >>
    EqEq,       // ==
    NotEq,      // !=
    Lt,         // <
    LtEq,       // <=
    Gt,         // >
    GtEq,       // >=
    AndAnd,     // &&
    OrOr,       // ||
    Plus,       // +
    Minus,      // -
    Star,       // *
    Slash,      // /
    Percent,    // %
    Caret,      // ^
    Amp,        // &
    Pipe,       // |
    Bang,       // !
    Eq,         // =
    LParen,     // (
    RParen,     // )
    LBracket,   // [
    RBracket,   // ]
    LBrace,     // {
    RBrace,     // }
    Comma,      // ,
    Semi,       // ; (explicit or virtual ASI)
    Colon,      // :
    Dot,        // .
    Eof,
}

impl TokenKind {
    /// Go ASI rule: returns `true` if this token at the end of a line triggers
    /// automatic insertion of a virtual `;` token.
    ///
    /// Per Go specification & QGo Pass 1:
    /// Insert virtual `;` after identifier, literal, `break`, `continue`, `return`,
    /// `qubit`, `++`, `--`, or closing delimiter `)`, `]`, `}`.
    pub fn triggers_asi(&self) -> bool {
        matches!(
            self,
            TokenKind::Ident(_)
                | TokenKind::IntLit(_)
                | TokenKind::FloatLit(_)
                | TokenKind::StringLit(_)
                | TokenKind::True
                | TokenKind::False
                | TokenKind::Break
                | TokenKind::Continue
                | TokenKind::Return
                | TokenKind::Qubit
                | TokenKind::PlusPlus
                | TokenKind::MinusMinus
                | TokenKind::RParen
                | TokenKind::RBracket
                | TokenKind::RBrace
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
    pub is_virtual_semi: bool,
}

impl Token {
    pub fn new(kind: TokenKind, span: Span) -> Self {
        Self {
            kind,
            span,
            is_virtual_semi: false,
        }
    }

    pub fn virtual_semi(span: Span) -> Self {
        Self {
            kind: TokenKind::Semi,
            span,
            is_virtual_semi: true,
        }
    }
}

/// Tokenizes a QGo source file and applies Pass 1 (Go Automatic Semicolon Insertion).
pub fn lex(file: FileId, src: &str) -> (Vec<Token>, Vec<Diagnostic>) {
    let mut tokens: Vec<Token> = Vec::new();
    let mut diags: Vec<Diagnostic> = Vec::new();
    let bytes = src.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    let maybe_insert_asi = |tokens: &mut Vec<Token>, pos: u32| {
        if let Some(last) = tokens.last() {
            if last.kind.triggers_asi() {
                tokens.push(Token::virtual_semi(Span::new(file, pos, pos)));
            }
        }
    };

    while i < len {
        let b = bytes[i];
        match b {
            b' ' | b'\t' | b'\r' => {
                i += 1;
            }
            b'\n' => {
                maybe_insert_asi(&mut tokens, i as u32);
                i += 1;
            }
            b'/' if i + 1 < len && bytes[i + 1] == b'/' => {
                // Line comment: consumes until '\n' (leaving '\n' to trigger ASI if needed)
                i += 2;
                while i < len && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < len && bytes[i + 1] == b'*' => {
                // Block comment: if it spans across a newline, it acts like a newline for ASI
                let start = i;
                i += 2;
                let mut saw_newline = false;
                let mut closed = false;
                while i + 1 < len {
                    if bytes[i] == b'\n' {
                        saw_newline = true;
                    }
                    if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        i += 2;
                        closed = true;
                        break;
                    }
                    i += 1;
                }
                if !closed {
                    i = len;
                    diags.push(Diagnostic::error(
                        "E0101",
                        "unterminated block comment",
                        Span::new(file, start as u32, i as u32),
                    ));
                }
                if saw_newline {
                    maybe_insert_asi(&mut tokens, i as u32);
                }
            }
            b'"' => {
                let start = i;
                i += 1;
                let mut s = String::new();
                let mut closed = false;
                while i < len {
                    let ch = bytes[i];
                    if ch == b'"' {
                        i += 1;
                        closed = true;
                        break;
                    } else if ch == b'\\' && i + 1 < len {
                        i += 1;
                        let esc = match bytes[i] {
                            b'n' => '\n',
                            b't' => '\t',
                            b'r' => '\r',
                            b'"' => '"',
                            b'\\' => '\\',
                            other => other as char,
                        };
                        s.push(esc);
                        i += 1;
                    } else if ch == b'\n' {
                        break;
                    } else {
                        let ch_str = &src[i..];
                        if let Some(c) = ch_str.chars().next() {
                            s.push(c);
                            i += c.len_utf8();
                        } else {
                            break;
                        }
                    }
                }
                if !closed {
                    diags.push(Diagnostic::error(
                        "E0102",
                        "unterminated string literal",
                        Span::new(file, start as u32, i as u32),
                    ));
                }
                tokens.push(Token::new(
                    TokenKind::StringLit(s),
                    Span::new(file, start as u32, i as u32),
                ));
            }
            b'`' => {
                // Raw backtick string literal (no escape processing, may span lines)
                let start = i;
                i += 1;
                let mut s = String::new();
                let mut closed = false;
                while i < len {
                    if bytes[i] == b'`' {
                        i += 1;
                        closed = true;
                        break;
                    }
                    let ch_str = &src[i..];
                    if let Some(c) = ch_str.chars().next() {
                        if c != '\r' {
                            s.push(c);
                        }
                        i += c.len_utf8();
                    } else {
                        break;
                    }
                }
                if !closed {
                    diags.push(Diagnostic::error(
                        "E0102",
                        "unterminated raw string literal",
                        Span::new(file, start as u32, i as u32),
                    ));
                }
                tokens.push(Token::new(
                    TokenKind::StringLit(s),
                    Span::new(file, start as u32, i as u32),
                ));
            }
            b'\'' => {
                // Byte / rune character literal `'a'`, `'\n'` -> IntLit
                let start = i;
                i += 1;
                let val = if i < len && bytes[i] == b'\\' && i + 1 < len {
                    i += 1;
                    let esc = match bytes[i] {
                        b'n' => b'\n' as i64,
                        b't' => b'\t' as i64,
                        b'r' => b'\r' as i64,
                        b'0' => 0i64,
                        b'\'' => b'\'' as i64,
                        b'\\' => b'\\' as i64,
                        other => other as i64,
                    };
                    i += 1;
                    esc
                } else if i < len {
                    let ch = src[i..].chars().next().unwrap_or('\0');
                    i += ch.len_utf8();
                    ch as i64
                } else {
                    0
                };
                if i < len && bytes[i] == b'\'' {
                    i += 1;
                } else {
                    diags.push(Diagnostic::error(
                        "E0103",
                        "unterminated character literal",
                        Span::new(file, start as u32, i as u32),
                    ));
                }
                tokens.push(Token::new(
                    TokenKind::IntLit(val),
                    Span::new(file, start as u32, i as u32),
                ));
            }
            b'0'..=b'9' => {
                let start = i;
                if b == b'0' && i + 1 < len && (bytes[i + 1] == b'x' || bytes[i + 1] == b'X') {
                    i += 2;
                    while i < len && (bytes[i].is_ascii_hexdigit() || bytes[i] == b'_') {
                        i += 1;
                    }
                    let raw: String = src[start + 2..i].chars().filter(|&c| c != '_').collect();
                    let val = i64::from_str_radix(&raw, 16).unwrap_or(0);
                    tokens.push(Token::new(
                        TokenKind::IntLit(val),
                        Span::new(file, start as u32, i as u32),
                    ));
                } else if b == b'0' && i + 1 < len && (bytes[i + 1] == b'b' || bytes[i + 1] == b'B') {
                    i += 2;
                    while i < len && (bytes[i] == b'0' || bytes[i] == b'1' || bytes[i] == b'_') {
                        i += 1;
                    }
                    let raw: String = src[start + 2..i].chars().filter(|&c| c != '_').collect();
                    let val = i64::from_str_radix(&raw, 2).unwrap_or(0);
                    tokens.push(Token::new(
                        TokenKind::IntLit(val),
                        Span::new(file, start as u32, i as u32),
                    ));
                } else {
                    let mut is_float = false;
                    while i < len && (bytes[i].is_ascii_digit() || bytes[i] == b'_') {
                        i += 1;
                    }
                    if i < len && bytes[i] == b'.' && (i + 1 == len || bytes[i + 1] != b'.') {
                        is_float = true;
                        i += 1;
                        while i < len && (bytes[i].is_ascii_digit() || bytes[i] == b'_') {
                            i += 1;
                        }
                    }
                    if i < len && (bytes[i] == b'e' || bytes[i] == b'E') {
                        is_float = true;
                        i += 1;
                        if i < len && (bytes[i] == b'+' || bytes[i] == b'-') {
                            i += 1;
                        }
                        while i < len && bytes[i].is_ascii_digit() {
                            i += 1;
                        }
                    }
                    let raw: String = src[start..i].chars().filter(|&c| c != '_').collect();
                    if is_float {
                        tokens.push(Token::new(
                            TokenKind::FloatLit(raw),
                            Span::new(file, start as u32, i as u32),
                        ));
                    } else {
                        let val = raw.parse::<i64>().unwrap_or(0);
                        tokens.push(Token::new(
                            TokenKind::IntLit(val),
                            Span::new(file, start as u32, i as u32),
                        ));
                    }
                }
            }
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => {
                let start = i;
                while i < len && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                let word = &src[start..i];
                let kind = match word {
                    "package" => TokenKind::Package,
                    "import" => TokenKind::Import,
                    "func" => TokenKind::Func,
                    "type" => TokenKind::Type,
                    "struct" => TokenKind::Struct,
                    "const" => TokenKind::Const,
                    "var" => TokenKind::Var,
                    "return" => TokenKind::Return,
                    "if" => TokenKind::If,
                    "else" => TokenKind::Else,
                    "for" => TokenKind::For,
                    "break" => TokenKind::Break,
                    "continue" => TokenKind::Continue,
                    "defer" => TokenKind::Defer,
                    "adj" => TokenKind::Adj,
                    "ctrl" => TokenKind::Ctrl,
                    "unitary" => TokenKind::Unitary,
                    "qfree" => TokenKind::QFree,
                    "qalloc" => TokenKind::QAlloc,
                    "measure" => TokenKind::Measure,
                    "true" => TokenKind::True,
                    "false" => TokenKind::False,
                    "bool" => TokenKind::Bool,
                    "int" => TokenKind::Int,
                    "int64" => TokenKind::Int64,
                    "uint" => TokenKind::Uint,
                    "float64" => TokenKind::Float64,
                    "string" => TokenKind::StringTy,
                    "qubit" => TokenKind::Qubit,
                    "qreg" => TokenKind::QReg,
                    "qint" => TokenKind::QInt,
                    "qfixed" => TokenKind::QFixed,
                    _ => TokenKind::Ident(word.to_owned()),
                };
                tokens.push(Token::new(kind, Span::new(file, start as u32, i as u32)));
            }
            _ => {
                let start = i;
                let two = if i + 1 < len {
                    Some((bytes[i], bytes[i + 1]))
                } else {
                    None
                };
                let (kind_opt, step) = match two {
                    Some((b':', b'=')) => (Some(TokenKind::ColonEq), 2),
                    Some((b'-', b'>')) => (Some(TokenKind::Arrow), 2),
                    Some((b'+', b'=')) => (Some(TokenKind::PlusEq), 2),
                    Some((b'-', b'=')) => (Some(TokenKind::MinusEq), 2),
                    Some((b'+', b'+')) => (Some(TokenKind::PlusPlus), 2),
                    Some((b'-', b'-')) => (Some(TokenKind::MinusMinus), 2),
                    Some((b'<', b'<')) => (Some(TokenKind::Shl), 2),
                    Some((b'>', b'>')) => (Some(TokenKind::Shr), 2),
                    Some((b'=', b'=')) => (Some(TokenKind::EqEq), 2),
                    Some((b'!', b'=')) => (Some(TokenKind::NotEq), 2),
                    Some((b'<', b'=')) => (Some(TokenKind::LtEq), 2),
                    Some((b'>', b'=')) => (Some(TokenKind::GtEq), 2),
                    Some((b'&', b'&')) => (Some(TokenKind::AndAnd), 2),
                    Some((b'|', b'|')) => (Some(TokenKind::OrOr), 2),
                    _ => {
                        let single = match b {
                            b'<' => Some(TokenKind::Lt),
                            b'>' => Some(TokenKind::Gt),
                            b'+' => Some(TokenKind::Plus),
                            b'-' => Some(TokenKind::Minus),
                            b'*' => Some(TokenKind::Star),
                            b'/' => Some(TokenKind::Slash),
                            b'%' => Some(TokenKind::Percent),
                            b'^' => Some(TokenKind::Caret),
                            b'&' => Some(TokenKind::Amp),
                            b'|' => Some(TokenKind::Pipe),
                            b'!' => Some(TokenKind::Bang),
                            b'=' => Some(TokenKind::Eq),
                            b'(' => Some(TokenKind::LParen),
                            b')' => Some(TokenKind::RParen),
                            b'[' => Some(TokenKind::LBracket),
                            b']' => Some(TokenKind::RBracket),
                            b'{' => Some(TokenKind::LBrace),
                            b'}' => Some(TokenKind::RBrace),
                            b',' => Some(TokenKind::Comma),
                            b';' => Some(TokenKind::Semi),
                            b':' => Some(TokenKind::Colon),
                            b'.' => Some(TokenKind::Dot),
                            _ => None,
                        };
                        (single, 1)
                    }
                };
                if let Some(kind) = kind_opt {
                    i += step;
                    tokens.push(Token::new(kind, Span::new(file, start as u32, i as u32)));
                } else {
                    let ch = src[i..].chars().next().unwrap_or('?');
                    i += ch.len_utf8();
                    diags.push(Diagnostic::error(
                        "E0100",
                        format!("unexpected character `{}`", ch),
                        Span::new(file, start as u32, i as u32),
                    ));
                }
            }
        }
    }

    // End-of-file also triggers Go ASI if the final token can end a statement.
    maybe_insert_asi(&mut tokens, len as u32);
    tokens.push(Token::new(
        TokenKind::Eof,
        Span::new(file, len as u32, len as u32),
    ));

    (tokens, diags)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_go_asi_insertion() {
        let src = r#"package main
import "std/qft"

func Oracle(x qint[4], target qubit) qfree {
    flag := qalloc()
    defer adj Compute(x) -> flag
    return
}
"#;
        let (tokens, diags) = lex(FileId(0), src);
        assert!(diags.is_empty(), "unexpected lexer errors: {:?}", diags);

        let virtual_semis = tokens.iter().filter(|t| t.is_virtual_semi).count();
        // Lines triggering ASI:
        // 1. `package main` -> Ident("main") -> ;
        // 2. `import "std/qft"` -> StringLit -> ;
        // 3. `func Oracle(...) qfree {` -> `{` does NOT trigger ASI!
        // 4. `flag := qalloc()` -> `)` -> ;
        // 5. `defer adj Compute(x) -> flag` -> Ident("flag") -> ;
        // 6. `return` -> Return -> ;
        // 7. `}` -> `}` -> ;
        assert_eq!(virtual_semis, 6);
    }

    #[test]
    fn test_no_asi_after_operators_or_open_delims() {
        let src = "func Add(\n a int,\n b int\n) -> int {\n return a +\n b\n}";
        let (tokens, diags) = lex(FileId(0), src);
        assert!(diags.is_empty());
        // Semicolons should only appear after `b` (on line `return a +\n b`) and `}`
        let semis: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind == TokenKind::Semi)
            .collect();
        assert_eq!(semis.len(), 2);
    }
}
