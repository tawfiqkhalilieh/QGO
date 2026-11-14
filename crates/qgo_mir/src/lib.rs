//! `qgo_mir`: Mid-Level Quantum IR featuring explicit `Conjugate` blocks,
//! logical qubit wire addressing, and `defer adj` scope lowering.

use std::collections::HashMap;
use qgo_hir::HirModule;
use qgo_syntax::{
    BinOp, Block, Expr, FuncDecl, ModKind, PurityMod, Stmt, TopLevelDecl, TypeExpr,
};

/// Logical qubit wire reference `(register_name, bit_index)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct QubitWire {
    pub reg: String,
    pub index: usize,
}

impl QubitWire {
    pub fn new(reg: impl Into<String>, index: usize) -> Self {
        Self {
            reg: reg.into(),
            index,
        }
    }
}

impl std::fmt::Display for QubitWire {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}[{}]", self.reg, self.index)
    }
}

/// Elementary or composite unitary/qfree gate in MIR.
#[derive(Debug, Clone, PartialEq)]
pub enum MirGateKind {
    X,
    Y,
    Z,
    H,
    S,
    Sdg,
    T,
    Tdg,
    Rx(f64),
    Ry(f64),
    Rz(f64),
    Phase(f64),
    Swap,
}

impl MirGateKind {
    /// Returns the algebraic Hermitian conjugate ($G^\dagger$) of this gate.
    pub fn adjoint(&self) -> Self {
        match self {
            MirGateKind::X => MirGateKind::X,
            MirGateKind::Y => MirGateKind::Y,
            MirGateKind::Z => MirGateKind::Z,
            MirGateKind::H => MirGateKind::H,
            MirGateKind::S => MirGateKind::Sdg,
            MirGateKind::Sdg => MirGateKind::S,
            MirGateKind::T => MirGateKind::Tdg,
            MirGateKind::Tdg => MirGateKind::T,
            MirGateKind::Rx(theta) => MirGateKind::Rx(-theta),
            MirGateKind::Ry(theta) => MirGateKind::Ry(-theta),
            MirGateKind::Rz(theta) => MirGateKind::Rz(-theta),
            MirGateKind::Phase(theta) => MirGateKind::Phase(-theta),
            MirGateKind::Swap => MirGateKind::Swap,
        }
    }

    /// Returns `true` if `self` followed immediately by `other` on identical wires cancels to $I$.
    pub fn is_inverse_of(&self, other: &Self) -> bool {
        match (self, other) {
            (MirGateKind::X, MirGateKind::X)
            | (MirGateKind::Y, MirGateKind::Y)
            | (MirGateKind::Z, MirGateKind::Z)
            | (MirGateKind::H, MirGateKind::H)
            | (MirGateKind::Swap, MirGateKind::Swap)
            | (MirGateKind::S, MirGateKind::Sdg)
            | (MirGateKind::Sdg, MirGateKind::S)
            | (MirGateKind::T, MirGateKind::Tdg)
            | (MirGateKind::Tdg, MirGateKind::T) => true,
            (MirGateKind::Rx(a), MirGateKind::Rx(b))
            | (MirGateKind::Ry(a), MirGateKind::Ry(b))
            | (MirGateKind::Rz(a), MirGateKind::Rz(b))
            | (MirGateKind::Phase(a), MirGateKind::Phase(b)) => (a + b).abs() < 1e-12,
            _ => false,
        }
    }
}

/// Quantum arithmetic comparison/binary operation in MIR prior toPass 4 synthesis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuantumCmpKind {
    Add,
    Sub,
    Xor,
    Eq,
    Lt,
    Gt,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MirStmt {
    /// Allocate a quantum register of `size` qubits initialized to $|0\dots0\rangle$.
    QAlloc {
        reg: String,
        size: usize,
        is_ancilla: bool,
    },
    /// Free/reset an uncomputed ancilla register in state $|0\dots0\rangle$.
    QFree {
        reg: String,
        size: usize,
    },
    /// Elementary or multi-controlled quantum gate.
    Gate {
        kind: MirGateKind,
        controls: Vec<QubitWire>,
        targets: Vec<QubitWire>,
    },
    /// Pass 3.1 Explicit Conjugation Node ($U \cdot V \cdot U^\dagger$):
    /// Executes `compute` ($U$), then `action` ($V$), then `adj(compute)` ($U^\dagger$).
    /// Under `ctrl(c)`, the Ctrl-Conjugation Theorem transforms this into
    /// `compute` ($U$) -> `ctrl(c) action` ($\text{ctrl}_c V$) -> `adj(compute)` ($U^\dagger$).
    Conjugate {
        compute: Vec<MirStmt>,
        action: Vec<MirStmt>,
        target: Option<String>,
    },
    /// High-level quantum conditional (`if <qfree_expr> { then_body }`) lowered by Pass 4 (`qgo_opt`).
    QuantumIf {
        op: QuantumCmpKind,
        lhs_reg: String,
        rhs_reg: Option<String>,
        rhs_const: Option<i64>,
        width: usize,
        then_body: Vec<MirStmt>,
    },
    /// High-level quantum arithmetic (`CuccaroAdd(a, b)` / `a + b`) lowered by Pass 4 (`qgo_opt`).
    QuantumArith {
        op: QuantumCmpKind,
        lhs_reg: String,
        rhs_reg: String,
        width: usize,
    },
    /// Mid-circuit measurement into classical bit variable.
    Measure {
        qubit: QubitWire,
        dst_var: String,
    },
    /// Mid-circuit qubit reset back to $|0\rangle$.
    Reset {
        qubit: QubitWire,
    },
    /// Classical conditional on a measured boolean/int variable.
    ClassicalIf {
        cond_var: String,
        then_body: Vec<MirStmt>,
        else_body: Vec<MirStmt>,
    },
    /// Hybrid loop conditioned on a classical measurement variable.
    While {
        cond_var: String,
        body: Vec<MirStmt>,
    },
}

