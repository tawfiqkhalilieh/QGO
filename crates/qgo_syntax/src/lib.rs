//! `qgo_syntax`: Typed AST, recursive-descent + Pratt expression parser, and code formatter.

use qgo_lexer::{lex, Token, TokenKind};
use qgo_span::{Diagnostic, FileId, Span};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFile {
    pub package: String,
    pub imports: Vec<String>,
    pub items: Vec<TopLevelDecl>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TopLevelDecl {
    Func(FuncDecl),
    Struct(StructDecl),
    Const(ConstDecl),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PurityMod {
    /// Hybrid classical-quantum function (default).
    Hybrid,
    /// Pure isometric/unitary quantum transformation (`unitary`).
    Unitary,
    /// Classical-reversible basis permutation (`qfree`).
    QFree,
}

impl std::fmt::Display for PurityMod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PurityMod::Hybrid => write!(f, "hybrid"),
            PurityMod::Unitary => write!(f, "unitary"),
            PurityMod::QFree => write!(f, "qfree"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuncDecl {
    pub name: String,
    pub params: Vec<Param>,
    pub purity: PurityMod,
    pub ret_ty: Option<TypeExpr>,
    pub body: Block,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Param {
    pub name: String,
    pub ty: TypeExpr,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructDecl {
    pub name: String,
    pub fields: Vec<Param>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConstDecl {
    pub name: String,
    pub ty: Option<TypeExpr>,
    pub value: Expr,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConstExpr {
    Int(usize),
    Ident(String),
}

impl ConstExpr {
    pub fn resolve_static(&self, consts: &std::collections::HashMap<String, usize>) -> Option<usize> {
        match self {
            ConstExpr::Int(n) => Some(*n),
            ConstExpr::Ident(name) => consts.get(name).copied(),
        }
    }
}

impl std::fmt::Display for ConstExpr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConstExpr::Int(n) => write!(f, "{}", n),
            ConstExpr::Ident(s) => write!(f, "{}", s),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeExpr {
    // Classical value types (Copyable)
    Bool,
    Int,
    Int64,
    Uint,
    Float64,
    StringTy,
    Array(ConstExpr, Box<TypeExpr>),
    Slice(Box<TypeExpr>),
    Named(String),

    // Quantum linear types (Non-copyable / Move & Mutate-In-Place Only)
    Qubit,
    QReg(ConstExpr),
    QInt(ConstExpr),
    QFixed(ConstExpr, ConstExpr),
}

impl TypeExpr {
    pub fn is_quantum(&self) -> bool {
        match self {
            TypeExpr::Qubit | TypeExpr::QReg(_) | TypeExpr::QInt(_) | TypeExpr::QFixed(_, _) => true,
            TypeExpr::Array(_, inner) | TypeExpr::Slice(inner) => inner.is_quantum(),
            _ => false,
        }
    }
}

impl std::fmt::Display for TypeExpr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TypeExpr::Bool => write!(f, "bool"),
            TypeExpr::Int => write!(f, "int"),
            TypeExpr::Int64 => write!(f, "int64"),
            TypeExpr::Uint => write!(f, "uint"),
            TypeExpr::Float64 => write!(f, "float64"),
            TypeExpr::StringTy => write!(f, "string"),
            TypeExpr::Array(n, inner) => write!(f, "[{}]{}", n, inner),
            TypeExpr::Slice(inner) => write!(f, "[]{}", inner),
            TypeExpr::Named(s) => write!(f, "{}", s),
            TypeExpr::Qubit => write!(f, "qubit"),
            TypeExpr::QReg(n) => write!(f, "qreg[{}]", n),
            TypeExpr::QInt(n) => write!(f, "qint[{}]", n),
            TypeExpr::QFixed(n, m) => write!(f, "qfixed[{}, {}]", n, m),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModKind {
    Adj,
    Ctrl(Vec<Expr>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stmt {
    /// `name := init` or `var name: ty = init`
    Decl {
        name: String,
        ty: Option<TypeExpr>,
        init: Expr,
        span: Span,
    },
    /// `target = value`
    Assign {
        target: Expr,
        value: Expr,
        span: Span,
    },
    /// `defer adj CallExpr [-> target]`
    DeferAdj {
        call: Expr,
        target: Option<String>,
        span: Span,
    },
    /// `adj Stmt` or `ctrl(c1, c2, ...) Stmt`
    QuantumMod {
        kind: ModKind,
        body: Box<Stmt>,
        span: Span,
    },
    /// `if cond { then_block } else { else_block }`
    If {
        cond: Expr,
        then_block: Block,
        else_block: Option<Block>,
        span: Span,
    },
    /// `for [cond] { body }` or `for init; cond; post { body }`
    For {
        init: Option<Box<Stmt>>,
        cond: Option<Expr>,
        post: Option<Box<Stmt>>,
        body: Block,
        span: Span,
    },
    Return {
        value: Option<Expr>,
        span: Span,
    },
    Break(Span),
    Continue(Span),
    Block(Block),
    Expr {
        expr: Expr,
        span: Span,
    },
}

impl Stmt {
    pub fn span(&self) -> Span {
        match self {
            Stmt::Decl { span, .. }
            | Stmt::Assign { span, .. }
            | Stmt::DeferAdj { span, .. }
            | Stmt::QuantumMod { span, .. }
            | Stmt::If { span, .. }
            | Stmt::For { span, .. }
            | Stmt::Return { span, .. }
            | Stmt::Break(span)
            | Stmt::Continue(span)
            | Stmt::Expr { span, .. } => *span,
            Stmt::Block(b) => b.span,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    BitXor,
    BitAnd,
    BitOr,
    Shl,
    Shr,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    And,
    Or,
}

impl std::fmt::Display for BinOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Rem => "%",
            BinOp::BitXor => "^",
            BinOp::BitAnd => "&",
            BinOp::BitOr => "|",
            BinOp::Shl => "<<",
            BinOp::Shr => ">>",
            BinOp::Eq => "==",
            BinOp::NotEq => "!=",
            BinOp::Lt => "<",
            BinOp::LtEq => "<=",
            BinOp::Gt => ">",
            BinOp::GtEq => ">=",
            BinOp::And => "&&",
            BinOp::Or => "||",
        };
        write!(f, "{}", s)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    Ident(String, Span),
    IntLit(i64, Span),
    FloatLit(String, Span),
    BoolLit(bool, Span),
    StringLit(String, Span),
    /// `StructName{ field1: val1, field2: val2 }`
    StructLit {
        name: String,
        fields: Vec<(String, Expr)>,
        span: Span,
    },
    /// `[]ElemTy{ e1, e2, ... }`
    SliceLit {
        elem_ty: TypeExpr,
        elems: Vec<Expr>,
        span: Span,
    },
    /// `qalloc()` or `qalloc[N]()` or `qalloc[qint[N]]()`
    QAlloc {
        size: Option<ConstExpr>,
        ty_override: Option<TypeExpr>,
        span: Span,
    },
    /// `measure(expr)`
    Measure {
        arg: Box<Expr>,
        span: Span,
    },
    /// Function or gate invocation `Name(args...)`
    Call {
        callee: String,
        args: Vec<Expr>,
        span: Span,
    },
    /// `base[index]`
    Index {
        base: Box<Expr>,
        index: Box<Expr>,
        span: Span,
    },
    /// `base[start:end]`
    Slice {
        base: Box<Expr>,
        start: Option<Box<Expr>>,
        end: Option<Box<Expr>>,
        span: Span,
    },
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
        span: Span,
    },
    Unary {
        op: UnOp,
        expr: Box<Expr>,
        span: Span,
    },
    QuantumMod {
        kind: ModKind,
        expr: Box<Expr>,
        span: Span,
    },
    FieldAccess {
        base: Box<Expr>,
        field: String,
        span: Span,
    },
}

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Expr::Ident(_, s)
            | Expr::IntLit(_, s)
            | Expr::FloatLit(_, s)
            | Expr::BoolLit(_, s)
            | Expr::StringLit(_, s)
            | Expr::StructLit { span: s, .. }
            | Expr::SliceLit { span: s, .. }
            | Expr::QAlloc { span: s, .. }
            | Expr::Measure { span: s, .. }
            | Expr::Call { span: s, .. }
            | Expr::Index { span: s, .. }
            | Expr::Slice { span: s, .. }
            | Expr::Binary { span: s, .. }
            | Expr::Unary { span: s, .. }
            | Expr::QuantumMod { span: s, .. }
            | Expr::FieldAccess { span: s, .. } => *s,
        }
    }
}

/// Parses a QGo source string into an AST [`SourceFile`] and collects any diagnostics.
pub fn parse_source(file: FileId, src: &str) -> (SourceFile, Vec<Diagnostic>) {
    let (tokens, mut diags) = lex(file, src);
    let mut parser = Parser::new(file, tokens);
    let ast = parser.parse_file(&mut diags);
    (ast, diags)
}

struct Parser {
    _file: FileId,
    tokens: Vec<Token>,
    pos: usize,
    allow_struct_lit: bool,
}

impl Parser {
    fn new(file: FileId, tokens: Vec<Token>) -> Self {
        Self {
            _file: file,
            tokens,
            pos: 0,
            allow_struct_lit: true,
        }
    }

    fn peek(&self) -> &Token {
        self.tokens
            .get(self.pos)
            .unwrap_or_else(|| self.tokens.last().expect("non-empty token stream"))
    }

    fn peek_kind(&self) -> &TokenKind {
        &self.peek().kind
    }

    fn peek_n_kind(&self, n: usize) -> &TokenKind {
        &self
            .tokens
            .get(self.pos + n)
            .unwrap_or_else(|| self.tokens.last().expect("non-empty token stream"))
            .kind
    }

    fn bump(&mut self) -> Token {
        let tok = self.peek().clone();
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
        tok
    }

    fn eat_semis(&mut self) {
        while matches!(self.peek_kind(), TokenKind::Semi) {
            self.bump();
        }
    }

    fn expect_semi(&mut self, diags: &mut Vec<Diagnostic>) {
        if matches!(self.peek_kind(), TokenKind::Semi) {
            self.bump();
        } else if !matches!(self.peek_kind(), TokenKind::RBrace | TokenKind::Eof) {
            diags.push(Diagnostic::error(
                "E0200",
                format!("expected `;` or newline, found `{:?}`", self.peek_kind()),
                self.peek().span,
            ));
        }
        self.eat_semis();
    }

    fn expect_ident(&mut self, diags: &mut Vec<Diagnostic>, context: &str) -> (String, Span) {
        let tok = self.peek().clone();
        match tok.kind {
            TokenKind::Ident(s) => {
                self.bump();
                (s, tok.span)
            }
            _ => {
                diags.push(Diagnostic::error(
                    "E0201",
                    format!("expected identifier for {}, found `{:?}`", context, tok.kind),
                    tok.span,
                ));
                self.bump();
                ("_error".to_owned(), tok.span)
            }
        }
    }

    fn parse_file(&mut self, diags: &mut Vec<Diagnostic>) -> SourceFile {
        let start_span = self.peek().span;
        self.eat_semis();

        let mut package = "main".to_owned();
        if matches!(self.peek_kind(), TokenKind::Package) {
            self.bump();
            let (pkg_name, _) = self.expect_ident(diags, "package clause");
            package = pkg_name;
            self.expect_semi(diags);
        } else {
            diags.push(Diagnostic::error(
                "E0202",
                "QGo source file must begin with a `package <name>` declaration",
                self.peek().span,
            ));
        }

        let mut imports = Vec::new();
        while matches!(self.peek_kind(), TokenKind::Import) {
            self.bump();
            if matches!(self.peek_kind(), TokenKind::LParen) {
                self.bump();
                self.eat_semis();
                while !matches!(self.peek_kind(), TokenKind::RParen | TokenKind::Eof) {
                    if let TokenKind::StringLit(s) = self.peek_kind().clone() {
                        self.bump();
                        imports.push(s);
                    } else {
                        diags.push(Diagnostic::error(
                            "E0203",
                            "expected string literal in import block",
                            self.peek().span,
                        ));
                        self.bump();
                    }
                    self.eat_semis();
                }
                if matches!(self.peek_kind(), TokenKind::RParen) {
                    self.bump();
                }
                self.expect_semi(diags);
            } else if let TokenKind::StringLit(s) = self.peek_kind().clone() {
                self.bump();
                imports.push(s);
                self.expect_semi(diags);
            } else {
                diags.push(Diagnostic::error(
                    "E0203",
                    "expected string literal or `(` after `import`",
                    self.peek().span,
                ));
                self.bump();
            }
        }

        let mut items = Vec::new();
        while !matches!(self.peek_kind(), TokenKind::Eof) {
            self.eat_semis();
            if matches!(self.peek_kind(), TokenKind::Eof) {
                break;
            }
            match self.peek_kind() {
                TokenKind::Func => items.push(TopLevelDecl::Func(self.parse_func_decl(diags))),
                TokenKind::Struct | TokenKind::Type => {
                    items.push(TopLevelDecl::Struct(self.parse_struct_decl(diags)))
                }
                TokenKind::Const => items.push(TopLevelDecl::Const(self.parse_const_decl(diags))),
                _ => {
                    diags.push(Diagnostic::error(
                        "E0204",
                        format!("unexpected top-level token `{:?}`", self.peek_kind()),
                        self.peek().span,
                    ));
                    self.bump();
                }
            }
            self.eat_semis();
        }

        let end_span = self.peek().span;
        SourceFile {
            package,
            imports,
            items,
            span: start_span.merge(end_span),
        }
    }

    fn parse_func_decl(&mut self, diags: &mut Vec<Diagnostic>) -> FuncDecl {
        let start = self.bump().span; // `func`
        let (name, _) = self.expect_ident(diags, "function name");

        if matches!(self.peek_kind(), TokenKind::LParen) {
            self.bump();
        } else {
            diags.push(Diagnostic::error(
                "E0205",
                "expected `(` after function name",
                self.peek().span,
            ));
        }

        let mut params = Vec::new();
        self.eat_semis();
        while !matches!(self.peek_kind(), TokenKind::RParen | TokenKind::Eof) {
            let (pname, pspan) = self.expect_ident(diags, "parameter name");
            if matches!(self.peek_kind(), TokenKind::Colon) {
                self.bump();
            }
            let pty = self.parse_type(diags);
            let end = self.peek().span;
            params.push(Param {
                name: pname,
                ty: pty,
                span: pspan.merge(end),
            });
            if matches!(self.peek_kind(), TokenKind::Comma) {
                self.bump();
                self.eat_semis();
            } else {
                self.eat_semis();
                break;
            }
        }
        if matches!(self.peek_kind(), TokenKind::RParen) {
            self.bump();
        }

        let purity = match self.peek_kind() {
            TokenKind::Unitary => {
                self.bump();
                PurityMod::Unitary
            }
            TokenKind::QFree => {
                self.bump();
                PurityMod::QFree
            }
            _ => PurityMod::Hybrid,
        };

        let mut ret_ty = None;
        if matches!(self.peek_kind(), TokenKind::Arrow) {
            self.bump();
            ret_ty = Some(self.parse_type(diags));
        } else if !matches!(self.peek_kind(), TokenKind::LBrace | TokenKind::Eof) {
            // Also support Go-style return type directly before `{`
            ret_ty = Some(self.parse_type(diags));
        }

        let body = self.parse_block(diags);
        let span = start.merge(body.span);
        FuncDecl {
            name,
            params,
            purity,
            ret_ty,
            body,
            span,
        }
    }

    fn parse_struct_decl(&mut self, diags: &mut Vec<Diagnostic>) -> StructDecl {
        let start = self.bump().span; // `struct` or `type`
        let (name, _) = self.expect_ident(diags, "struct name");
        if matches!(self.peek_kind(), TokenKind::Struct) {
            self.bump(); // consume `struct` in `type Name struct { ... }`
        }
        if matches!(self.peek_kind(), TokenKind::LBrace) {
            self.bump();
        }
        self.eat_semis();
        let mut fields = Vec::new();
        while !matches!(self.peek_kind(), TokenKind::RBrace | TokenKind::Eof) {
            let (fname, fspan) = self.expect_ident(diags, "struct field");
            if matches!(self.peek_kind(), TokenKind::Colon) {
                self.bump();
            }
            let fty = self.parse_type(diags);
            fields.push(Param {
                name: fname,
                ty: fty,
                span: fspan,
            });
            if matches!(self.peek_kind(), TokenKind::Comma) {
                self.bump();
            }
            self.eat_semis();
        }
        let end = if matches!(self.peek_kind(), TokenKind::RBrace) {
            self.bump().span
        } else {
            self.peek().span
        };
        StructDecl {
            name,
            fields,
            span: start.merge(end),
        }
    }

    fn parse_const_decl(&mut self, diags: &mut Vec<Diagnostic>) -> ConstDecl {
        let start = self.bump().span; // `const`
        let (name, _) = self.expect_ident(diags, "constant name");
        let mut ty = None;
        if !matches!(self.peek_kind(), TokenKind::Eq | TokenKind::ColonEq) {
            if matches!(self.peek_kind(), TokenKind::Colon) {
                self.bump();
            }
            ty = Some(self.parse_type(diags));
        }
        if matches!(self.peek_kind(), TokenKind::Eq | TokenKind::ColonEq) {
            self.bump();
        }
        let value = self.parse_expr(diags);
        let span = start.merge(value.span());
        self.expect_semi(diags);
        ConstDecl {
            name,
            ty,
            value,
            span,
        }
    }

    fn parse_const_expr(&mut self, diags: &mut Vec<Diagnostic>) -> ConstExpr {
        match self.peek_kind().clone() {
            TokenKind::IntLit(n) => {
                self.bump();
                ConstExpr::Int(n.max(0) as usize)
            }
            TokenKind::Ident(s) => {
                self.bump();
                ConstExpr::Ident(s)
            }
            other => {
                diags.push(Diagnostic::error(
                    "E0206",
                    format!("expected constant integer or identifier in type parameter, found `{:?}`", other),
                    self.peek().span,
                ));
                self.bump();
                ConstExpr::Int(1)
            }
        }
    }

    fn parse_type(&mut self, diags: &mut Vec<Diagnostic>) -> TypeExpr {
        match self.peek_kind().clone() {
            TokenKind::Bool => {
                self.bump();
                TypeExpr::Bool
            }
            TokenKind::Int => {
                self.bump();
                TypeExpr::Int
            }
            TokenKind::Int64 => {
                self.bump();
                TypeExpr::Int64
            }
            TokenKind::Uint => {
                self.bump();
                TypeExpr::Uint
            }
            TokenKind::Float64 => {
                self.bump();
                TypeExpr::Float64
            }
            TokenKind::StringTy => {
                self.bump();
                TypeExpr::StringTy
            }
            TokenKind::Qubit => {
                self.bump();
                TypeExpr::Qubit
            }
            TokenKind::QReg => {
                self.bump();
                if matches!(self.peek_kind(), TokenKind::LBracket) {
                    self.bump();
                }
                let n = self.parse_const_expr(diags);
                if matches!(self.peek_kind(), TokenKind::RBracket) {
                    self.bump();
                }
                TypeExpr::QReg(n)
            }
            TokenKind::QInt => {
                self.bump();
                if matches!(self.peek_kind(), TokenKind::LBracket) {
                    self.bump();
                }
                let n = self.parse_const_expr(diags);
                if matches!(self.peek_kind(), TokenKind::RBracket) {
                    self.bump();
                }
                TypeExpr::QInt(n)
            }
            TokenKind::QFixed => {
                self.bump();
                if matches!(self.peek_kind(), TokenKind::LBracket) {
                    self.bump();
                }
                let n = self.parse_const_expr(diags);
                if matches!(self.peek_kind(), TokenKind::Comma) {
                    self.bump();
                }
                let m = self.parse_const_expr(diags);
                if matches!(self.peek_kind(), TokenKind::RBracket) {
                    self.bump();
                }
                TypeExpr::QFixed(n, m)
            }
            TokenKind::LBracket => {
                self.bump();
                if matches!(self.peek_kind(), TokenKind::RBracket) {
                    self.bump();
                    let inner = self.parse_type(diags);
                    TypeExpr::Slice(Box::new(inner))
                } else {
                    let n = self.parse_const_expr(diags);
                    if matches!(self.peek_kind(), TokenKind::RBracket) {
                        self.bump();
                    }
                    let inner = self.parse_type(diags);
                    TypeExpr::Array(n, Box::new(inner))
                }
            }
            TokenKind::Ident(name) => {
                self.bump();
                TypeExpr::Named(name)
            }
            other => {
                diags.push(Diagnostic::error(
                    "E0207",
                    format!("expected type expression, found `{:?}`", other),
                    self.peek().span,
                ));
                self.bump();
                TypeExpr::Int
            }
        }
    }

    fn parse_block(&mut self, diags: &mut Vec<Diagnostic>) -> Block {
        let start = self.peek().span;
        if matches!(self.peek_kind(), TokenKind::LBrace) {
            self.bump();
        } else {
            diags.push(Diagnostic::error(
                "E0208",
                format!("expected `{{` to start block, found `{:?}`", self.peek_kind()),
                self.peek().span,
            ));
        }
        self.eat_semis();
        let mut stmts = Vec::new();
        while !matches!(self.peek_kind(), TokenKind::RBrace | TokenKind::Eof) {
            let stmt = self.parse_stmt(diags);
            stmts.push(stmt);
            self.eat_semis();
        }
        let end = if matches!(self.peek_kind(), TokenKind::RBrace) {
            self.bump().span
        } else {
            self.peek().span
        };
        Block {
            stmts,
            span: start.merge(end),
        }
    }

    fn parse_stmt(&mut self, diags: &mut Vec<Diagnostic>) -> Stmt {
        match self.peek_kind() {
            TokenKind::Var => {
                let start = self.bump().span;
                let (name, _) = self.expect_ident(diags, "variable declaration");
                let mut ty = None;
                if !matches!(self.peek_kind(), TokenKind::Eq | TokenKind::ColonEq) {
                    if matches!(self.peek_kind(), TokenKind::Colon) {
                        self.bump();
                    }
                    ty = Some(self.parse_type(diags));
                }
                let init = if matches!(self.peek_kind(), TokenKind::Eq | TokenKind::ColonEq) {
                    self.bump();
                    self.parse_expr(diags)
                } else {
                    match &ty {
                        Some(TypeExpr::StringTy) => Expr::StringLit(String::new(), start),
                        Some(TypeExpr::Bool) => Expr::BoolLit(false, start),
                        Some(TypeExpr::Float64) => Expr::FloatLit("0.0".to_owned(), start),
                        Some(TypeExpr::Slice(inner)) => Expr::SliceLit {
                            elem_ty: (**inner).clone(),
                            elems: Vec::new(),
                            span: start,
                        },
                        Some(TypeExpr::Named(sname)) => Expr::StructLit {
                            name: sname.clone(),
                            fields: Vec::new(),
                            span: start,
                        },
                        _ => Expr::IntLit(0, start),
                    }
                };
                let span = start.merge(init.span());
                self.expect_semi(diags);
                Stmt::Decl {
                    name,
                    ty,
                    init,
                    span,
                }
            }
            TokenKind::Defer => {
                let start = self.bump().span; // `defer`
                if matches!(self.peek_kind(), TokenKind::Adj) {
                    self.bump();
                } else {
                    diags.push(Diagnostic::error(
                        "E0209",
                        "expected `adj` after `defer` (`defer adj CallExpr [-> ident]`)",
                        self.peek().span,
                    ));
                }
                let call = self.parse_expr(diags);
                let mut target = None;
                if matches!(self.peek_kind(), TokenKind::Arrow) {
                    self.bump();
                    let (tname, _) = self.expect_ident(diags, "defer adj target");
                    target = Some(tname);
                }
                let end = self.peek().span;
                self.expect_semi(diags);
                Stmt::DeferAdj {
                    call,
                    target,
                    span: start.merge(end),
                }
            }
            TokenKind::Adj => {
                let start = self.bump().span;
                let inner = self.parse_stmt(diags);
                let span = start.merge(inner.span());
                Stmt::QuantumMod {
                    kind: ModKind::Adj,
                    body: Box::new(inner),
                    span,
                }
            }
            TokenKind::Ctrl => {
                let start = self.bump().span;
                if matches!(self.peek_kind(), TokenKind::LParen) {
                    self.bump();
                }
                let mut ctrls = Vec::new();
                while !matches!(self.peek_kind(), TokenKind::RParen | TokenKind::Eof) {
                    ctrls.push(self.parse_expr(diags));
                    if matches!(self.peek_kind(), TokenKind::Comma) {
                        self.bump();
                    } else {
                        break;
                    }
                }
                if matches!(self.peek_kind(), TokenKind::RParen) {
                    self.bump();
                }
                let inner = self.parse_stmt(diags);
                let span = start.merge(inner.span());
                Stmt::QuantumMod {
                    kind: ModKind::Ctrl(ctrls),
                    body: Box::new(inner),
                    span,
                }
            }
            TokenKind::If => {
                let start = self.bump().span;
                let prev_allow = self.allow_struct_lit;
                self.allow_struct_lit = false;
                let cond = self.parse_expr(diags);
                self.allow_struct_lit = prev_allow;
                let then_block = self.parse_block(diags);
                let mut else_block = None;
                if matches!(self.peek_kind(), TokenKind::Else) {
                    self.bump();
                    if matches!(self.peek_kind(), TokenKind::If) {
                        let nested_if = self.parse_stmt(diags);
                        let nspan = nested_if.span();
                        else_block = Some(Block {
                            stmts: vec![nested_if],
                            span: nspan,
                        });
                    } else {
                        else_block = Some(self.parse_block(diags));
                    }
                }
                let end = else_block
                    .as_ref()
                    .map(|b| b.span)
                    .unwrap_or(then_block.span);
                self.eat_semis();
                Stmt::If {
                    cond,
                    then_block,
                    else_block,
                    span: start.merge(end),
                }
            }
            TokenKind::For => {
                let start = self.bump().span;
                if matches!(self.peek_kind(), TokenKind::LBrace) {
                    let body = self.parse_block(diags);
                    let span = start.merge(body.span);
                    self.eat_semis();
                    return Stmt::For {
                        init: None,
                        cond: None,
                        post: None,
                        body,
                        span,
                    };
                }
                let prev_allow = self.allow_struct_lit;
                self.allow_struct_lit = false;
                // Check if C-style `for i := 0; i < N; i++` or single condition `for cond`
                if matches!(self.peek_kind(), TokenKind::Ident(_))
                    && matches!(self.peek_n_kind(1), TokenKind::ColonEq)
                {
                    let (iname, ispan) = self.expect_ident(diags, "loop var");
                    self.bump(); // `:=`
                    let iexpr = self.parse_expr(diags);
                    let init_stmt = Stmt::Decl {
                        name: iname,
                        ty: None,
                        span: ispan.merge(iexpr.span()),
                        init: iexpr,
                    };
                    self.expect_semi(diags);
                    let cond = self.parse_expr(diags);
                    self.expect_semi(diags);
                    let post_lhs = self.parse_expr(diags);
                    let post_stmt = self.parse_assign_or_inc_tail(post_lhs, diags, false);
                    self.allow_struct_lit = prev_allow;
                    let body = self.parse_block(diags);
                    let span = start.merge(body.span);
                    self.eat_semis();
                    Stmt::For {
                        init: Some(Box::new(init_stmt)),
                        cond: Some(cond),
                        post: Some(Box::new(post_stmt)),
                        body,
                        span,
                    }
                } else {
                    let cond = self.parse_expr(diags);
                    self.allow_struct_lit = prev_allow;
                    let body = self.parse_block(diags);
                    let span = start.merge(body.span);
                    self.eat_semis();
                    Stmt::For {
                        init: None,
                        cond: Some(cond),
                        post: None,
                        body,
                        span,
                    }
                }
            }
            TokenKind::Return => {
                let start = self.bump().span;
                let value = if matches!(
                    self.peek_kind(),
                    TokenKind::Semi | TokenKind::RBrace | TokenKind::Eof
                ) {
                    None
                } else {
                    Some(self.parse_expr(diags))
                };
                let end = value.as_ref().map(|e| e.span()).unwrap_or(start);
                self.expect_semi(diags);
                Stmt::Return {
                    value,
                    span: start.merge(end),
                }
            }
            TokenKind::Break => {
                let span = self.bump().span;
                self.expect_semi(diags);
                Stmt::Break(span)
            }
            TokenKind::Continue => {
                let span = self.bump().span;
                self.expect_semi(diags);
                Stmt::Continue(span)
            }
            TokenKind::LBrace => {
                let b = self.parse_block(diags);
                self.eat_semis();
                Stmt::Block(b)
            }
            _ => {
                // Short declaration `x := expr` or assignment `lhs = expr` or expression statement
                if let TokenKind::Ident(_) = self.peek_kind() {
                    if matches!(self.peek_n_kind(1), TokenKind::ColonEq) {
                        let (name, start) = self.expect_ident(diags, "short variable declaration");
                        self.bump(); // `:=`
                        let init = self.parse_expr(diags);
                        let span = start.merge(init.span());
                        self.expect_semi(diags);
                        return Stmt::Decl {
                            name,
                            ty: None,
                            init,
                            span,
                        };
                    }
                }
                let lhs = self.parse_expr(diags);
                self.parse_assign_or_inc_tail(lhs, diags, true)
            }
        }
    }

    fn parse_assign_or_inc_tail(
        &mut self,
        lhs: Expr,
        diags: &mut Vec<Diagnostic>,
        consume_semi: bool,
    ) -> Stmt {
        let stmt = match self.peek_kind() {
            TokenKind::Eq => {
                self.bump();
                let rhs = self.parse_expr(diags);
                let span = lhs.span().merge(rhs.span());
                Stmt::Assign {
                    target: lhs,
                    value: rhs,
                    span,
                }
            }
            TokenKind::PlusEq => {
                self.bump();
                let rhs = self.parse_expr(diags);
                let span = lhs.span().merge(rhs.span());
                Stmt::Assign {
                    target: lhs.clone(),
                    value: Expr::Binary {
                        op: BinOp::Add,
                        lhs: Box::new(lhs),
                        rhs: Box::new(rhs),
                        span,
                    },
                    span,
                }
            }
            TokenKind::MinusEq => {
                self.bump();
                let rhs = self.parse_expr(diags);
                let span = lhs.span().merge(rhs.span());
                Stmt::Assign {
                    target: lhs.clone(),
                    value: Expr::Binary {
                        op: BinOp::Sub,
                        lhs: Box::new(lhs),
                        rhs: Box::new(rhs),
                        span,
                    },
                    span,
                }
            }
            TokenKind::PlusPlus => {
                let end = self.bump().span;
                let span = lhs.span().merge(end);
                Stmt::Assign {
                    target: lhs.clone(),
                    value: Expr::Binary {
                        op: BinOp::Add,
                        lhs: Box::new(lhs),
                        rhs: Box::new(Expr::IntLit(1, end)),
                        span,
                    },
                    span,
                }
            }
            TokenKind::MinusMinus => {
                let end = self.bump().span;
                let span = lhs.span().merge(end);
                Stmt::Assign {
                    target: lhs.clone(),
                    value: Expr::Binary {
                        op: BinOp::Sub,
                        lhs: Box::new(lhs),
                        rhs: Box::new(Expr::IntLit(1, end)),
                        span,
                    },
                    span,
                }
            }
            _ => {
                let span = lhs.span();
                Stmt::Expr { expr: lhs, span }
            }
        };
        if consume_semi {
            self.expect_semi(diags);
        }
        stmt
    }

    fn parse_expr(&mut self, diags: &mut Vec<Diagnostic>) -> Expr {
        self.parse_expr_bp(diags, 0)
    }

    fn parse_expr_bp(&mut self, diags: &mut Vec<Diagnostic>, min_bp: u8) -> Expr {
        let mut lhs = self.parse_prefix(diags);

        loop {
            // Postfix operators: `[...]` index/slice, `.field`
            if matches!(self.peek_kind(), TokenKind::LBracket) {
                self.bump();
                if matches!(self.peek_kind(), TokenKind::Colon) {
                    self.bump();
                    let end_expr = if matches!(self.peek_kind(), TokenKind::RBracket) {
                        None
                    } else {
                        Some(Box::new(self.parse_expr(diags)))
                    };
                    let rspan = if matches!(self.peek_kind(), TokenKind::RBracket) {
                        self.bump().span
                    } else {
                        self.peek().span
                    };
                    let span = lhs.span().merge(rspan);
                    lhs = Expr::Slice {
                        base: Box::new(lhs),
                        start: None,
                        end: end_expr,
                        span,
                    };
                    continue;
                } else {
                    let first = self.parse_expr(diags);
                    if matches!(self.peek_kind(), TokenKind::Colon) {
                        self.bump();
                        let end_expr = if matches!(self.peek_kind(), TokenKind::RBracket) {
                            None
                        } else {
                            Some(Box::new(self.parse_expr(diags)))
                        };
                        let rspan = if matches!(self.peek_kind(), TokenKind::RBracket) {
                            self.bump().span
                        } else {
                            self.peek().span
                        };
                        let span = lhs.span().merge(rspan);
                        lhs = Expr::Slice {
                            base: Box::new(lhs),
                            start: Some(Box::new(first)),
                            end: end_expr,
                            span,
                        };
                        continue;
                    } else {
                        let rspan = if matches!(self.peek_kind(), TokenKind::RBracket) {
                            self.bump().span
                        } else {
                            self.peek().span
                        };
                        let span = lhs.span().merge(rspan);
                        lhs = Expr::Index {
                            base: Box::new(lhs),
                            index: Box::new(first),
                            span,
                        };
                        continue;
                    }
                }
            }

            if matches!(self.peek_kind(), TokenKind::Dot) {
                self.bump();
                let (field, fspan) = self.expect_ident(diags, "field access");
                let span = lhs.span().merge(fspan);
                lhs = Expr::FieldAccess {
                    base: Box::new(lhs),
                    field,
                    span,
                };
                continue;
            }

            let op_info = match self.peek_kind() {
                TokenKind::OrOr => Some((BinOp::Or, 1, 2)),
                TokenKind::AndAnd => Some((BinOp::And, 3, 4)),
                TokenKind::EqEq => Some((BinOp::Eq, 5, 6)),
                TokenKind::NotEq => Some((BinOp::NotEq, 5, 6)),
                TokenKind::Lt => Some((BinOp::Lt, 5, 6)),
                TokenKind::LtEq => Some((BinOp::LtEq, 5, 6)),
                TokenKind::Gt => Some((BinOp::Gt, 5, 6)),
                TokenKind::GtEq => Some((BinOp::GtEq, 5, 6)),
                TokenKind::Pipe => Some((BinOp::BitOr, 7, 8)),
                TokenKind::Caret => Some((BinOp::BitXor, 9, 10)),
                TokenKind::Amp => Some((BinOp::BitAnd, 11, 12)),
                TokenKind::Shl => Some((BinOp::Shl, 11, 12)),
                TokenKind::Shr => Some((BinOp::Shr, 11, 12)),
                TokenKind::Plus => Some((BinOp::Add, 13, 14)),
                TokenKind::Minus => Some((BinOp::Sub, 13, 14)),
                TokenKind::Star => Some((BinOp::Mul, 15, 16)),
                TokenKind::Slash => Some((BinOp::Div, 15, 16)),
                TokenKind::Percent => Some((BinOp::Rem, 15, 16)),
                _ => None,
            };

            if let Some((op, l_bp, r_bp)) = op_info {
                if l_bp < min_bp {
                    break;
                }
                self.bump();
                let rhs = self.parse_expr_bp(diags, r_bp);
                let span = lhs.span().merge(rhs.span());
                lhs = Expr::Binary {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                    span,
                };
            } else {
                break;
            }
        }

        lhs
    }

    fn parse_prefix(&mut self, diags: &mut Vec<Diagnostic>) -> Expr {
        let tok = self.peek().clone();
        match tok.kind {
            TokenKind::IntLit(n) => {
                self.bump();
                Expr::IntLit(n, tok.span)
            }
            TokenKind::FloatLit(s) => {
                self.bump();
                Expr::FloatLit(s, tok.span)
            }
            TokenKind::True => {
                self.bump();
                Expr::BoolLit(true, tok.span)
            }
            TokenKind::False => {
                self.bump();
                Expr::BoolLit(false, tok.span)
            }
            TokenKind::StringLit(s) => {
                self.bump();
                Expr::StringLit(s, tok.span)
            }
            TokenKind::Minus => {
                self.bump();
                let rhs = self.parse_expr_bp(diags, 17);
                let span = tok.span.merge(rhs.span());
                Expr::Unary {
                    op: UnOp::Neg,
                    expr: Box::new(rhs),
                    span,
                }
            }
            TokenKind::Bang => {
                self.bump();
                let rhs = self.parse_expr_bp(diags, 17);
                let span = tok.span.merge(rhs.span());
                Expr::Unary {
                    op: UnOp::Not,
                    expr: Box::new(rhs),
                    span,
                }
            }
            TokenKind::LParen => {
                self.bump();
                let prev_allow = self.allow_struct_lit;
                self.allow_struct_lit = true;
                let inner = self.parse_expr(diags);
                self.allow_struct_lit = prev_allow;
                if matches!(self.peek_kind(), TokenKind::RParen) {
                    self.bump();
                }
                inner
            }
            TokenKind::LBracket => {
                // Slice or array literal `[]ElemTy{ e1, e2, ... }` or `[N]ElemTy{ e1, e2, ... }`
                let ty = self.parse_type(diags);
                let elem_ty = match ty {
                    TypeExpr::Slice(inner) | TypeExpr::Array(_, inner) => *inner,
                    other => other,
                };
                if matches!(self.peek_kind(), TokenKind::LBrace) {
                    self.bump();
                }
                let prev_allow = self.allow_struct_lit;
                self.allow_struct_lit = true;
                self.eat_semis();
                let mut elems = Vec::new();
                while !matches!(self.peek_kind(), TokenKind::RBrace | TokenKind::Eof) {
                    elems.push(self.parse_expr(diags));
                    if matches!(self.peek_kind(), TokenKind::Comma) {
                        self.bump();
                        self.eat_semis();
                    } else {
                        self.eat_semis();
                        break;
                    }
                }
                self.allow_struct_lit = prev_allow;
                let end = if matches!(self.peek_kind(), TokenKind::RBrace) {
                    self.bump().span
                } else {
                    tok.span
                };
                Expr::SliceLit {
                    elem_ty,
                    elems,
                    span: tok.span.merge(end),
                }
            }
            TokenKind::Adj => {
                self.bump();
                let inner = self.parse_expr_bp(diags, 17);
                let span = tok.span.merge(inner.span());
                Expr::QuantumMod {
                    kind: ModKind::Adj,
                    expr: Box::new(inner),
                    span,
                }
            }
            TokenKind::Ctrl => {
                self.bump();
                if matches!(self.peek_kind(), TokenKind::LParen) {
                    self.bump();
                }
                let mut ctrls = Vec::new();
                while !matches!(self.peek_kind(), TokenKind::RParen | TokenKind::Eof) {
                    ctrls.push(self.parse_expr(diags));
                    if matches!(self.peek_kind(), TokenKind::Comma) {
                        self.bump();
                    } else {
                        break;
                    }
                }
                if matches!(self.peek_kind(), TokenKind::RParen) {
                    self.bump();
                }
                let inner = self.parse_expr_bp(diags, 17);
                let span = tok.span.merge(inner.span());
                Expr::QuantumMod {
                    kind: ModKind::Ctrl(ctrls),
                    expr: Box::new(inner),
                    span,
                }
            }
            TokenKind::QAlloc => {
                self.bump();
                let mut size = None;
                let mut ty_override = None;
                if matches!(self.peek_kind(), TokenKind::LBracket) {
                    self.bump();
                    if matches!(
                        self.peek_kind(),
                        TokenKind::QInt | TokenKind::QReg | TokenKind::QFixed | TokenKind::Qubit
                    ) {
                        let t = self.parse_type(diags);
                        if let TypeExpr::QInt(ref c) | TypeExpr::QReg(ref c) = t {
                            size = Some(c.clone());
                        }
                        ty_override = Some(t);
                    } else {
                        size = Some(self.parse_const_expr(diags));
                    }
                    if matches!(self.peek_kind(), TokenKind::RBracket) {
                        self.bump();
                    }
                }
                let mut end_span = tok.span;
                if matches!(self.peek_kind(), TokenKind::LParen) {
                    self.bump();
                    if matches!(self.peek_kind(), TokenKind::RParen) {
                        end_span = self.bump().span;
                    }
                }
                Expr::QAlloc {
                    size,
                    ty_override,
                    span: tok.span.merge(end_span),
                }
            }
            TokenKind::Measure => {
                self.bump();
                if matches!(self.peek_kind(), TokenKind::LParen) {
                    self.bump();
                }
                let arg = self.parse_expr(diags);
                let end_span = if matches!(self.peek_kind(), TokenKind::RParen) {
                    self.bump().span
                } else {
                    arg.span()
                };
                Expr::Measure {
                    arg: Box::new(arg),
                    span: tok.span.merge(end_span),
                }
            }
            TokenKind::Ident(name) => {
                self.bump();
                if matches!(self.peek_kind(), TokenKind::LParen) {
                    self.bump();
                    let prev_allow = self.allow_struct_lit;
                    self.allow_struct_lit = true;
                    self.eat_semis();
                    let mut args = Vec::new();
                    while !matches!(self.peek_kind(), TokenKind::RParen | TokenKind::Eof) {
                        args.push(self.parse_expr(diags));
                        if matches!(self.peek_kind(), TokenKind::Comma) {
                            self.bump();
                            self.eat_semis();
                        } else {
                            self.eat_semis();
                            break;
                        }
                    }
                    self.allow_struct_lit = prev_allow;
                    let end = if matches!(self.peek_kind(), TokenKind::RParen) {
                        self.bump().span
                    } else {
                        tok.span
                    };
                    Expr::Call {
                        callee: name,
                        args,
                        span: tok.span.merge(end),
                    }
                } else if self.allow_struct_lit && matches!(self.peek_kind(), TokenKind::LBrace) {
                    self.bump(); // `{`
                    self.eat_semis();
                    let mut fields = Vec::new();
                    let mut idx = 0usize;
                    while !matches!(self.peek_kind(), TokenKind::RBrace | TokenKind::Eof) {
                        if let TokenKind::Ident(fname) = self.peek_kind().clone() {
                            if matches!(self.peek_n_kind(1), TokenKind::Colon) {
                                self.bump(); // field name
                                self.bump(); // `:`
                                let val = self.parse_expr(diags);
                                fields.push((fname, val));
                            } else {
                                let val = self.parse_expr(diags);
                                fields.push((format!("_{}", idx), val));
                            }
                        } else {
                            let val = self.parse_expr(diags);
                            fields.push((format!("_{}", idx), val));
                        }
                        idx += 1;
                        if matches!(self.peek_kind(), TokenKind::Comma) {
                            self.bump();
                            self.eat_semis();
                        } else {
                            self.eat_semis();
                            break;
                        }
                    }
                    let end = if matches!(self.peek_kind(), TokenKind::RBrace) {
                        self.bump().span
                    } else {
                        tok.span
                    };
                    Expr::StructLit {
                        name,
                        fields,
                        span: tok.span.merge(end),
                    }
                } else {
                    Expr::Ident(name, tok.span)
                }
            }
            _ => {
                diags.push(Diagnostic::error(
                    "E0210",
                    format!("unexpected token `{:?}` in expression", tok.kind),
                    tok.span,
                ));
                self.bump();
                Expr::IntLit(0, tok.span)
            }
        }
    }
}

/// Canonical QGo source code formatter (`qgo fmt`).
pub fn format_source(ast: &SourceFile) -> String {
    let mut out = String::new();
    out.push_str(&format!("package {}\n\n", ast.package));
    for imp in &ast.imports {
        out.push_str(&format!("import \"{}\"\n", imp));
    }
    if !ast.imports.is_empty() {
        out.push('\n');
    }
    for (idx, item) in ast.items.iter().enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        match item {
            TopLevelDecl::Const(c) => {
                out.push_str(&format!("const {} = {}\n", c.name, format_expr(&c.value)));
            }
            TopLevelDecl::Struct(s) => {
                out.push_str(&format!("struct {} {{\n", s.name));
                for f in &s.fields {
                    out.push_str(&format!("    {} {}\n", f.name, f.ty));
                }
                out.push_str("}\n");
            }
            TopLevelDecl::Func(f) => {
                let params_str: Vec<String> = f
                    .params
                    .iter()
                    .map(|p| format!("{} {}", p.name, p.ty))
                    .collect();
                let purity_str = match f.purity {
                    PurityMod::Hybrid => "".to_owned(),
                    PurityMod::Unitary => " unitary".to_owned(),
                    PurityMod::QFree => " qfree".to_owned(),
                };
                let ret_str = f
                    .ret_ty
                    .as_ref()
                    .map(|t| format!(" -> {}", t))
                    .unwrap_or_default();
                out.push_str(&format!(
                    "func {}({}){}{} {{\n",
                    f.name,
                    params_str.join(", "),
                    purity_str,
                    ret_str
                ));
                for stmt in &f.body.stmts {
                    format_stmt(stmt, 1, &mut out);
                }
                out.push_str("}\n");
            }
        }
    }
    out
}

fn format_stmt(stmt: &Stmt, indent: usize, out: &mut String) {
    let pad = "    ".repeat(indent);
    match stmt {
        Stmt::Decl { name, ty, init, .. } => {
            if let Some(t) = ty {
                out.push_str(&format!("{}var {} {} = {}\n", pad, name, t, format_expr(init)));
            } else {
                out.push_str(&format!("{}{} := {}\n", pad, name, format_expr(init)));
            }
        }
        Stmt::Assign { target, value, .. } => {
            out.push_str(&format!("{}{} = {}\n", pad, format_expr(target), format_expr(value)));
        }
        Stmt::DeferAdj { call, target, .. } => {
            if let Some(t) = target {
                out.push_str(&format!("{}defer adj {} -> {}\n", pad, format_expr(call), t));
            } else {
                out.push_str(&format!("{}defer adj {}\n", pad, format_expr(call)));
            }
        }
        Stmt::QuantumMod { kind, body, .. } => {
            let prefix = match kind {
                ModKind::Adj => "adj".to_owned(),
                ModKind::Ctrl(cs) => {
                    let s: Vec<_> = cs.iter().map(format_expr).collect();
                    format!("ctrl({})", s.join(", "))
                }
            };
            let mut inner_str = String::new();
            format_stmt(body, 0, &mut inner_str);
            out.push_str(&format!("{}{} {}", pad, prefix, inner_str.trim_start()));
        }
        Stmt::If {
            cond,
            then_block,
            else_block,
            ..
        } => {
            out.push_str(&format!("{}if {} {{\n", pad, format_expr(cond)));
            for s in &then_block.stmts {
                format_stmt(s, indent + 1, out);
            }
            if let Some(eb) = else_block {
                out.push_str(&format!("{}}} else {{\n", pad));
                for s in &eb.stmts {
                    format_stmt(s, indent + 1, out);
                }
            }
            out.push_str(&format!("{}}}\n", pad));
        }
        Stmt::For { cond, body, .. } => {
            if let Some(c) = cond {
                out.push_str(&format!("{}for {} {{\n", pad, format_expr(c)));
            } else {
                out.push_str(&format!("{}for {{\n", pad));
            }
            for s in &body.stmts {
                format_stmt(s, indent + 1, out);
            }
            out.push_str(&format!("{}}}\n", pad));
        }
        Stmt::Return { value, .. } => {
            if let Some(v) = value {
                out.push_str(&format!("{}return {}\n", pad, format_expr(v)));
            } else {
                out.push_str(&format!("{}return\n", pad));
            }
        }
        Stmt::Break(_) => out.push_str(&format!("{}break\n", pad)),
        Stmt::Continue(_) => out.push_str(&format!("{}continue\n", pad)),
        Stmt::Block(b) => {
            out.push_str(&format!("{}{{\n", pad));
            for s in &b.stmts {
                format_stmt(s, indent + 1, out);
            }
            out.push_str(&format!("{}}}\n", pad));
        }
        Stmt::Expr { expr, .. } => {
            out.push_str(&format!("{}{}\n", pad, format_expr(expr)));
        }
    }
}

pub fn format_expr(expr: &Expr) -> String {
    match expr {
        Expr::Ident(s, _) => s.clone(),
        Expr::IntLit(n, _) => n.to_string(),
        Expr::FloatLit(s, _) => s.clone(),
        Expr::BoolLit(b, _) => b.to_string(),
        Expr::StringLit(s, _) => format!("\"{}\"", s),
        Expr::StructLit { name, fields, .. } => {
            let fs: Vec<_> = fields
                .iter()
                .map(|(k, v)| format!("{}: {}", k, format_expr(v)))
                .collect();
            format!("{}{{ {} }}", name, fs.join(", "))
        }
        Expr::SliceLit { elem_ty, elems, .. } => {
            let es: Vec<_> = elems.iter().map(format_expr).collect();
            format!("[]{}{{ {} }}", elem_ty, es.join(", "))
        }
        Expr::QAlloc { size, .. } => match size {
            Some(n) => format!("qalloc[{}]()", n),
            None => "qalloc()".to_owned(),
        },
        Expr::Measure { arg, .. } => format!("measure({})", format_expr(arg)),
        Expr::Call { callee, args, .. } => {
            let a: Vec<_> = args.iter().map(format_expr).collect();
            format!("{}({})", callee, a.join(", "))
        }
        Expr::Index { base, index, .. } => format!("{}[{}]", format_expr(base), format_expr(index)),
        Expr::Slice { base, start, end, .. } => {
            let s = start.as_ref().map(|e| format_expr(e)).unwrap_or_default();
            let e = end.as_ref().map(|e| format_expr(e)).unwrap_or_default();
            format!("{}[{}:{}]", format_expr(base), s, e)
        }
        Expr::Binary { op, lhs, rhs, .. } => {
            format!("({} {} {})", format_expr(lhs), op, format_expr(rhs))
        }
        Expr::Unary { op, expr, .. } => {
            let p = match op {
                UnOp::Neg => "-",
                UnOp::Not => "!",
            };
            format!("{}{}", p, format_expr(expr))
        }
        Expr::QuantumMod { kind, expr, .. } => match kind {
            ModKind::Adj => format!("adj {}", format_expr(expr)),
            ModKind::Ctrl(cs) => {
                let s: Vec<_> = cs.iter().map(format_expr).collect();
                format!("ctrl({}) {}", s.join(", "), format_expr(expr))
            }
        },
        Expr::FieldAccess { base, field, .. } => format!("{}.{}", format_expr(base), field),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_grover_and_defer_adj() {
        let src = r#"package grover

func ComputeOracle(x qreg[3], flag qubit) qfree {
    ctrl(x[0], x[1], x[2]) X(flag)
}

func GroverStep(reg qreg[3]) unitary {
    H(reg[:])
    flag := qalloc()
    ComputeOracle(reg, flag)
    defer adj ComputeOracle(reg, flag) -> flag
    Z(flag)
}
"#;
        let (ast, diags) = parse_source(FileId(0), src);
        assert!(diags.is_empty(), "parse errors: {:?}", diags);
        assert_eq!(ast.package, "grover");
        assert_eq!(ast.items.len(), 2);
    }
}
