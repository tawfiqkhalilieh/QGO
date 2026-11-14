//! `qgo_hir`: Symbol resolution, type inference, Purity hierarchy enforcement,
//! and Pass 2 CFG Quantum Linearity & Ancilla-Leak Checker.

use std::collections::HashMap;
use qgo_span::{Diagnostic, Label, Span};
use qgo_syntax::{
    BinOp, Block, ConstExpr, Expr, FuncDecl, ModKind, PurityMod, SourceFile, Stmt, TopLevelDecl,
    TypeExpr,
};

/// Resolved semantic type in HIR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ty {
    Void,
    Bool,
    Int,
    Int64,
    Uint,
    Float64,
    StringTy,
    Array(usize, Box<Ty>),
    Slice(Box<Ty>),
    Struct(String),
    Qubit,
    QReg(usize),
    QInt(usize),
    QFixed(usize, usize),
}

impl Ty {
    pub fn is_quantum(&self) -> bool {
        match self {
            Ty::Qubit | Ty::QReg(_) | Ty::QInt(_) | Ty::QFixed(_, _) => true,
            Ty::Array(_, inner) | Ty::Slice(inner) => inner.is_quantum(),
            _ => false,
        }
    }

    pub fn qubit_width(&self) -> usize {
        match self {
            Ty::Qubit => 1,
            Ty::QReg(n) | Ty::QInt(n) | Ty::QFixed(n, _) => *n,
            Ty::Array(n, inner) => n * inner.qubit_width(),
            _ => 0,
        }
    }
}

impl std::fmt::Display for Ty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Ty::Void => write!(f, "void"),
            Ty::Bool => write!(f, "bool"),
            Ty::Int => write!(f, "int"),
            Ty::Int64 => write!(f, "int64"),
            Ty::Uint => write!(f, "uint"),
            Ty::Float64 => write!(f, "float64"),
            Ty::StringTy => write!(f, "string"),
            Ty::Array(n, t) => write!(f, "[{}]{}", n, t),
            Ty::Slice(t) => write!(f, "[]{}", t),
            Ty::Struct(s) => write!(f, "{}", s),
            Ty::Qubit => write!(f, "qubit"),
            Ty::QReg(n) => write!(f, "qreg[{}]", n),
            Ty::QInt(n) => write!(f, "qint[{}]", n),
            Ty::QFixed(n, m) => write!(f, "qfixed[{}, {}]", n, m),
        }
    }
}

/// Lifecycle state of an allocated qubit/register across the HIR Control-Flow Graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QubitState {
    Unallocated,
    /// Freshly allocated in $|0\rangle$ or uncomputed via `defer adj`.
    Zeroed,
    /// In superposition or entangled with other qubits.
    Active,
    /// Collapsed via `measure(q)`.
    Measured,
    /// Freed or moved out.
    Freed,
}

#[derive(Debug, Clone)]
pub struct VarInfo {
    pub name: String,
    pub ty: Ty,
    pub is_param: bool,
    pub is_local_qalloc: bool,
    pub state: QubitState,
    pub has_defer_uncompute: bool,
    pub decl_span: Span,
}