#[derive(Debug, Clone)]
pub struct MirParam {
    pub name: String,
    pub width: usize,
    pub is_quantum: bool,
}

#[derive(Debug, Clone)]
pub struct MirFunc {
    pub name: String,
    pub params: Vec<MirParam>,
    pub purity: PurityMod,
    pub body: Vec<MirStmt>,
}

#[derive(Debug, Clone)]
pub struct MirModule {
    pub package: String,
    pub funcs: Vec<MirFunc>,
}

/// Lowers a verified [`HirModule`] into a [`MirModule`] with `Conjugate` blocks.
pub fn lower_to_mir(hir: &HirModule) -> MirModule {
    let mut func_map = HashMap::new();
    for item in &hir.ast.items {
        if let TopLevelDecl::Func(f) = item {
            func_map.insert(f.name.clone(), f);
        }
    }

    let mut mir_funcs = Vec::new();
    for item in &hir.ast.items {
        if let TopLevelDecl::Func(f) = item {
            let mut local_ints = HashMap::new();
            for (k, v) in &hir.consts {
                local_ints.insert(k.clone(), *v as i64);
            }
            let mut lowerer = MirLowerer {
                hir,
                func_map: &func_map,
                reg_widths: HashMap::new(),
                local_ints,
                current_purity: f.purity,
            };
            mir_funcs.push(lowerer.lower_func(f));
        }
    }

    MirModule {
        package: hir.package.clone(),
        funcs: mir_funcs,
    }
}

struct MirLowerer<'a> {
    hir: &'a HirModule,
    func_map: &'a HashMap<String, &'a FuncDecl>,
    reg_widths: HashMap<String, usize>,
    local_ints: HashMap<String, i64>,
    current_purity: PurityMod,
}

impl<'a> MirLowerer<'a> {
    fn lower_func(&mut self, f: &FuncDecl) -> MirFunc {
        let mut params = Vec::new();
        for p in &f.params {
            let width = self.type_width(&p.ty);
            let is_q = p.ty.is_quantum();
            if is_q {
                self.reg_widths.insert(p.name.clone(), width);
            }
            params.push(MirParam {
                name: p.name.clone(),
                width,
                is_quantum: is_q,
            });
        }
        let body = self.lower_block(&f.body, &[]);
        MirFunc {
            name: f.name.clone(),
            params,
            purity: f.purity,
            body,
        }
    }

    fn type_width(&self, ty: &TypeExpr) -> usize {
        match ty {
            TypeExpr::Qubit => 1,
            TypeExpr::QReg(c) | TypeExpr::QInt(c) | TypeExpr::QFixed(c, _) => {
                c.resolve_static(&self.hir.consts).unwrap_or(1)
            }
            _ => 0,
        }
    }

    fn lower_block(&mut self, block: &Block, controls: &[QubitWire]) -> Vec<MirStmt> {
        let mut out = Vec::new();
        let mut idx = 0;
        let stmts = &block.stmts;

        while idx < stmts.len() {
            // Pass 3.1: Detect `defer adj CallExpr [-> target]` and wrap the compute + target's active scope
            // into an explicit `MirStmt::Conjugate { compute, action, target }` node!
            if let Stmt::DeferAdj { call, target, .. } = &stmts[idx] {
                let compute_stmts = self.lower_expr_call(call, &[]);
                // If the preceding statement in `out` was the forward `Compute(...)` call,
                // absorb it into `compute` so `compute` isn't duplicated!
                if !compute_stmts.is_empty() && out.ends_with(&compute_stmts) {
                    let new_len = out.len() - compute_stmts.len();
                    out.truncate(new_len);
                }

                // Determine the end of the `action` region:
                // If `target` is specified and referenced in subsequent statements of this block,
                // end the `action` region right after the last statement that references `target`
                // so `target` is uncomputed before unrelated operations mutate `compute`'s input registers.
                let action_end = if let Some(tname) = target {
                    stmts[idx + 1..]
                        .iter()
                        .rposition(|s| stmt_references_var(s, tname))
                        .map(|rel| idx + 1 + rel + 1)
                        .unwrap_or(stmts.len())
                } else {
                    stmts.len()
                };

                let action_block = Block {
                    stmts: stmts[idx + 1..action_end].to_vec(),
                    span: block.span,
                };
                let action_stmts = self.lower_block(&action_block, controls);

                let conj = MirStmt::Conjugate {
                    compute: compute_stmts,
                    action: action_stmts,
                    target: target.clone(),
                };
                if controls.is_empty() {
                    out.push(conj);
                } else {
                    out.push(apply_controls_to_stmt(conj, controls));
                }
                if let Some(t) = target {
                    let w = self.reg_widths.get(t).copied().unwrap_or(1);
                    out.push(MirStmt::QFree {
                        reg: t.clone(),
                        size: w,
                    });
                }
                idx = action_end;
                continue;
            }

            let lowered = self.lower_stmt(&stmts[idx], controls);
            out.extend(lowered);
            idx += 1;
        }

        out
    }

    fn lower_stmt(&mut self, stmt: &Stmt, controls: &[QubitWire]) -> Vec<MirStmt> {
        match stmt {
            Stmt::Decl { name, init, .. } => match init {
                Expr::QAlloc {
                    size, ty_override, ..
                } => {
                    let w = if let Some(t) = ty_override {
                        self.type_width(t)
                    } else if let Some(c) = size {
                        c.resolve_static(&self.hir.consts).unwrap_or(1)
                    } else {
                        1
                    };
                    self.reg_widths.insert(name.clone(), w);
                    let is_ancilla =
                        matches!(self.current_purity, PurityMod::Unitary | PurityMod::QFree);
                    vec![MirStmt::QAlloc {
                        reg: name.clone(),
                        size: w,
                        is_ancilla,
                    }]
                }
                Expr::Measure { arg, .. } => {
                    let wires = self.resolve_wires(arg);
                    if let Some(w) = wires.first() {
                        vec![MirStmt::Measure {
                            qubit: w.clone(),
                            dst_var: name.clone(),
                        }]
                    } else {
                        Vec::new()
                    }
                }
                Expr::Call { callee, args, .. } => {
                    // Functional quantum oracle form `flag := ComputeOracle(x)` where `ComputeOracle` has an implicit out-ancilla
                    let is_implicit_oracle = self
                        .func_map
                        .get(callee.as_str())
                        .map(|f| f.ret_ty.is_none() && f.params.len() == args.len() + 1)
                        .unwrap_or(false);
                    if is_implicit_oracle {
                        self.reg_widths.entry(name.clone()).or_insert(1);
                        let is_ancilla =
                            matches!(self.current_purity, PurityMod::Unitary | PurityMod::QFree);
                        let mut stmts = vec![MirStmt::QAlloc {
                            reg: name.clone(),
                            size: 1,
                            is_ancilla,
                        }];
                        stmts.extend(self.lower_expr_call_with_implicit_out(init, name, controls));
                        stmts
                    } else {
                        Vec::new()
                    }
                }
                other => {
                    if let Some(val) = self.eval_int(other) {
                        self.local_ints.insert(name.clone(), val);
                    }
                    Vec::new()
                }
            },
            Stmt::Assign { target, value, .. } => {
                if let Expr::Ident(tname, _) = target {
                    // Check if quantum in-place arithmetic `b = b + a` / `b += a` / `b -= a` / `b ^= a`
                    if let Some(&w) = self.reg_widths.get(tname) {
                        if let Expr::Binary { op, lhs, rhs, .. } = value {
                            let other_reg = match (lhs.as_ref(), rhs.as_ref()) {
                                (Expr::Ident(l, _), Expr::Ident(r, _)) if l == tname => Some(r.clone()),
                                (Expr::Ident(l, _), Expr::Ident(r, _)) if r == tname => Some(l.clone()),
                                _ => None,
                            };
                            if let Some(src_reg) = other_reg {
                                let qop = match op {
                                    BinOp::Add => Some(QuantumCmpKind::Add),
                                    BinOp::Sub => Some(QuantumCmpKind::Sub),
                                    BinOp::BitXor => Some(QuantumCmpKind::Xor),
                                    _ => None,
                                };
                                if let Some(kind) = qop {
                                    return vec![MirStmt::QuantumArith {
                                        op: kind,
                                        lhs_reg: src_reg,
                                        rhs_reg: tname.clone(),
                                        width: w,
                                    }];
                                }
                            }
                        }
                    }
                    if let Some(val) = self.eval_int(value) {
                        self.local_ints.insert(tname.clone(), val);
                    }
                }
                Vec::new()
            }
            Stmt::QuantumMod { kind, body, .. } => match kind {
                ModKind::Adj => {
                    let inner = self.lower_stmt(body, controls);
                    invert_stmts(&inner)
                }
                ModKind::Ctrl(ctrl_exprs) => {
                    let mut new_ctrls = controls.to_vec();
                    for ce in ctrl_exprs {
                        new_ctrls.extend(self.resolve_wires(ce));
                    }
                    let inner = self.lower_stmt(body, &[]);
                    inner
                        .into_iter()
                        .map(|s| apply_controls_to_stmt(s, &new_ctrls))
                        .collect()
                }
            },
            Stmt::If {
                cond,
                then_block,
                else_block,
                ..
            } => {
                if let Some((op, lhs_reg, rhs_reg, rhs_const, width)) =
                    self.extract_quantum_cond(cond)
                {
                    let then_body = self.lower_block(then_block, controls);
                    vec![MirStmt::QuantumIf {
                        op,
                        lhs_reg,
                        rhs_reg,
                        rhs_const,
                        width,
                        then_body,
                    }]
                } else {
                    if let Some(cval) = self.eval_int(cond) {
                        if cval != 0 {
                            return self.lower_block(then_block, controls);
                        } else if let Some(eb) = else_block {
                            return self.lower_block(eb, controls);
                        } else {
                            return Vec::new();
                        }
                    }
                    let cond_var = match cond {
                        Expr::Ident(s, _) => s.clone(),
                        _ => "cond".to_owned(),
                    };
                    let then_body = self.lower_block(then_block, controls);
                    let else_body = else_block
                        .as_ref()
                        .map(|b| self.lower_block(b, controls))
                        .unwrap_or_default();
                    vec![MirStmt::ClassicalIf {
                        cond_var,
                        then_body,
                        else_body,
                    }]
                }
            }
            Stmt::For {
                init,
                cond,
                post,
                body,
                ..
            } => {
                // Attempt compile-time loop unrolling if `init` and `cond` evaluate statically!
                if let (Some(init_stmt), Some(cond_expr)) = (init.as_deref(), cond.as_ref()) {
                    let _ = self.lower_stmt(init_stmt, controls);
                    if self.eval_int(cond_expr).is_some() {
                        let mut unrolled = Vec::new();
                        let mut guard = 0usize;
                        while self.eval_int(cond_expr).unwrap_or(0) != 0 && guard < 1024 {
                            unrolled.extend(self.lower_block(body, controls));
                            if let Some(post_stmt) = post.as_deref() {
                                let _ = self.lower_stmt(post_stmt, controls);
                            }
                            guard += 1;
                        }
                        return unrolled;
                    }
                }
                let cond_var = cond
                    .as_ref()
                    .and_then(|e| match e {
                        Expr::Ident(s, _) => Some(s.clone()),
                        _ => None,
                    })
                    .unwrap_or_else(|| "loop_cond".to_owned());
                let body_stmts = self.lower_block(body, controls);
                vec![MirStmt::While {
                    cond_var,
                    body: body_stmts,
                }]
            }
            Stmt::Block(b) => self.lower_block(b, controls),
            Stmt::Expr { expr, .. } => self.lower_expr_call(expr, controls),
            _ => Vec::new(),
        }
    }