#[derive(Debug, Clone)]
pub struct FuncSig {
    pub name: String,
    pub params: Vec<(String, Ty)>,
    pub purity: PurityMod,
    pub ret_ty: Ty,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct HirModule {
    pub package: String,
    pub consts: HashMap<String, usize>,
    pub const_types: HashMap<String, Ty>,
    pub structs: HashMap<String, Vec<(String, Ty)>>,
    pub funcs: HashMap<String, FuncSig>,
    pub ast: SourceFile,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BorrowRange {
    Full(usize),
    Static(usize, usize),
    Dynamic(String),
}

/// Represents a resolved quantum wire region `var[start..end]` for disjointness analysis.
#[derive(Debug, Clone, PartialEq, Eq)]
struct QuantumBorrow {
    var: String,
    range: BorrowRange,
    span: Span,
}

impl QuantumBorrow {
    fn overlaps(&self, other: &QuantumBorrow) -> bool {
        if self.var != other.var {
            return false;
        }
        match (&self.range, &other.range) {
            (BorrowRange::Full(_), _) | (_, BorrowRange::Full(_)) => true,
            (BorrowRange::Static(s1, e1), BorrowRange::Static(s2, e2)) => s1 < e2 && s2 < e1,
            (BorrowRange::Dynamic(d1), BorrowRange::Dynamic(d2)) => d1 == d2,
            _ => false,
        }
    }
}

/// Runs HIR semantic analysis, type inference, Purity checking, and Pass 2 Quantum Linearity verification.
pub fn check_module(ast: &SourceFile) -> (HirModule, Vec<Diagnostic>) {
    let mut diags = Vec::new();
    let mut consts = HashMap::new();
    let mut const_types = HashMap::new();
    let mut structs = HashMap::new();
    let mut funcs = builtin_gate_signatures();

    // 1. Collect compile-time constants
    for item in &ast.items {
        if let TopLevelDecl::Const(c) = item {
            match &c.value {
                Expr::IntLit(n, _) => {
                    consts.insert(c.name.clone(), (*n).max(0) as usize);
                    const_types.insert(c.name.clone(), Ty::Int);
                }
                Expr::FloatLit(_, _) => {
                    const_types.insert(c.name.clone(), Ty::Float64);
                }
                Expr::StringLit(_, _) => {
                    const_types.insert(c.name.clone(), Ty::StringTy);
                }
                Expr::BoolLit(_, _) => {
                    const_types.insert(c.name.clone(), Ty::Bool);
                }
                _ => {
                    const_types.insert(c.name.clone(), Ty::Int);
                }
            }
        }
    }

    // 1b. Collect struct definitions
    for item in &ast.items {
        if let TopLevelDecl::Struct(s) = item {
            let mut fields = Vec::new();
            for f in &s.fields {
                let fty = resolve_type(&f.ty, &consts, &mut diags, f.span);
                fields.push((f.name.clone(), fty));
            }
            structs.insert(s.name.clone(), fields);
        }
    }

    // 2. Collect top-level function signatures
    for item in &ast.items {
        if let TopLevelDecl::Func(f) = item {
            let mut params = Vec::new();
            for p in &f.params {
                let ty = resolve_type(&p.ty, &consts, &mut diags, p.span);
                params.push((p.name.clone(), ty));
            }
            let ret_ty = f
                .ret_ty
                .as_ref()
                .map(|t| resolve_type(t, &consts, &mut diags, f.span))
                .unwrap_or(Ty::Void);
            funcs.insert(
                f.name.clone(),
                FuncSig {
                    name: f.name.clone(),
                    params,
                    purity: f.purity,
                    ret_ty,
                    span: f.span,
                },
            );
        }
    }

    // 3. Check each function body (Types, Purity Hierarchy, Aliasing, Ancilla Lifecycle)
    for item in &ast.items {
        if let TopLevelDecl::Func(f) = item {
            let mut checker = FuncChecker {
                func: f,
                consts: &consts,
                const_types: &const_types,
                structs: &structs,
                funcs: &funcs,
                vars: HashMap::new(),
                diags: &mut diags,
            };
            checker.check();
        }
    }

    (
        HirModule {
            package: ast.package.clone(),
            consts,
            const_types,
            structs,
            funcs,
            ast: ast.clone(),
        },
        diags,
    )
}

fn builtin_gate_signatures() -> HashMap<String, FuncSig> {
    let mut m = HashMap::new();
    let dummy = Span::dummy();

    // qfree reversible basis-permutation gates
    for g in ["X", "NOT"] {
        m.insert(
            g.to_owned(),
            FuncSig {
                name: g.to_owned(),
                params: vec![("q".to_owned(), Ty::Qubit)],
                purity: PurityMod::QFree,
                ret_ty: Ty::Void,
                span: dummy,
            },
        );
    }
    for g in ["CX", "CNOT", "Swap", "SWAP"] {
        m.insert(
            g.to_owned(),
            FuncSig {
                name: g.to_owned(),
                params: vec![
                    ("q0".to_owned(), Ty::Qubit),
                    ("q1".to_owned(), Ty::Qubit),
                ],
                purity: PurityMod::QFree,
                ret_ty: Ty::Void,
                span: dummy,
            },
        );
    }
    for g in ["CCX", "Toffoli"] {
        m.insert(
            g.to_owned(),
            FuncSig {
                name: g.to_owned(),
                params: vec![
                    ("c0".to_owned(), Ty::Qubit),
                    ("c1".to_owned(), Ty::Qubit),
                    ("target".to_owned(), Ty::Qubit),
                ],
                purity: PurityMod::QFree,
                ret_ty: Ty::Void,
                span: dummy,
            },
        );
    }

    // Unitary single-qubit superposition / phase gates
    for g in ["H", "Y", "Z", "S", "Sdg", "T", "Tdg"] {
        m.insert(
            g.to_owned(),
            FuncSig {
                name: g.to_owned(),
                params: vec![("q".to_owned(), Ty::Qubit)],
                purity: PurityMod::Unitary,
                ret_ty: Ty::Void,
                span: dummy,
            },
        );
    }

    // Parameterized unitary rotation gates
    for g in ["Rx", "Ry", "Rz", "Phase", "P"] {
        m.insert(
            g.to_owned(),
            FuncSig {
                name: g.to_owned(),
                params: vec![
                    ("theta".to_owned(), Ty::Float64),
                    ("q".to_owned(), Ty::Qubit),
                ],
                purity: PurityMod::Unitary,
                ret_ty: Ty::Void,
                span: dummy,
            },
        );
    }
    for g in ["CP", "CRz"] {
        m.insert(
            g.to_owned(),
            FuncSig {
                name: g.to_owned(),
                params: vec![
                    ("theta".to_owned(), Ty::Float64),
                    ("ctrl".to_owned(), Ty::Qubit),
                    ("target".to_owned(), Ty::Qubit),
                ],
                purity: PurityMod::Unitary,
                ret_ty: Ty::Void,
                span: dummy,
            },
        );
    }

    // Standard library / intrinsic helpers
    for g in ["CuccaroAdd", "CuccaroSub"] {
        m.insert(
            g.to_owned(),
            FuncSig {
                name: g.to_owned(),
                params: vec![
                    ("a".to_owned(), Ty::QInt(4)),
                    ("b".to_owned(), Ty::QInt(4)),
                ],
                purity: PurityMod::QFree,
                ret_ty: Ty::Void,
                span: dummy,
            },
        );
    }
    for g in ["DraperAdd", "DraperSub"] {
        m.insert(
            g.to_owned(),
            FuncSig {
                name: g.to_owned(),
                params: vec![
                    ("a".to_owned(), Ty::QInt(4)),
                    ("b".to_owned(), Ty::QInt(4)),
                ],
                purity: PurityMod::Unitary,
                ret_ty: Ty::Void,
                span: dummy,
            },
        );
    }
    m.insert(
        "reset".to_owned(),
        FuncSig {
            name: "reset".to_owned(),
            params: vec![("q".to_owned(), Ty::Qubit)],
            purity: PurityMod::Hybrid,
            ret_ty: Ty::Void,
            span: dummy,
        },
    );

    // Classical & Hybrid Built-in Functions
    m.insert(
        "len".to_owned(),
        FuncSig {
            name: "len".to_owned(),
            params: vec![("v".to_owned(), Ty::StringTy)],
            purity: PurityMod::QFree,
            ret_ty: Ty::Int,
            span: dummy,
        },
    );
    for (name, ret) in [
        ("int_to_str", Ty::StringTy),
        ("float_to_str", Ty::StringTy),
        ("char_at", Ty::StringTy),
        ("from_byte", Ty::StringTy),
        ("substr", Ty::StringTy),
        ("trim", Ty::StringTy),
        ("join", Ty::StringTy),
        ("replace", Ty::StringTy),
        ("str_to_int", Ty::Int),
        ("byte_at", Ty::Int),
        ("ord", Ty::Int),
        ("contains", Ty::Bool),
        ("starts_with", Ty::Bool),
        ("ends_with", Ty::Bool),
        ("split", Ty::Slice(Box::new(Ty::StringTy))),
    ] {
        m.insert(
            name.to_owned(),
            FuncSig {
                name: name.to_owned(),
                params: Vec::new(),
                purity: PurityMod::QFree,
                ret_ty: ret,
                span: dummy,
            },
        );
    }
    for (name, ret) in [
        ("print", Ty::Void),
        ("println", Ty::Void),
        ("eprintln", Ty::Void),
        ("format", Ty::StringTy),
        ("read_file", Ty::StringTy),
        ("write_file", Ty::Bool),
        ("args", Ty::Slice(Box::new(Ty::StringTy))),
        ("assert", Ty::Void),
        ("assert_eq", Ty::Void),
        ("prob_zero", Ty::Float64),
        ("prob_basis", Ty::Float64),
        ("peek_uint", Ty::Int),
        ("gc_collect", Ty::Int),
        ("gc_live_objects", Ty::Int),
        ("gc_total_swept", Ty::Int),
        ("gc_live_bytes", Ty::Int),
        ("qpool_recycled_count", Ty::Int),
        ("qpool_peak_qubits", Ty::Int),
        ("exit", Ty::Void),
    ] {
        m.insert(
            name.to_owned(),
            FuncSig {
                name: name.to_owned(),
                params: Vec::new(),
                purity: PurityMod::Hybrid,
                ret_ty: ret,
                span: dummy,
            },
        );
    }

    m
}

pub fn resolve_type(
    te: &TypeExpr,
    consts: &HashMap<String, usize>,
    diags: &mut Vec<Diagnostic>,
    span: Span,
) -> Ty {
    match te {
        TypeExpr::Bool => Ty::Bool,
        TypeExpr::Int => Ty::Int,
        TypeExpr::Int64 => Ty::Int64,
        TypeExpr::Uint => Ty::Uint,
        TypeExpr::Float64 => Ty::Float64,
        TypeExpr::StringTy => Ty::StringTy,
        TypeExpr::Qubit => Ty::Qubit,
        TypeExpr::QReg(c) => {
            let n = resolve_const(c, consts, diags, span);
            Ty::QReg(n)
        }
        TypeExpr::QInt(c) => {
            let n = resolve_const(c, consts, diags, span);
            Ty::QInt(n)
        }
        TypeExpr::QFixed(c1, c2) => {
            let n = resolve_const(c1, consts, diags, span);
            let m = resolve_const(c2, consts, diags, span);
            Ty::QFixed(n, m)
        }
        TypeExpr::Array(c, inner) => {
            let n = resolve_const(c, consts, diags, span);
            Ty::Array(n, Box::new(resolve_type(inner, consts, diags, span)))
        }
        TypeExpr::Slice(inner) => Ty::Slice(Box::new(resolve_type(inner, consts, diags, span))),
        TypeExpr::Named(s) => Ty::Struct(s.clone()),
    }
}

fn resolve_const(
    c: &ConstExpr,
    consts: &HashMap<String, usize>,
    diags: &mut Vec<Diagnostic>,
    span: Span,
) -> usize {
    match c.resolve_static(consts) {
        Some(n) => n,
        None => {
            diags.push(Diagnostic::error(
                "E0300",
                format!("unresolved compile-time constant `{}`", c),
                span,
            ));
            1
        }
    }
}

struct FuncChecker<'a> {
    func: &'a FuncDecl,
    consts: &'a HashMap<String, usize>,
    const_types: &'a HashMap<String, Ty>,
    structs: &'a HashMap<String, Vec<(String, Ty)>>,
    funcs: &'a HashMap<String, FuncSig>,
    vars: HashMap<String, VarInfo>,
    diags: &'a mut Vec<Diagnostic>,
}

impl<'a> FuncChecker<'a> {
    fn check(&mut self) {
        for p in &self.func.params {
            let ty = resolve_type(&p.ty, self.consts, self.diags, p.span);
            self.vars.insert(
                p.name.clone(),
                VarInfo {
                    name: p.name.clone(),
                    ty,
                    is_param: true,
                    is_local_qalloc: false,
                    state: QubitState::Active,
                    has_defer_uncompute: false,
                    decl_span: p.span,
                },
            );
        }

        self.check_block(&self.func.body);

        // Pass 2.3: No-Leaked-Ancilla Rule at function exit
        if matches!(self.func.purity, PurityMod::Unitary | PurityMod::QFree) {
            for var in self.vars.values() {
                if var.is_local_qalloc
                    && var.state == QubitState::Active
                    && !var.has_defer_uncompute
                {
                    self.diags.push(
                        Diagnostic::error(
                            "E0401",
                            format!(
                                "EntangledAncillaLeak: local quantum variable `{}` is in `Active` state at exit of `{}` function `{}`",
                                var.name, self.func.purity, self.func.name
                            ),
                            var.decl_span,
                        )
                        .with_note(format!(
                            "uncompute `{0}` back to |0> using `defer adj <Oracle>(...) -> {0}` or return it",
                            var.name
                        )),
                    );
                }
            }
        }
    }