    fn lower_expr_call_with_implicit_out(
        &mut self,
        expr: &Expr,
        out_reg: &str,
        controls: &[QubitWire],
    ) -> Vec<MirStmt> {
        if let Expr::Call { callee, args, span } = expr {
            let mut ext_args = args.clone();
            ext_args.push(Expr::Ident(out_reg.to_owned(), *span));
            let synth = Expr::Call {
                callee: callee.clone(),
                args: ext_args,
                span: *span,
            };
            self.lower_expr_call(&synth, controls)
        } else {
            self.lower_expr_call(expr, controls)
        }
    }

    fn lower_expr_call(&mut self, expr: &Expr, controls: &[QubitWire]) -> Vec<MirStmt> {
        match expr {
            Expr::QuantumMod { kind, expr: inner, .. } => match kind {
                ModKind::Adj => {
                    let s = self.lower_expr_call(inner, controls);
                    invert_stmts(&s)
                }
                ModKind::Ctrl(cs) => {
                    let mut new_ctrls = controls.to_vec();
                    for c in cs {
                        new_ctrls.extend(self.resolve_wires(c));
                    }
                    let inner_stmts = self.lower_expr_call(inner, &[]);
                    inner_stmts
                        .into_iter()
                        .map(|s| apply_controls_to_stmt(s, &new_ctrls))
                        .collect()
                }
            },
            Expr::Call { callee, args, .. } => {
                // Builtin 1-qubit gates (with slice broadcasting support `H(reg[:])`)
                let one_q_gate = match callee.as_str() {
                    "X" | "NOT" => Some(MirGateKind::X),
                    "Y" => Some(MirGateKind::Y),
                    "Z" => Some(MirGateKind::Z),
                    "H" => Some(MirGateKind::H),
                    "S" => Some(MirGateKind::S),
                    "Sdg" => Some(MirGateKind::Sdg),
                    "T" => Some(MirGateKind::T),
                    "Tdg" => Some(MirGateKind::Tdg),
                    _ => None,
                };
                if let Some(kind) = one_q_gate {
                    if let Some(arg) = args.first() {
                        let wires = self.resolve_wires(arg);
                        return wires
                            .into_iter()
                            .map(|w| MirStmt::Gate {
                                kind: kind.clone(),
                                controls: controls.to_vec(),
                                targets: vec![w],
                            })
                            .collect();
                    }
                }

                // Parameterized rotations Rx, Ry, Rz, Phase
                if matches!(callee.as_str(), "Rx" | "Ry" | "Rz" | "Phase" | "P") && args.len() == 2 {
                    let theta = self.eval_float(&args[0]);
                    let kind = match callee.as_str() {
                        "Rx" => MirGateKind::Rx(theta),
                        "Ry" => MirGateKind::Ry(theta),
                        "Rz" => MirGateKind::Rz(theta),
                        _ => MirGateKind::Phase(theta),
                    };
                    let wires = self.resolve_wires(&args[1]);
                    return wires
                        .into_iter()
                        .map(|w| MirStmt::Gate {
                            kind: kind.clone(),
                            controls: controls.to_vec(),
                            targets: vec![w],
                        })
                        .collect();
                }

                // Controlled phase CP(theta, c, t)
                if matches!(callee.as_str(), "CP" | "CRz") && args.len() == 3 {
                    let theta = self.eval_float(&args[0]);
                    let mut ctrls = controls.to_vec();
                    ctrls.extend(self.resolve_wires(&args[1]));
                    let targets = self.resolve_wires(&args[2]);
                    let kind = if callee == "CRz" {
                        MirGateKind::Rz(theta)
                    } else {
                        MirGateKind::Phase(theta)
                    };
                    return targets
                        .into_iter()
                        .map(|t| MirStmt::Gate {
                            kind: kind.clone(),
                            controls: ctrls.clone(),
                            targets: vec![t],
                        })
                        .collect();
                }

                // CX / CNOT
                if matches!(callee.as_str(), "CX" | "CNOT") && args.len() == 2 {
                    let c_wires = self.resolve_wires(&args[0]);
                    let t_wires = self.resolve_wires(&args[1]);
                    let mut stmts = Vec::new();
                    for (c, t) in c_wires.into_iter().zip(t_wires.into_iter()) {
                        let mut ctrls = controls.to_vec();
                        ctrls.push(c);
                        stmts.push(MirStmt::Gate {
                            kind: MirGateKind::X,
                            controls: ctrls,
                            targets: vec![t],
                        });
                    }
                    return stmts;
                }

                // CCX / Toffoli
                if matches!(callee.as_str(), "CCX" | "Toffoli") && args.len() == 3 {
                    let c0 = self.resolve_wires(&args[0]);
                    let c1 = self.resolve_wires(&args[1]);
                    let t = self.resolve_wires(&args[2]);
                    if let (Some(w0), Some(w1), Some(wt)) = (c0.first(), c1.first(), t.first()) {
                        let mut ctrls = controls.to_vec();
                        ctrls.push(w0.clone());
                        ctrls.push(w1.clone());
                        return vec![MirStmt::Gate {
                            kind: MirGateKind::X,
                            controls: ctrls,
                            targets: vec![wt.clone()],
                        }];
                    }
                }

                // Swap
                if matches!(callee.as_str(), "Swap" | "SWAP") && args.len() == 2 {
                    let w0 = self.resolve_wires(&args[0]);
                    let w1 = self.resolve_wires(&args[1]);
                    let mut stmts = Vec::new();
                    for (a, b) in w0.into_iter().zip(w1.into_iter()) {
                        stmts.push(MirStmt::Gate {
                            kind: MirGateKind::Swap,
                            controls: controls.to_vec(),
                            targets: vec![a, b],
                        });
                    }
                    return stmts;
                }

                // CuccaroAdd / CuccaroSub / DraperAdd / DraperSub intrinsic arithmetic calls
                if matches!(
                    callee.as_str(),
                    "CuccaroAdd" | "DraperAdd" | "CuccaroSub" | "DraperSub"
                ) && args.len() == 2
                {
                    if let (Expr::Ident(a, _), Expr::Ident(b, _)) = (&args[0], &args[1]) {
                        let w = self.reg_widths.get(a).copied().unwrap_or(4);
                        let op = if callee.ends_with("Sub") {
                            QuantumCmpKind::Sub
                        } else {
                            QuantumCmpKind::Add
                        };
                        return vec![MirStmt::QuantumArith {
                            op,
                            lhs_reg: a.clone(),
                            rhs_reg: b.clone(),
                            width: w,
                        }];
                    }
                }

                // reset(q)
                if callee == "reset" && args.len() == 1 {
                    let wires = self.resolve_wires(&args[0]);
                    return wires
                        .into_iter()
                        .map(|qubit| MirStmt::Reset { qubit })
                        .collect();
                }

                // User-defined function inline expansion with argument wire substitution
                if let Some(target_func) = self.func_map.get(callee).copied() {
                    let mut wire_subst: HashMap<String, Vec<QubitWire>> = HashMap::new();
                    for (param, arg_expr) in target_func.params.iter().zip(args.iter()) {
                        let actual_wires = self.resolve_wires(arg_expr);
                        if !actual_wires.is_empty() {
                            self.reg_widths
                                .insert(param.name.clone(), actual_wires.len());
                            wire_subst.insert(param.name.clone(), actual_wires);
                        }
                    }
                    let raw_body = self.lower_block(&target_func.body, controls);
                    return substitute_wires(raw_body, &wire_subst);
                }

                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn resolve_wires(&self, expr: &Expr) -> Vec<QubitWire> {
        match expr {
            Expr::Ident(name, _) => {
                let w = self.reg_widths.get(name).copied().unwrap_or(1);
                (0..w).map(|i| QubitWire::new(name.clone(), i)).collect()
            }
            Expr::Index { base, index, .. } => {
                if let Expr::Ident(name, _) = base.as_ref() {
                    let idx = self.eval_int(index).unwrap_or(0).max(0) as usize;
                    vec![QubitWire::new(name.clone(), idx)]
                } else {
                    Vec::new()
                }
            }
            Expr::Slice {
                base, start, end, ..
            } => {
                if let Expr::Ident(name, _) = base.as_ref() {
                    let total = self.reg_widths.get(name).copied().unwrap_or(1);
                    let s = start
                        .as_deref()
                        .and_then(|e| self.eval_int(e))
                        .map(|n| n.max(0) as usize)
                        .unwrap_or(0);
                    let e = end
                        .as_deref()
                        .and_then(|e| self.eval_int(e))
                        .map(|n| n.max(0) as usize)
                        .unwrap_or(total);
                    (s..e.min(total))
                        .map(|i| QubitWire::new(name.clone(), i))
                        .collect()
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
        }
    }

    fn eval_int(&self, expr: &Expr) -> Option<i64> {
        match expr {
            Expr::IntLit(n, _) => Some(*n),
            Expr::BoolLit(b, _) => Some(if *b { 1 } else { 0 }),
            Expr::Ident(name, _) => self
                .local_ints
                .get(name)
                .copied()
                .or_else(|| self.hir.consts.get(name).map(|&v| v as i64)),
            Expr::Unary { op, expr: inner, .. } => {
                let v = self.eval_int(inner)?;
                match op {
                    qgo_syntax::UnOp::Neg => Some(-v),
                    qgo_syntax::UnOp::Not => Some(if v == 0 { 1 } else { 0 }),
                }
            }
            Expr::Binary { op, lhs, rhs, .. } => {
                let l = self.eval_int(lhs)?;
                let r = self.eval_int(rhs)?;
                match op {
                    BinOp::Add => Some(l.wrapping_add(r)),
                    BinOp::Sub => Some(l.wrapping_sub(r)),
                    BinOp::Mul => Some(l.wrapping_mul(r)),
                    BinOp::Div => Some(if r != 0 { l / r } else { 0 }),
                    BinOp::Rem => Some(if r != 0 { l % r } else { 0 }),
                    BinOp::BitXor => Some(l ^ r),
                    BinOp::BitAnd => Some(l & r),
                    BinOp::BitOr => Some(l | r),
                    BinOp::Shl => Some(l.wrapping_shl(r as u32)),
                    BinOp::Shr => Some(l.wrapping_shr(r as u32)),
                    BinOp::Eq => Some((l == r) as i64),
                    BinOp::NotEq => Some((l != r) as i64),
                    BinOp::Lt => Some((l < r) as i64),
                    BinOp::LtEq => Some((l <= r) as i64),
                    BinOp::Gt => Some((l > r) as i64),
                    BinOp::GtEq => Some((l >= r) as i64),
                    BinOp::And => Some(((l != 0) && (r != 0)) as i64),
                    BinOp::Or => Some(((l != 0) || (r != 0)) as i64),
                }
            }
            _ => None,
        }
    }

    fn eval_float(&self, expr: &Expr) -> f64 {
        if let Some(iv) = self.eval_int(expr) {
            return iv as f64;
        }
        match expr {
            Expr::FloatLit(s, _) => s.parse::<f64>().unwrap_or(0.0),
            Expr::IntLit(n, _) => *n as f64,
            Expr::Unary { expr: inner, .. } => -self.eval_float(inner),
            Expr::Binary { op, lhs, rhs, .. } => {
                let l = self.eval_float(lhs);
                let r = self.eval_float(rhs);
                match op {
                    BinOp::Add => l + r,
                    BinOp::Sub => l - r,
                    BinOp::Mul => l * r,
                    BinOp::Div => {
                        if r.abs() > 1e-15 {
                            l / r
                        } else {
                            0.0
                        }
                    }
                    _ => 0.0,
                }
            }
            _ => std::f64::consts::FRAC_PI_4,
        }
    }

    fn extract_quantum_cond(
        &self,
        cond: &Expr,
    ) -> Option<(QuantumCmpKind, String, Option<String>, Option<i64>, usize)> {
        if let Expr::Binary { op, lhs, rhs, .. } = cond {
            let cmp_op = match op {
                BinOp::Eq => QuantumCmpKind::Eq,
                BinOp::Lt => QuantumCmpKind::Lt,
                BinOp::Gt => QuantumCmpKind::Gt,
                BinOp::Add => QuantumCmpKind::Add,
                BinOp::Sub => QuantumCmpKind::Sub,
                BinOp::BitXor => QuantumCmpKind::Xor,
                _ => return None,
            };
            // Support `x > 5`, `x == y`, or `x + y > 5`
            let (lhs_reg, add_reg) = match lhs.as_ref() {
                Expr::Ident(name, _) if self.reg_widths.contains_key(name) => (name.clone(), None),
                Expr::Binary {
                    op: BinOp::Add,
                    lhs: inner_l,
                    rhs: inner_r,
                    ..
                } => {
                    if let (Expr::Ident(lname, _), Expr::Ident(rname, _)) =
                        (inner_l.as_ref(), inner_r.as_ref())
                    {
                        if self.reg_widths.contains_key(lname) {
                            (lname.clone(), Some(rname.clone()))
                        } else {
                            return None;
                        }
                    } else {
                        return None;
                    }
                }
                _ => return None,
            };
            let width = self.reg_widths.get(&lhs_reg).copied().unwrap_or(4);
            if let Some(cval) = self.eval_int(rhs) {
                Some((cmp_op, lhs_reg, add_reg, Some(cval), width))
            } else if let Expr::Ident(rname, _) = rhs.as_ref() {
                Some((cmp_op, lhs_reg, Some(rname.clone()), None, width))
            } else {
                Some((cmp_op, lhs_reg, add_reg, Some(0), width))
            }
        } else {
            None
        }
    }
}

/// Pass 3.3: Structural Adjoint Inversion (`adj Block`).
/// Reverses topological statement order and replaces every gate $G(\theta)$ with $G^\dagger(-\theta)$.
/// Preserves `Conjugate` self-symmetry: $\text{adj}(U V U^\dagger) = U V^\dagger U^\dagger$.
pub fn invert_stmts(stmts: &[MirStmt]) -> Vec<MirStmt> {
    let mut rev = Vec::with_capacity(stmts.len());
    for stmt in stmts.iter().rev() {
        match stmt {
            MirStmt::Gate {
                kind,
                controls,
                targets,
            } => {
                rev.push(MirStmt::Gate {
                    kind: kind.adjoint(),
                    controls: controls.clone(),
                    targets: targets.clone(),
                });
            }
            MirStmt::Conjugate {
                compute,
                action,
                target,
            } => {
                // Mathematical identity: (U V U^\dagger)^\dagger = U V^\dagger U^\dagger
                rev.push(MirStmt::Conjugate {
                    compute: compute.clone(),
                    action: invert_stmts(action),
                    target: target.clone(),
                });
            }
            MirStmt::QAlloc { reg, size, .. } => {
                rev.push(MirStmt::QFree {
                    reg: reg.clone(),
                    size: *size,
                });
            }
            MirStmt::QFree { reg, size } => {
                rev.push(MirStmt::QAlloc {
                    reg: reg.clone(),
                    size: *size,
                    is_ancilla: true,
                });
            }
            MirStmt::QuantumArith {
                op,
                lhs_reg,
                rhs_reg,
                width,
            } => {
                let inv_op = match op {
                    QuantumCmpKind::Add => QuantumCmpKind::Sub,
                    QuantumCmpKind::Sub => QuantumCmpKind::Add,
                    other => other.clone(),
                };
                rev.push(MirStmt::QuantumArith {
                    op: inv_op,
                    lhs_reg: lhs_reg.clone(),
                    rhs_reg: rhs_reg.clone(),
                    width: *width,
                });
            }
            other => rev.push(other.clone()),
        }
    }
    rev
}

/// Applies control wires to a MIR statement (including Pass 3.2 Ctrl-Conjugation theorem support).
pub fn apply_controls_to_stmt(stmt: MirStmt, extra_ctrls: &[QubitWire]) -> MirStmt {
    if extra_ctrls.is_empty() {
        return stmt;
    }
    match stmt {
        MirStmt::Gate {
            kind,
            mut controls,
            targets,
        } => {
            for c in extra_ctrls {
                if !controls.contains(c) {
                    controls.push(c.clone());
                }
            }
            MirStmt::Gate {
                kind,
                controls,
                targets,
            }
        }
        MirStmt::Conjugate {
            compute,
            action,
            target,
        } => {
            // Pass 3.2: Ctrl-Conjugation Theorem!
            // ctrl_c(U * V * U_dag) = U * (ctrl_c V) * U_dag
            // Notice `compute` (U) is left UNCONTROLLED!
            let controlled_action = action
                .into_iter()
                .map(|s| apply_controls_to_stmt(s, extra_ctrls))
                .collect();
            MirStmt::Conjugate {
                compute,
                action: controlled_action,
                target,
            }
        }
        other => other,
    }
}

fn substitute_wires(
    stmts: Vec<MirStmt>,
    subst: &HashMap<String, Vec<QubitWire>>,
) -> Vec<MirStmt> {
    let map_wire = |w: &QubitWire| -> QubitWire {
        if let Some(mapped) = subst.get(&w.reg) {
            if let Some(actual) = mapped.get(w.index) {
                return actual.clone();
            }
        }
        w.clone()
    };

    stmts
        .into_iter()
        .map(|stmt| match stmt {
            MirStmt::Gate {
                kind,
                controls,
                targets,
            } => MirStmt::Gate {
                kind,
                controls: controls.iter().map(map_wire).collect(),
                targets: targets.iter().map(map_wire).collect(),
            },
            MirStmt::Conjugate {
                compute,
                action,
                target,
            } => MirStmt::Conjugate {
                compute: substitute_wires(compute, subst),
                action: substitute_wires(action, subst),
                target,
            },
            MirStmt::QuantumArith {
                op,
                lhs_reg,
                rhs_reg,
                width,
            } => {
                let new_lhs = subst
                    .get(&lhs_reg)
                    .and_then(|v| v.first())
                    .map(|w| w.reg.clone())
                    .unwrap_or(lhs_reg);
                let new_rhs = subst
                    .get(&rhs_reg)
                    .and_then(|v| v.first())
                    .map(|w| w.reg.clone())
                    .unwrap_or(rhs_reg);
                MirStmt::QuantumArith {
                    op,
                    lhs_reg: new_lhs,
                    rhs_reg: new_rhs,
                    width,
                }
            }
            MirStmt::Measure { qubit, dst_var } => MirStmt::Measure {
                qubit: map_wire(&qubit),
                dst_var,
            },
            MirStmt::Reset { qubit } => MirStmt::Reset {
                qubit: map_wire(&qubit),
            },
            other => other,
        })
        .collect()
}

fn stmt_references_var(stmt: &Stmt, var: &str) -> bool {
    match stmt {
        Stmt::Decl { name, init, .. } => name == var || expr_references_var(init, var),
        Stmt::Assign { target, value, .. } => {
            expr_references_var(target, var) || expr_references_var(value, var)
        }
        Stmt::DeferAdj { call, target, .. } => {
            target.as_deref() == Some(var) || expr_references_var(call, var)
        }
        Stmt::QuantumMod { kind, body, .. } => {
            let ctrl_ref = match kind {
                ModKind::Adj => false,
                ModKind::Ctrl(cs) => cs.iter().any(|e| expr_references_var(e, var)),
            };
            ctrl_ref || stmt_references_var(body, var)
        }
        Stmt::If {
            cond,
            then_block,
            else_block,
            ..
        } => {
            expr_references_var(cond, var)
                || then_block.stmts.iter().any(|s| stmt_references_var(s, var))
                || else_block
                    .as_ref()
                    .map(|b| b.stmts.iter().any(|s| stmt_references_var(s, var)))
                    .unwrap_or(false)
        }
        Stmt::For {
            init,
            cond,
            post,
            body,
            ..
        } => {
            init.as_ref().map(|s| stmt_references_var(s, var)).unwrap_or(false)
                || cond.as_ref().map(|e| expr_references_var(e, var)).unwrap_or(false)
                || post.as_ref().map(|s| stmt_references_var(s, var)).unwrap_or(false)
                || body.stmts.iter().any(|s| stmt_references_var(s, var))
        }
        Stmt::Return { value, .. } => value.as_ref().map(|e| expr_references_var(e, var)).unwrap_or(false),
        Stmt::Break(_) | Stmt::Continue(_) => false,
        Stmt::Block(b) => b.stmts.iter().any(|s| stmt_references_var(s, var)),
        Stmt::Expr { expr, .. } => expr_references_var(expr, var),
    }
}

fn expr_references_var(expr: &Expr, var: &str) -> bool {
    match expr {
        Expr::Ident(name, _) => name == var,
        Expr::Measure { arg, .. } => expr_references_var(arg, var),
        Expr::Call { callee, args, .. } => {
            if matches!(
                callee.as_str(),
                "prob_zero" | "prob_basis" | "peek_uint" | "assert" | "assert_eq"
            ) {
                false
            } else {
                args.iter().any(|a| expr_references_var(a, var))
            }
        }
        Expr::Index { base, index, .. } => {
            expr_references_var(base, var) || expr_references_var(index, var)
        }
        Expr::Slice {
            base, start, end, ..
        } => {
            expr_references_var(base, var)
                || start.as_ref().map(|e| expr_references_var(e, var)).unwrap_or(false)
                || end.as_ref().map(|e| expr_references_var(e, var)).unwrap_or(false)
        }
        Expr::Binary { lhs, rhs, .. } => {
            expr_references_var(lhs, var) || expr_references_var(rhs, var)
        }
        Expr::Unary { expr: inner, .. } => expr_references_var(inner, var),
        Expr::QuantumMod { kind, expr: inner, .. } => {
            let ctrl_ref = match kind {
                ModKind::Adj => false,
                ModKind::Ctrl(cs) => cs.iter().any(|e| expr_references_var(e, var)),
            };
            ctrl_ref || expr_references_var(inner, var)
        }
        Expr::FieldAccess { base, .. } => expr_references_var(base, var),
        Expr::StructLit { fields, .. } => fields.iter().any(|(_, e)| expr_references_var(e, var)),
        Expr::SliceLit { elems, .. } => elems.iter().any(|e| expr_references_var(e, var)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qgo_hir::check_module;
    use qgo_span::FileId;
    use qgo_syntax::parse_source;

    #[test]
    fn test_mir_loop_unrolling_and_conjugate_synthesis() {
        let src = r#"package main
const N = 4

func HadamardLadder(reg qreg[N]) unitary {
    for i := 0; i < N; i++ {
        H(reg[i])
    }
    for j := 0; j < N - 1; j++ {
        CX(reg[j], reg[j + 1])
    }
}
"#;
        let (ast, pdiags) = parse_source(FileId(0), src);
        assert!(pdiags.is_empty());
        let (hir, hdiags) = check_module(&ast);
        assert!(hdiags.is_empty());
        let mir = lower_to_mir(&hir);
        let func = &mir.funcs[0];
        // 4 H gates + 3 CX gates = 7 Gate statements
        assert_eq!(func.body.len(), 7);
    }
}