    fn check_block(&mut self, block: &Block) {
        for stmt in &block.stmts {
            self.check_stmt(stmt, &[]);
        }
    }

    fn check_stmt(&mut self, stmt: &Stmt, enclosing_ctrls: &[QuantumBorrow]) {
        match stmt {
            Stmt::Decl {
                name,
                ty,
                init,
                span,
            } => {
                // Check for linear move violation if `init` is an existing quantum variable identifier
                if let Expr::Ident(src_var, id_span) = init {
                    if let Some(vinfo) = self.vars.get(src_var) {
                        if vinfo.ty.is_quantum() {
                            self.diags.push(
                                Diagnostic::error(
                                    "E0402",
                                    format!(
                                        "LinearTypeCopy: cannot copy quantum linear variable `{}` of type `{}`",
                                        src_var, vinfo.ty
                                    ),
                                    *id_span,
                                )
                                .with_label(Label::secondary(
                                    vinfo.decl_span,
                                    format!("`{}` declared here as linear quantum resource", src_var),
                                ))
                                .with_note("quantum types obey the No-Cloning Theorem and must be mutated in-place"),
                            );
                        }
                    }
                }

                let inferred_ty = self.check_expr(init, enclosing_ctrls);
                let final_ty = if let Some(te) = ty {
                    resolve_type(te, self.consts, self.diags, *span)
                } else {
                    inferred_ty
                };

                let is_local_qalloc = matches!(init, Expr::QAlloc { .. })
                    || (final_ty.is_quantum() && matches!(init, Expr::Call { .. }));
                let initial_state = if matches!(init, Expr::QAlloc { .. }) {
                    QubitState::Zeroed
                } else if final_ty.is_quantum() {
                    QubitState::Active
                } else {
                    QubitState::Unallocated
                };

                self.vars.insert(
                    name.clone(),
                    VarInfo {
                        name: name.clone(),
                        ty: final_ty,
                        is_param: false,
                        is_local_qalloc,
                        state: initial_state,
                        has_defer_uncompute: false,
                        decl_span: *span,
                    },
                );
            }
            Stmt::Assign {
                target,
                value,
                span,
            } => {
                let lhs_ty = self.check_expr(target, enclosing_ctrls);
                let _rhs_ty = self.check_expr(value, enclosing_ctrls);
                if lhs_ty.is_quantum() {
                    if let Expr::Ident(vname, _) = target {
                        if let Some(v) = self.vars.get_mut(vname) {
                            v.state = QubitState::Active;
                        }
                    }
                }
                let _ = span;
            }
            Stmt::DeferAdj { call, target, span } => {
                // Verify target exists and mark it as uncomputed (`Zeroed` at scope exit)
                self.check_expr(call, enclosing_ctrls);
                if let Some(tname) = target {
                    if let Some(v) = self.vars.get_mut(tname) {
                        v.has_defer_uncompute = true;
                        v.state = QubitState::Zeroed;
                    } else {
                        self.diags.push(Diagnostic::error(
                            "E0304",
                            format!("undeclared target `{}` in `defer adj` statement", tname),
                            *span,
                        ));
                    }
                } else if let Expr::Call { args, .. } = call {
                    // If no explicit `-> target`, check if last arg is a local qalloc
                    if let Some(Expr::Ident(last_name, _)) = args.last() {
                        if let Some(v) = self.vars.get_mut(last_name) {
                            if v.is_local_qalloc {
                                v.has_defer_uncompute = true;
                            }
                        }
                    }
                }
            }
            Stmt::QuantumMod { kind, body, span } => {
                let mut combined_ctrls = enclosing_ctrls.to_vec();
                if let ModKind::Ctrl(ctrl_exprs) = kind {
                    for ce in ctrl_exprs {
                        let _ = self.check_expr(ce, &[]);
                        if let Some(b) = self.extract_borrow(ce) {
                            // Check disjointness among controls themselves
                            for prev in &combined_ctrls {
                                if b.overlaps(prev) {
                                    self.diags.push(
                                        Diagnostic::error(
                                            "E0400",
                                            format!(
                                                "QuantumAliasingViolation: control qubit `{}` overlaps with existing control on `{}`",
                                                b.var, prev.var
                                            ),
                                            *span,
                                        )
                                        .with_label(Label::secondary(prev.span, "first borrowed as control here")),
                                    );
                                }
                            }
                            combined_ctrls.push(b);
                        }
                    }
                }
                self.check_stmt(body, &combined_ctrls);
            }
            Stmt::If {
                cond,
                then_block,
                else_block,
                span,
            } => {
                let _cond_ty = self.check_expr(cond, enclosing_ctrls);
                // If condition references quantum variables, ensure the condition expression is `qfree`
                if self.expr_touches_quantum(cond) && self.func.purity == PurityMod::QFree {
                    // Allowed inside qfree or unitary as quantum conditional!
                }
                let _ = span;
                self.check_block(then_block);
                if let Some(eb) = else_block {
                    self.check_block(eb);
                }
            }
            Stmt::For {
                init,
                cond,
                post,
                body,
                ..
            } => {
                if let Some(i) = init {
                    self.check_stmt(i, enclosing_ctrls);
                }
                if let Some(c) = cond {
                    self.check_expr(c, enclosing_ctrls);
                }
                if let Some(p) = post {
                    self.check_stmt(p, enclosing_ctrls);
                }
                self.check_block(body);
            }
            Stmt::Return { value, .. } => {
                if let Some(v) = value {
                    self.check_expr(v, enclosing_ctrls);
                    // If returning a local qalloc variable, ownership is transferred to caller (not leaked)
                    if let Expr::Ident(vname, _) = v {
                        if let Some(var) = self.vars.get_mut(vname) {
                            var.has_defer_uncompute = true;
                            var.state = QubitState::Freed;
                        }
                    }
                }
            }
            Stmt::Break(_) | Stmt::Continue(_) => {}
            Stmt::Block(b) => self.check_block(b),
            Stmt::Expr { expr, .. } => {
                self.check_expr(expr, enclosing_ctrls);
            }
        }
    }

    fn check_expr(&mut self, expr: &Expr, enclosing_ctrls: &[QuantumBorrow]) -> Ty {
        match expr {
            Expr::Ident(name, span) => {
                if let Some(v) = self.vars.get(name) {
                    v.ty.clone()
                } else if let Some(cty) = self.const_types.get(name) {
                    cty.clone()
                } else if self.consts.contains_key(name) {
                    Ty::Int
                } else {
                    self.diags.push(Diagnostic::error(
                        "E0305",
                        format!("use of undeclared identifier `{}`", name),
                        *span,
                    ));
                    Ty::Void
                }
            }
            Expr::IntLit(_, _) => Ty::Int,
            Expr::FloatLit(_, _) => Ty::Float64,
            Expr::BoolLit(_, _) => Ty::Bool,
            Expr::StringLit(_, _) => Ty::StringTy,
            Expr::StructLit {
                name, fields, span, ..
            } => {
                for (_, fexpr) in fields {
                    self.check_expr(fexpr, enclosing_ctrls);
                }
                if !self.structs.contains_key(name) {
                    self.diags.push(Diagnostic::error(
                        "E0307",
                        format!("unknown struct type `{}`", name),
                        *span,
                    ));
                }
                Ty::Struct(name.clone())
            }
            Expr::SliceLit {
                elem_ty,
                elems,
                span,
            } => {
                let ety = resolve_type(elem_ty, self.consts, self.diags, *span);
                for e in elems {
                    self.check_expr(e, enclosing_ctrls);
                }
                Ty::Slice(Box::new(ety))
            }
            Expr::QAlloc {
                size,
                ty_override,
                span,
            } => {
                if let Some(te) = ty_override {
                    resolve_type(te, self.consts, self.diags, *span)
                } else if let Some(c) = size {
                    let n = resolve_const(c, self.consts, self.diags, *span);
                    Ty::QReg(n)
                } else {
                    Ty::Qubit
                }
            }
            Expr::Measure { arg, span } => {
                // Pass 2 Purity rule: `measure` is strictly forbidden inside `unitary` or `qfree` functions!
                if matches!(self.func.purity, PurityMod::Unitary | PurityMod::QFree) {
                    self.diags.push(
                        Diagnostic::error(
                            "E0302",
                            format!(
                                "MeasurementInPureFunction: non-unitary `measure` is forbidden inside `{}` function `{}`",
                                self.func.purity, self.func.name
                            ),
                            *span,
                        )
                        .with_note("remove `unitary`/`qfree` modifier to make this a hybrid quantum-classical function"),
                    );
                }
                let arg_ty = self.check_expr(arg, enclosing_ctrls);
                if let Some(b) = self.extract_borrow(arg) {
                    if let Some(v) = self.vars.get_mut(&b.var) {
                        v.state = QubitState::Measured;
                    }
                }
                match arg_ty {
                    Ty::Qubit => Ty::Bool,
                    Ty::QReg(_) | Ty::QInt(_) => Ty::Int,
                    _ => Ty::Int,
                }
            }
            Expr::Call { callee, args, span } => {
                let mut arg_borrows: Vec<QuantumBorrow> = enclosing_ctrls.to_vec();
                for arg in args {
                    self.check_expr(arg, &[]);
                    if let Some(b) = self.extract_borrow(arg) {
                        // Pass 2.1: Disjoint Aliasing Invariant
                        for prev in &arg_borrows {
                            if b.overlaps(prev) {
                                self.diags.push(
                                    Diagnostic::error(
                                        "E0400",
                                        format!(
                                            "QuantumAliasingViolation: overlapping quantum register/qubit borrow on `{}` in call to `{}`",
                                            b.var, callee
                                        ),
                                        b.span,
                                    )
                                    .with_label(Label::secondary(
                                        prev.span,
                                        format!("`{}` previously borrowed in the same operation here", prev.var),
                                    ))
                                    .with_note("quantum gates require mutually disjoint qubit arguments (No-Cloning Theorem)"),
                                );
                            }
                        }
                        // Transition variable state to `Active`
                        if let Some(v) = self.vars.get_mut(&b.var) {
                            if v.state == QubitState::Zeroed {
                                v.state = QubitState::Active;
                            }
                        }
                        arg_borrows.push(b);
                    }
                }

                if callee == "append" {
                    if let Some(first) = args.first() {
                        return self.check_expr(first, enclosing_ctrls);
                    }
                    return Ty::Slice(Box::new(Ty::Int));
                }
                if callee == "reset" {
                    if let Some(first) = args.first() {
                        if let Some(b) = self.extract_borrow(first) {
                            if let Some(v) = self.vars.get_mut(&b.var) {
                                v.state = QubitState::Zeroed;
                                v.has_defer_uncompute = true;
                            }
                        }
                    }
                }

                if let Some(sig) = self.funcs.get(callee).cloned() {
                    // Check Purity Hierarchy:
                    // `qfree` can only call `qfree`.
                    // `unitary` can call `qfree` or `unitary`, NOT `hybrid`.
                    if self.func.purity == PurityMod::QFree && sig.purity != PurityMod::QFree {
                        self.diags.push(
                            Diagnostic::error(
                                "E0301",
                                format!(
                                    "ImpureOperationInQFree: `{}` function `{}` cannot call `{}` operation `{}`",
                                    self.func.purity, self.func.name, sig.purity, callee
                                ),
                                *span,
                            )
                            .with_note("`qfree` functions only permit classical-reversible basis permutations (X, CX, CCX, Swap)"),
                        );
                    } else if self.func.purity == PurityMod::Unitary
                        && sig.purity == PurityMod::Hybrid
                    {
                        self.diags.push(Diagnostic::error(
                            "E0303",
                            format!(
                                "HybridCallInUnitary: `unitary` function `{}` cannot call hybrid function `{}`",
                                self.func.name, callee
                            ),
                            *span,
                        ));
                    }
                    sig.ret_ty
                } else {
                    self.diags.push(Diagnostic::error(
                        "E0306",
                        format!("call to undeclared function or gate `{}`", callee),
                        *span,
                    ));
                    Ty::Void
                }
            }
            Expr::Index { base, index, span } => {
                let bty = self.check_expr(base, enclosing_ctrls);
                let _ity = self.check_expr(index, enclosing_ctrls);
                match bty {
                    Ty::QReg(n) | Ty::QInt(n) | Ty::QFixed(n, _) => {
                        if let Expr::IntLit(idx, _) = index.as_ref() {
                            if *idx < 0 || (*idx as usize) >= n {
                                self.diags.push(Diagnostic::error(
                                    "E0403",
                                    format!(
                                        "QuantumIndexOutOfBounds: index `{}` is out of bounds for quantum register of width `{}`",
                                        idx, n
                                    ),
                                    *span,
                                ));
                            }
                        }
                        Ty::Qubit
                    }
                    Ty::Array(_, inner) | Ty::Slice(inner) => *inner,
                    Ty::StringTy => Ty::Int,
                    _ => Ty::Qubit,
                }
            }
            Expr::Slice {
                base, start, end, ..
            } => {
                let bty = self.check_expr(base, enclosing_ctrls);
                if let Some(s) = start {
                    self.check_expr(s, enclosing_ctrls);
                }
                if let Some(e) = end {
                    self.check_expr(e, enclosing_ctrls);
                }
                match bty {
                    Ty::QReg(n) | Ty::QInt(n) => Ty::QReg(n),
                    Ty::Array(_, inner) | Ty::Slice(inner) => Ty::Slice(inner),
                    other => other,
                }
            }
            Expr::Binary { op, lhs, rhs, .. } => {
                let lty = self.check_expr(lhs, enclosing_ctrls);
                let _rty = self.check_expr(rhs, enclosing_ctrls);
                match op {
                    BinOp::Eq
                    | BinOp::NotEq
                    | BinOp::Lt
                    | BinOp::LtEq
                    | BinOp::Gt
                    | BinOp::GtEq
                    | BinOp::And
                    | BinOp::Or => Ty::Bool,
                    _ => lty,
                }
            }
            Expr::Unary { expr: inner, .. } => self.check_expr(inner, enclosing_ctrls),
            Expr::QuantumMod { kind, expr, .. } => {
                let mut combined = enclosing_ctrls.to_vec();
                if let ModKind::Ctrl(cs) = kind {
                    for c in cs {
                        self.check_expr(c, &[]);
                        if let Some(b) = self.extract_borrow(c) {
                            combined.push(b);
                        }
                    }
                }
                self.check_expr(expr, &combined)
            }
            Expr::FieldAccess { base, field, span } => {
                let bty = self.check_expr(base, enclosing_ctrls);
                if let Ty::Struct(sname) = bty {
                    if let Some(fields) = self.structs.get(&sname) {
                        if let Some((_, fty)) = fields.iter().find(|(n, _)| n == field) {
                            return fty.clone();
                        }
                        self.diags.push(Diagnostic::error(
                            "E0308",
                            format!("struct `{}` has no field `{}`", sname, field),
                            *span,
                        ));
                    }
                }
                Ty::Int
            }
        }
    }

    fn extract_borrow(&self, expr: &Expr) -> Option<QuantumBorrow> {
        match expr {
            Expr::Ident(name, span) => {
                let v = self.vars.get(name)?;
                if !v.ty.is_quantum() {
                    return None;
                }
                let w = v.ty.qubit_width().max(1);
                Some(QuantumBorrow {
                    var: name.clone(),
                    range: BorrowRange::Full(w),
                    span: *span,
                })
            }
            Expr::Index { base, index, span } => {
                if let Expr::Ident(name, _) = base.as_ref() {
                    let v = self.vars.get(name)?;
                    if !v.ty.is_quantum() {
                        return None;
                    }
                    let range = match index.as_ref() {
                        Expr::IntLit(i, _) if *i >= 0 => {
                            let idx = *i as usize;
                            BorrowRange::Static(idx, idx + 1)
                        }
                        Expr::Ident(cname, _) if self.consts.contains_key(cname) => {
                            let idx = self.consts[cname];
                            BorrowRange::Static(idx, idx + 1)
                        }
                        other => BorrowRange::Dynamic(qgo_syntax::format_expr(other)),
                    };
                    Some(QuantumBorrow {
                        var: name.clone(),
                        range,
                        span: *span,
                    })
                } else {
                    None
                }
            }
            Expr::Slice {
                base,
                start,
                end,
                span,
            } => {
                if let Expr::Ident(name, _) = base.as_ref() {
                    let v = self.vars.get(name)?;
                    if !v.ty.is_quantum() {
                        return None;
                    }
                    let total = v.ty.qubit_width().max(1);
                    let s_idx = match start.as_deref() {
                        None => Some(0),
                        Some(Expr::IntLit(i, _)) if *i >= 0 => Some(*i as usize),
                        Some(Expr::Ident(cname, _)) => self.consts.get(cname).copied(),
                        _ => None,
                    };
                    let e_idx = match end.as_deref() {
                        None => Some(total),
                        Some(Expr::IntLit(i, _)) if *i >= 0 => Some(*i as usize),
                        Some(Expr::Ident(cname, _)) => self.consts.get(cname).copied(),
                        _ => None,
                    };
                    let range = match (s_idx, e_idx) {
                        (Some(s), Some(e)) => BorrowRange::Static(s, e),
                        _ => BorrowRange::Dynamic(qgo_syntax::format_expr(expr)),
                    };
                    Some(QuantumBorrow {
                        var: name.clone(),
                        range,
                        span: *span,
                    })
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn expr_touches_quantum(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Ident(name, _) => self.vars.get(name).map(|v| v.ty.is_quantum()).unwrap_or(false),
            Expr::Index { base, .. } | Expr::Slice { base, .. } => self.expr_touches_quantum(base),
            Expr::Binary { lhs, rhs, .. } => {
                self.expr_touches_quantum(lhs) || self.expr_touches_quantum(rhs)
            }
            Expr::Unary { expr: inner, .. } => self.expr_touches_quantum(inner),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qgo_span::FileId;
    use qgo_syntax::parse_source;

    #[test]
    fn test_rejects_cx_same_qubit_and_overlapping_slices() {
        let src = r#"package main
func BadAlias(q qubit, reg qreg[4]) unitary {
    CX(q, q)
    Swap(reg[0:2], reg[1:3])
}
"#;
        let (ast, pdiags) = parse_source(FileId(0), src);
        assert!(pdiags.is_empty());
        let (_, hdiags) = check_module(&ast);
        assert_eq!(hdiags.len(), 2);
        assert!(hdiags.iter().all(|d| d.code == "E0400"));
    }

    #[test]
    fn test_rejects_leaked_ancilla_e0401() {
        let src = r#"package main
func LeakedOracle(x qubit) unitary {
    anc := qalloc()
    CX(x, anc)
}
"#;
        let (ast, _) = parse_source(FileId(0), src);
        let (_, hdiags) = check_module(&ast);
        assert_eq!(hdiags.len(), 1);
        assert_eq!(hdiags[0].code, "E0401");
    }

    #[test]
    fn test_accepts_defer_adj_uncomputed_ancilla() {
        let src = r#"package main
func CleanOracle(x qubit, target qubit) unitary {
    anc := qalloc()
    CX(x, anc)
    defer adj CX(x, anc) -> anc
    CX(anc, target)
}
"#;
        let (ast, _) = parse_source(FileId(0), src);
        let (_, hdiags) = check_module(&ast);
        assert!(hdiags.is_empty(), "expected zero errors, got {:?}", hdiags);
    }

    #[test]
    fn test_rejects_hadamard_in_qfree_and_measure_in_unitary() {
        let src = r#"package main
func BadQFree(q qubit) qfree {
    H(q)
}
func BadUnitary(q qubit) unitary {
    b := measure(q)
}
"#;
        let (ast, _) = parse_source(FileId(0), src);
        let (_, hdiags) = check_module(&ast);
        assert_eq!(hdiags.len(), 2);
        assert_eq!(hdiags[0].code, "E0301");
        assert_eq!(hdiags[1].code, "E0302");
    }
}
