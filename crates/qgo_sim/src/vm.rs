//! `QGoVM`: Hybrid Classical-Quantum Virtual Machine & Interpreter.
//! Executes full QGo programs (classical structs, slices, strings, loops, recursion, file I/O)
//! tightly coupled with the live `StatevectorSim` quantum statevector engine.

use std::collections::HashMap;
use std::fs;
use qgo_mir::{
    apply_controls_to_stmt, invert_stmts, MirGateKind, MirStmt, QubitWire,
};
use qgo_opt::{
    flatten_conjugates, synthesize_cuccaro_adder, synthesize_draper_qft_adder, SynthMode,
};
use qgo_syntax::{
    BinOp, Block, ConstExpr, Expr, FuncDecl, ModKind, SourceFile, Stmt, TopLevelDecl, TypeExpr,
    UnOp,
};
use crate::StatevectorSim;

/// Runtime value in the Hybrid Classical-Quantum Virtual Machine (`QGoVM`).
#[derive(Debug, Clone, PartialEq)]
pub enum VmVal {
    Void,
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    Slice(Vec<VmVal>),
    Struct(String, HashMap<String, VmVal>),
    QReg {
        reg: String,
        offset: usize,
        width: usize,
    },
}

impl VmVal {
    pub fn as_int(&self) -> i64 {
        match self {
            VmVal::Int(n) => *n,
            VmVal::Bool(b) => *b as i64,
            VmVal::Float(f) => *f as i64,
            _ => 0,
        }
    }

    pub fn as_float(&self) -> f64 {
        match self {
            VmVal::Float(f) => *f,
            VmVal::Int(n) => *n as f64,
            _ => 0.0,
        }
    }

    pub fn as_bool(&self) -> bool {
        match self {
            VmVal::Bool(b) => *b,
            VmVal::Int(n) => *n != 0,
            VmVal::Str(s) => !s.is_empty(),
            _ => false,
        }
    }

    pub fn as_str(&self) -> String {
        match self {
            VmVal::Str(s) => s.clone(),
            VmVal::Int(n) => n.to_string(),
            VmVal::Float(f) => format!("{}", f),
            VmVal::Bool(b) => b.to_string(),
            VmVal::Void => "void".to_owned(),
            VmVal::Slice(items) => {
                let parts: Vec<String> = items.iter().map(|v| v.as_str()).collect();
                format!("[{}]", parts.join(", "))
            }
            VmVal::Struct(name, fields) => {
                let mut pairs: Vec<_> = fields.iter().collect();
                pairs.sort_by_key(|(k, _)| *k);
                let s: Vec<String> = pairs
                    .into_iter()
                    .map(|(k, v)| format!("{}: {}", k, v.as_str()))
                    .collect();
                format!("{}{{ {} }}", name, s.join(", "))
            }
            VmVal::QReg { reg, offset, width } => {
                format!("{}[{}..{}]", reg, offset, offset + width)
            }
        }
    }

    pub fn wires(&self) -> Vec<QubitWire> {
        match self {
            VmVal::QReg { reg, offset, width } => (*offset..(*offset + *width))
                .map(|i| QubitWire::new(reg.clone(), i))
                .collect(),
            _ => Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
enum FlowSignal {
    None,
    Return(VmVal),
    Break,
    Continue,
}

/// Hybrid Classical-Quantum Virtual Machine executing `.qg` AST modules with a live `StatevectorSim`.
pub struct QGoVM {
    pub consts: HashMap<String, VmVal>,
    pub const_uints: HashMap<String, usize>,
    pub structs: HashMap<String, Vec<(String, TypeExpr)>>,
    pub funcs: HashMap<String, FuncDecl>,
    pub sim: StatevectorSim,
    pub synth_mode: SynthMode,
    pub stdout: String,
    pub cli_args: Vec<String>,
    next_qreg_id: usize,
    /// When `Some`, quantum gates are recorded into this buffer (for `adj` / `ctrl` transform)
    /// before being committed to `self.sim`.
    recording_stack: Vec<Vec<MirStmt>>,
}

impl QGoVM {
    pub fn new(ast: &SourceFile, synth_mode: SynthMode) -> Self {
        let mut vm = Self {
            consts: HashMap::new(),
            const_uints: HashMap::new(),
            structs: HashMap::new(),
            funcs: HashMap::new(),
            sim: StatevectorSim::new(),
            synth_mode,
            stdout: String::new(),
            cli_args: Vec::new(),
            next_qreg_id: 0,
            recording_stack: Vec::new(),
        };
        vm.load_module(ast);
        vm
    }

    pub fn with_args(mut self, args: Vec<String>) -> Self {
        self.cli_args = args;
        self
    }

    pub fn load_module(&mut self, ast: &SourceFile) {
        for item in &ast.items {
            match item {
                TopLevelDecl::Const(c) => {
                    let mut dummy_vars = HashMap::new();
                    let val = self.eval_expr(&c.value, &mut dummy_vars).unwrap_or(VmVal::Int(0));
                    if let VmVal::Int(n) = val {
                        self.const_uints.insert(c.name.clone(), n.max(0) as usize);
                    }
                    self.consts.insert(c.name.clone(), val);
                }
                TopLevelDecl::Struct(s) => {
                    let fields = s
                        .fields
                        .iter()
                        .map(|f| (f.name.clone(), f.ty.clone()))
                        .collect();
                    self.structs.insert(s.name.clone(), fields);
                }
                TopLevelDecl::Func(f) => {
                    self.funcs.insert(f.name.clone(), f.clone());
                }
            }
        }
    }

    fn default_val_for_type(&self, ty: &TypeExpr) -> VmVal {
        match ty {
            TypeExpr::Bool => VmVal::Bool(false),
            TypeExpr::Int | TypeExpr::Int64 | TypeExpr::Uint => VmVal::Int(0),
            TypeExpr::Float64 => VmVal::Float(0.0),
            TypeExpr::StringTy => VmVal::Str(String::new()),
            TypeExpr::Slice(_) => VmVal::Slice(Vec::new()),
            TypeExpr::Array(c, inner) => {
                let n = c.resolve_static(&self.const_uints).unwrap_or(0);
                let elem = self.default_val_for_type(inner);
                VmVal::Slice(vec![elem; n])
            }
            TypeExpr::Named(sname) => {
                let mut map = HashMap::new();
                if let Some(fields) = self.structs.get(sname) {
                    for (fname, fty) in fields {
                        map.insert(fname.clone(), self.default_val_for_type(fty));
                    }
                }
                VmVal::Struct(sname.clone(), map)
            }
            _ => VmVal::Void,
        }
    }

    fn alloc_quantum_reg(&mut self, prefix: &str, width: usize, is_ancilla: bool) -> VmVal {
        let reg_name = format!("{}_{}", prefix, self.next_qreg_id);
        self.next_qreg_id += 1;
        let w = width.max(1);
        for b in 0..w {
            self.sim
                .allocate_wire(QubitWire::new(reg_name.clone(), b), is_ancilla);
        }
        VmVal::QReg {
            reg: reg_name,
            offset: 0,
            width: w,
        }
    }

    fn emit_or_exec_mir(&mut self, stmts: Vec<MirStmt>) {
        if let Some(buf) = self.recording_stack.last_mut() {
            buf.extend(stmts);
        } else {
            let flat = flatten_conjugates(&stmts);
            self.sim.execute_stmts(&flat);
        }
    }

    /// Executes `main()` if defined in the loaded module.
    pub fn run_main(&mut self) -> Result<VmVal, String> {
        self.call_function("main", Vec::new())
    }

    /// Calls a function by name with evaluated arguments.
    /// If the function has quantum parameters and `args` is empty (e.g. during `qgo test`),
    /// zero-initialized quantum registers are automatically allocated for its parameters.
    pub fn call_function(&mut self, name: &str, args: Vec<VmVal>) -> Result<VmVal, String> {
        let Some(func) = self.funcs.get(name).cloned() else {
            return Err(format!("undefined function `{}`", name));
        };

        let mut vars: HashMap<String, VmVal> = HashMap::new();
        for (idx, param) in func.params.iter().enumerate() {
            let val = if let Some(v) = args.get(idx) {
                v.clone()
            } else if param.ty.is_quantum() {
                let w = self.type_qubit_width(&param.ty);
                self.alloc_quantum_reg(&param.name, w, false)
            } else {
                self.default_val_for_type(&param.ty)
            };
            vars.insert(param.name.clone(), val);
        }

        let sig = self.exec_block(&func.body, &mut vars)?;
        match sig {
            FlowSignal::Return(v) => Ok(v),
            _ => Ok(VmVal::Void),
        }
    }

    fn type_qubit_width(&self, ty: &TypeExpr) -> usize {
        match ty {
            TypeExpr::Qubit => 1,
            TypeExpr::QReg(c) | TypeExpr::QInt(c) | TypeExpr::QFixed(c, _) => {
                c.resolve_static(&self.const_uints).unwrap_or(1)
            }
            _ => 1,
        }
    }

    fn exec_block(
        &mut self,
        block: &Block,
        vars: &mut HashMap<String, VmVal>,
    ) -> Result<FlowSignal, String> {
        let stmts = &block.stmts;
        let mut idx = 0;

        while idx < stmts.len() {
            if let Stmt::DeferAdj { call, target, .. } = &stmts[idx] {
                // Determine live range of `target` in remaining statements of this block
                let action_end = if let Some(tname) = target {
                    stmts[idx + 1..]
                        .iter()
                        .rposition(|s| stmt_touches_ident(s, tname))
                        .map(|rel| idx + 1 + rel + 1)
                        .unwrap_or(stmts.len())
                } else {
                    stmts.len()
                };

                let action_block = Block {
                    stmts: stmts[idx + 1..action_end].to_vec(),
                    span: block.span,
                };
                let sig = self.exec_block(&action_block, vars)?;

                // Execute the deferred adjoint `adj call`
                let adj_expr = Expr::QuantumMod {
                    kind: ModKind::Adj,
                    expr: Box::new(call.clone()),
                    span: call.span(),
                };
                let _ = self.eval_expr(&adj_expr, vars)?;

                // Verify ancilla target returned to |0> within 1e-12
                if let Some(tname) = target {
                    if let Some(tval) = vars.get(tname) {
                        for w in tval.wires() {
                            let p0 = self.sim.prob_zero(&w);
                            if (1.0 - p0).abs() >= 1e-12 {
                                return Err(format!(
                                    "Ancilla `{}` ({}) not uncomputed to |0> after `defer adj`: P(|0>) = {:.15}",
                                    tname, w, p0
                                ));
                            }
                        }
                    }
                }

                if !matches!(sig, FlowSignal::None) {
                    return Ok(sig);
                }
                idx = action_end;
                continue;
            }

            let sig = self.exec_stmt(&stmts[idx], vars)?;
            if !matches!(sig, FlowSignal::None) {
                return Ok(sig);
            }
            idx += 1;
        }

        Ok(FlowSignal::None)
    }

    fn exec_stmt(
        &mut self,
        stmt: &Stmt,
        vars: &mut HashMap<String, VmVal>,
    ) -> Result<FlowSignal, String> {
        match stmt {
            Stmt::Decl { name, init, .. } => {
                // Handle functional quantum oracle form `flag := ComputeOracle(x)` where `ComputeOracle` returns void
                if let Expr::Call { callee, args, span } = init {
                    if let Some(f) = self.funcs.get(callee) {
                        if f.ret_ty.is_none() && f.params.len() == args.len() + 1 {
                            let anc = self.alloc_quantum_reg(name, 1, true);
                            vars.insert(name.clone(), anc);
                            let mut ext_args = args.clone();
                            ext_args.push(Expr::Ident(name.clone(), *span));
                            let synth_call = Expr::Call {
                                callee: callee.clone(),
                                args: ext_args,
                                span: *span,
                            };
                            let _ = self.eval_expr(&synth_call, vars)?;
                            return Ok(FlowSignal::None);
                        }
                    }
                }
                let val = self.eval_expr(init, vars)?;
                vars.insert(name.clone(), val);
                Ok(FlowSignal::None)
            }
            Stmt::Assign { target, value, .. } => {
                // Check for quantum in-place arithmetic `b += a`, `b -= a`, `b ^= a`
                if let Expr::Ident(tname, _) = target {
                    if let Some(VmVal::QReg { .. }) = vars.get(tname) {
                        if let Expr::Binary { op, lhs, rhs, .. } = value {
                            let other_expr = match (lhs.as_ref(), rhs.as_ref()) {
                                (Expr::Ident(l, _), other) if l == tname => Some(other),
                                (other, Expr::Ident(r, _)) if r == tname => Some(other),
                                _ => None,
                            };
                            if let Some(other) = other_expr {
                                let rhs_q = vars.get(tname).cloned().unwrap();
                                let lhs_q = self.eval_expr(other, vars)?;
                                self.exec_quantum_inplace_arith(*op, &lhs_q, &rhs_q);
                                return Ok(FlowSignal::None);
                            }
                        }
                    }
                }
                let rhs_val = self.eval_expr(value, vars)?;
                self.assign_lvalue(target, rhs_val, vars)?;
                Ok(FlowSignal::None)
            }
            Stmt::DeferAdj { .. } => Ok(FlowSignal::None),
            Stmt::QuantumMod { kind, body, .. } => {
                self.recording_stack.push(Vec::new());
                let res = self.exec_stmt(body, vars);
                let recorded = self.recording_stack.pop().unwrap_or_default();
                let res_check = res?;
                let transformed = match kind {
                    ModKind::Adj => invert_stmts(&recorded),
                    ModKind::Ctrl(ctrl_exprs) => {
                        let mut ctrls = Vec::new();
                        for ce in ctrl_exprs {
                            let cv = self.eval_expr(ce, vars)?;
                            ctrls.extend(cv.wires());
                        }
                        recorded
                            .into_iter()
                            .map(|s| apply_controls_to_stmt(s, &ctrls))
                            .collect()
                    }
                };
                self.emit_or_exec_mir(transformed);
                Ok(res_check)
            }
            Stmt::If {
                cond,
                then_block,
                else_block,
                ..
            } => {
                // Check if `cond` is a quantum conditional expression (referencing a QReg variable)
                if self.expr_is_quantum(cond, vars) {
                    self.exec_quantum_if(cond, then_block, vars)?;
                    return Ok(FlowSignal::None);
                }
                let cval = self.eval_expr(cond, vars)?;
                if cval.as_bool() {
                    self.exec_block(then_block, vars)
                } else if let Some(eb) = else_block {
                    self.exec_block(eb, vars)
                } else {
                    Ok(FlowSignal::None)
                }
            }
            Stmt::For {
                init,
                cond,
                post,
                body,
                ..
            } => {
                if let Some(init_s) = init {
                    let _ = self.exec_stmt(init_s, vars)?;
                }
                loop {
                    if let Some(cond_e) = cond {
                        let cv = self.eval_expr(cond_e, vars)?;
                        if !cv.as_bool() {
                            break;
                        }
                    }
                    let sig = self.exec_block(body, vars)?;
                    match sig {
                        FlowSignal::None => {}
                        FlowSignal::Continue => {}
                        FlowSignal::Break => break,
                        FlowSignal::Return(v) => return Ok(FlowSignal::Return(v)),
                    }
                    if let Some(post_s) = post {
                        let _ = self.exec_stmt(post_s, vars)?;
                    }
                }
                Ok(FlowSignal::None)
            }
            Stmt::Return { value, .. } => {
                let v = if let Some(e) = value {
                    self.eval_expr(e, vars)?
                } else {
                    VmVal::Void
                };
                Ok(FlowSignal::Return(v))
            }
            Stmt::Break(_) => Ok(FlowSignal::Break),
            Stmt::Continue(_) => Ok(FlowSignal::Continue),
            Stmt::Block(b) => self.exec_block(b, vars),
            Stmt::Expr { expr, .. } => {
                let _ = self.eval_expr(expr, vars)?;
                Ok(FlowSignal::None)
            }
        }
    }

    fn assign_lvalue(
        &mut self,
        target: &Expr,
        new_val: VmVal,
        vars: &mut HashMap<String, VmVal>,
    ) -> Result<(), String> {
        match target {
            Expr::Ident(name, _) => {
                vars.insert(name.clone(), new_val);
                Ok(())
            }
            Expr::FieldAccess { base, field, .. } => {
                let mut base_val = self.eval_expr(base, vars)?;
                if let VmVal::Struct(_, ref mut fields) = base_val {
                    fields.insert(field.clone(), new_val);
                }
                self.assign_lvalue(base, base_val, vars)
            }
            Expr::Index { base, index, .. } => {
                let idx = self.eval_expr(index, vars)?.as_int().max(0) as usize;
                let mut base_val = self.eval_expr(base, vars)?;
                if let VmVal::Slice(ref mut elems) = base_val {
                    if idx < elems.len() {
                        elems[idx] = new_val;
                    }
                }
                self.assign_lvalue(base, base_val, vars)
            }
            _ => Err("invalid assignment target".to_owned()),
        }
    }

    fn expr_is_quantum(&self, expr: &Expr, vars: &HashMap<String, VmVal>) -> bool {
        match expr {
            Expr::Ident(name, _) => matches!(vars.get(name), Some(VmVal::QReg { .. })),
            Expr::Index { base, .. } | Expr::Slice { base, .. } => self.expr_is_quantum(base, vars),
            Expr::Binary { lhs, rhs, .. } => {
                self.expr_is_quantum(lhs, vars) || self.expr_is_quantum(rhs, vars)
            }
            Expr::Unary { expr: inner, .. } => self.expr_is_quantum(inner, vars),
            _ => false,
        }
    }

    fn exec_quantum_inplace_arith(&mut self, op: BinOp, lhs: &VmVal, rhs: &VmVal) {
        let (
            VmVal::QReg {
                reg: a_reg,
                width: w_a,
                ..
            },
            VmVal::QReg {
                reg: b_reg,
                width: w_b,
                ..
            },
        ) = (lhs, rhs)
        else {
            return;
        };
        let w = (*w_a).min(*w_b).max(1);
        let stmts = match op {
            BinOp::Add => match self.synth_mode {
                SynthMode::Width => synthesize_cuccaro_adder(a_reg, b_reg, w),
                SynthMode::Depth => synthesize_draper_qft_adder(a_reg, b_reg, w),
            },
            BinOp::Sub => {
                let add = match self.synth_mode {
                    SynthMode::Width => synthesize_cuccaro_adder(a_reg, b_reg, w),
                    SynthMode::Depth => synthesize_draper_qft_adder(a_reg, b_reg, w),
                };
                invert_stmts(&add)
            }
            BinOp::BitXor => (0..w)
                .map(|bit| MirStmt::Gate {
                    kind: MirGateKind::X,
                    controls: vec![QubitWire::new(a_reg.clone(), bit)],
                    targets: vec![QubitWire::new(b_reg.clone(), bit)],
                })
                .collect(),
            _ => Vec::new(),
        };
        self.emit_or_exec_mir(stmts);
    }

    fn exec_quantum_if(
        &mut self,
        cond: &Expr,
        then_block: &Block,
        vars: &mut HashMap<String, VmVal>,
    ) -> Result<(), String> {
        let Expr::Binary { op, lhs, rhs, .. } = cond else {
            return Ok(());
        };
        let (lhs_q, add_q) = match lhs.as_ref() {
            Expr::Ident(_, _) => (self.eval_expr(lhs, vars)?, None),
            Expr::Binary {
                op: BinOp::Add,
                lhs: l,
                rhs: r,
                ..
            } => (self.eval_expr(l, vars)?, Some(self.eval_expr(r, vars)?)),
            _ => return Ok(()),
        };
        let VmVal::QReg {
            reg: lhs_reg,
            width,
            ..
        } = lhs_q
        else {
            return Ok(());
        };
        let rhs_val = self.eval_expr(rhs, vars)?;

        let flag_q = self.alloc_quantum_reg("__qif_flag", 1, true);
        let flag_wire = flag_q.wires()[0].clone();

        let mut compute = Vec::new();
        if let Some(VmVal::QReg { reg: add_reg, .. }) = add_q {
            match self.synth_mode {
                SynthMode::Width => {
                    compute.extend(synthesize_cuccaro_adder(&add_reg, &lhs_reg, width))
                }
                SynthMode::Depth => {
                    compute.extend(synthesize_draper_qft_adder(&add_reg, &lhs_reg, width))
                }
            }
        } else if let VmVal::QReg { ref reg, .. } = rhs_val {
            for bit in 0..width {
                compute.push(MirStmt::Gate {
                    kind: MirGateKind::X,
                    controls: vec![QubitWire::new(reg.clone(), bit)],
                    targets: vec![QubitWire::new(lhs_reg.clone(), bit)],
                });
            }
        }

        let effective_const = match rhs_val {
            VmVal::Int(n) => n,
            _ => 0,
        };
        let max_basis = 1i64 << width.min(8);
        let matching: Vec<i64> = (0..max_basis)
            .filter(|&v| match op {
                BinOp::Eq => v == effective_const,
                BinOp::Lt => v < effective_const,
                BinOp::Gt => v > effective_const,
                _ => v == effective_const,
            })
            .collect();

        for val in matching {
            for bit in 0..width {
                if ((val >> bit) & 1) == 0 {
                    compute.push(MirStmt::Gate {
                        kind: MirGateKind::X,
                        controls: vec![],
                        targets: vec![QubitWire::new(lhs_reg.clone(), bit)],
                    });
                }
            }
            let ctrls: Vec<QubitWire> = (0..width)
                .map(|b| QubitWire::new(lhs_reg.clone(), b))
                .collect();
            compute.push(MirStmt::Gate {
                kind: MirGateKind::X,
                controls: ctrls,
                targets: vec![flag_wire.clone()],
            });
            for bit in (0..width).rev() {
                if ((val >> bit) & 1) == 0 {
                    compute.push(MirStmt::Gate {
                        kind: MirGateKind::X,
                        controls: vec![],
                        targets: vec![QubitWire::new(lhs_reg.clone(), bit)],
                    });
                }
            }
        }

        // Record `then_block` gates
        self.recording_stack.push(Vec::new());
        let _ = self.exec_block(then_block, vars)?;
        let recorded_then = self.recording_stack.pop().unwrap_or_default();
        let controlled_action: Vec<MirStmt> = recorded_then
            .into_iter()
            .map(|s| apply_controls_to_stmt(s, std::slice::from_ref(&flag_wire)))
            .collect();

        let conj = MirStmt::Conjugate {
            compute,
            action: controlled_action,
            target: None,
        };
        self.emit_or_exec_mir(vec![conj]);
        Ok(())
    }

    pub fn eval_expr(
        &mut self,
        expr: &Expr,
        vars: &mut HashMap<String, VmVal>,
    ) -> Result<VmVal, String> {
        match expr {
            Expr::Ident(name, _) => {
                if let Some(v) = vars.get(name) {
                    Ok(v.clone())
                } else if let Some(c) = self.consts.get(name) {
                    Ok(c.clone())
                } else {
                    Err(format!("undefined variable `{}`", name))
                }
            }
            Expr::IntLit(n, _) => Ok(VmVal::Int(*n)),
            Expr::FloatLit(s, _) => Ok(VmVal::Float(s.parse::<f64>().unwrap_or(0.0))),
            Expr::BoolLit(b, _) => Ok(VmVal::Bool(*b)),
            Expr::StringLit(s, _) => Ok(VmVal::Str(s.clone())),
            Expr::StructLit { name, fields, .. } => {
                let mut map = HashMap::new();
                if let Some(decl_fields) = self.structs.get(name).cloned() {
                    for (fname, fty) in &decl_fields {
                        map.insert(fname.clone(), self.default_val_for_type(fty));
                    }
                    for (idx, (k, vexpr)) in fields.iter().enumerate() {
                        let val = self.eval_expr(vexpr, vars)?;
                        if let Some(pos_idx) = k.strip_prefix('_').and_then(|s| s.parse::<usize>().ok()) {
                            if let Some((actual_name, _)) = decl_fields.get(pos_idx) {
                                map.insert(actual_name.clone(), val);
                                continue;
                            }
                        }
                        let _ = idx;
                        map.insert(k.clone(), val);
                    }
                } else {
                    for (k, vexpr) in fields {
                        let val = self.eval_expr(vexpr, vars)?;
                        map.insert(k.clone(), val);
                    }
                }
                Ok(VmVal::Struct(name.clone(), map))
            }
            Expr::SliceLit { elems, .. } => {
                let mut out = Vec::with_capacity(elems.len());
                for e in elems {
                    out.push(self.eval_expr(e, vars)?);
                }
                Ok(VmVal::Slice(out))
            }
            Expr::QAlloc {
                size, ty_override, ..
            } => {
                let w = if let Some(t) = ty_override {
                    self.type_qubit_width(t)
                } else if let Some(c) = size {
                    self.resolve_const_expr(c, vars)
                } else {
                    1
                };
                Ok(self.alloc_quantum_reg("qalloc", w, true))
            }
            Expr::Measure { arg, .. } => {
                let qval = self.eval_expr(arg, vars)?;
                let wires = qval.wires();
                let mut acc = 0i64;
                for (bit, w) in wires.iter().enumerate() {
                    let b = self.sim.measure_wire(w);
                    if b != 0 {
                        acc |= 1i64 << bit;
                    }
                }
                Ok(VmVal::Int(acc))
            }
            Expr::Index { base, index, .. } => {
                let bval = self.eval_expr(base, vars)?;
                let idx = self.eval_expr(index, vars)?.as_int().max(0) as usize;
                match bval {
                    VmVal::Slice(items) => Ok(items.get(idx).cloned().unwrap_or(VmVal::Void)),
                    VmVal::Str(s) => {
                        let b = s.as_bytes().get(idx).copied().unwrap_or(0);
                        Ok(VmVal::Int(b as i64))
                    }
                    VmVal::QReg { reg, offset, .. } => Ok(VmVal::QReg {
                        reg,
                        offset: offset + idx,
                        width: 1,
                    }),
                    _ => Ok(VmVal::Void),
                }
            }
            Expr::Slice {
                base, start, end, ..
            } => {
                let bval = self.eval_expr(base, vars)?;
                let s_idx = match start {
                    Some(se) => self.eval_expr(se, vars)?.as_int().max(0) as usize,
                    None => 0,
                };
                match bval {
                    VmVal::Slice(items) => {
                        let e_idx = match end {
                            Some(ee) => self.eval_expr(ee, vars)?.as_int().max(0) as usize,
                            None => items.len(),
                        };
                        let s = s_idx.min(items.len());
                        let e = e_idx.min(items.len()).max(s);
                        Ok(VmVal::Slice(items[s..e].to_vec()))
                    }
                    VmVal::Str(st) => {
                        let e_idx = match end {
                            Some(ee) => self.eval_expr(ee, vars)?.as_int().max(0) as usize,
                            None => st.len(),
                        };
                        let s = s_idx.min(st.len());
                        let e = e_idx.min(st.len()).max(s);
                        Ok(VmVal::Str(st[s..e].to_owned()))
                    }
                    VmVal::QReg { reg, offset, width } => {
                        let e_idx = match end {
                            Some(ee) => self.eval_expr(ee, vars)?.as_int().max(0) as usize,
                            None => width,
                        };
                        let s = s_idx.min(width);
                        let e = e_idx.min(width).max(s);
                        Ok(VmVal::QReg {
                            reg,
                            offset: offset + s,
                            width: e - s,
                        })
                    }
                    other => Ok(other),
                }
            }
            Expr::FieldAccess { base, field, .. } => {
                let bval = self.eval_expr(base, vars)?;
                if let VmVal::Struct(_, fields) = bval {
                    Ok(fields.get(field).cloned().unwrap_or(VmVal::Void))
                } else {
                    Ok(VmVal::Void)
                }
            }
            Expr::Unary { op, expr: inner, .. } => {
                let v = self.eval_expr(inner, vars)?;
                match op {
                    UnOp::Neg => match v {
                        VmVal::Float(f) => Ok(VmVal::Float(-f)),
                        other => Ok(VmVal::Int(-other.as_int())),
                    },
                    UnOp::Not => Ok(VmVal::Bool(!v.as_bool())),
                }
            }
            Expr::Binary { op, lhs, rhs, .. } => {
                // Short-circuit logical && and ||
                if *op == BinOp::And {
                    let l = self.eval_expr(lhs, vars)?;
                    if !l.as_bool() {
                        return Ok(VmVal::Bool(false));
                    }
                    let r = self.eval_expr(rhs, vars)?;
                    return Ok(VmVal::Bool(r.as_bool()));
                }
                if *op == BinOp::Or {
                    let l = self.eval_expr(lhs, vars)?;
                    if l.as_bool() {
                        return Ok(VmVal::Bool(true));
                    }
                    let r = self.eval_expr(rhs, vars)?;
                    return Ok(VmVal::Bool(r.as_bool()));
                }

                let l = self.eval_expr(lhs, vars)?;
                let r = self.eval_expr(rhs, vars)?;

                match (&l, &r, op) {
                    (VmVal::Str(s1), VmVal::Str(s2), BinOp::Add) => {
                        return Ok(VmVal::Str(format!("{}{}", s1, s2)));
                    }
                    (VmVal::Str(s1), VmVal::Str(s2), BinOp::Eq) => {
                        return Ok(VmVal::Bool(s1 == s2));
                    }
                    (VmVal::Str(s1), VmVal::Str(s2), BinOp::NotEq) => {
                        return Ok(VmVal::Bool(s1 != s2));
                    }
                    (VmVal::Float(_), _, _) | (_, VmVal::Float(_), _) => {
                        let lf = l.as_float();
                        let rf = r.as_float();
                        return Ok(match op {
                            BinOp::Add => VmVal::Float(lf + rf),
                            BinOp::Sub => VmVal::Float(lf - rf),
                            BinOp::Mul => VmVal::Float(lf * rf),
                            BinOp::Div => VmVal::Float(if rf.abs() > 1e-15 { lf / rf } else { 0.0 }),
                            BinOp::Eq => VmVal::Bool((lf - rf).abs() < 1e-12),
                            BinOp::NotEq => VmVal::Bool((lf - rf).abs() >= 1e-12),
                            BinOp::Lt => VmVal::Bool(lf < rf),
                            BinOp::LtEq => VmVal::Bool(lf <= rf),
                            BinOp::Gt => VmVal::Bool(lf > rf),
                            BinOp::GtEq => VmVal::Bool(lf >= rf),
                            _ => VmVal::Float(0.0),
                        });
                    }
                    _ => {}
                }

                let li = l.as_int();
                let ri = r.as_int();
                Ok(match op {
                    BinOp::Add => VmVal::Int(li.wrapping_add(ri)),
                    BinOp::Sub => VmVal::Int(li.wrapping_sub(ri)),
                    BinOp::Mul => VmVal::Int(li.wrapping_mul(ri)),
                    BinOp::Div => VmVal::Int(if ri != 0 { li / ri } else { 0 }),
                    BinOp::Rem => VmVal::Int(if ri != 0 { li % ri } else { 0 }),
                    BinOp::BitXor => VmVal::Int(li ^ ri),
                    BinOp::BitAnd => VmVal::Int(li & ri),
                    BinOp::BitOr => VmVal::Int(li | ri),
                    BinOp::Shl => VmVal::Int(li.wrapping_shl(ri as u32)),
                    BinOp::Shr => VmVal::Int(li.wrapping_shr(ri as u32)),
                    BinOp::Eq => VmVal::Bool(l == r),
                    BinOp::NotEq => VmVal::Bool(l != r),
                    BinOp::Lt => VmVal::Bool(li < ri),
                    BinOp::LtEq => VmVal::Bool(li <= ri),
                    BinOp::Gt => VmVal::Bool(li > ri),
                    BinOp::GtEq => VmVal::Bool(li >= ri),
                    BinOp::And => VmVal::Bool((li != 0) && (ri != 0)),
                    BinOp::Or => VmVal::Bool((li != 0) || (ri != 0)),
                })
            }
            Expr::QuantumMod { kind, expr: inner, .. } => {
                self.recording_stack.push(Vec::new());
                let res = self.eval_expr(inner, vars);
                let recorded = self.recording_stack.pop().unwrap_or_default();
                let val = res?;
                let transformed = match kind {
                    ModKind::Adj => invert_stmts(&recorded),
                    ModKind::Ctrl(ctrl_exprs) => {
                        let mut ctrls = Vec::new();
                        for ce in ctrl_exprs {
                            let cv = self.eval_expr(ce, vars)?;
                            ctrls.extend(cv.wires());
                        }
                        recorded
                            .into_iter()
                            .map(|s| apply_controls_to_stmt(s, &ctrls))
                            .collect()
                    }
                };
                self.emit_or_exec_mir(transformed);
                Ok(val)
            }
            Expr::Call { callee, args, .. } => {
                let mut eval_args = Vec::with_capacity(args.len());
                for a in args {
                    eval_args.push(self.eval_expr(a, vars)?);
                }
                self.eval_call(callee, eval_args)
            }
        }
    }

    fn resolve_const_expr(&self, c: &ConstExpr, vars: &HashMap<String, VmVal>) -> usize {
        match c {
            ConstExpr::Int(n) => *n,
            ConstExpr::Ident(s) => vars
                .get(s)
                .map(|v| v.as_int().max(0) as usize)
                .or_else(|| self.const_uints.get(s).copied())
                .unwrap_or(1),
        }
    }

    fn eval_call(&mut self, callee: &str, args: Vec<VmVal>) -> Result<VmVal, String> {
        // 1. Built-in 1-qubit quantum gates (with slice broadcast)
        let one_q = match callee {
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
        if let Some(kind) = one_q {
            if let Some(q) = args.first() {
                let stmts: Vec<MirStmt> = q
                    .wires()
                    .into_iter()
                    .map(|w| MirStmt::Gate {
                        kind: kind.clone(),
                        controls: vec![],
                        targets: vec![w],
                    })
                    .collect();
                self.emit_or_exec_mir(stmts);
            }
            return Ok(VmVal::Void);
        }

        // 2. Parameterized rotations Rx, Ry, Rz, Phase, P
        if matches!(callee, "Rx" | "Ry" | "Rz" | "Phase" | "P") && args.len() == 2 {
            let theta = args[0].as_float();
            let kind = match callee {
                "Rx" => MirGateKind::Rx(theta),
                "Ry" => MirGateKind::Ry(theta),
                "Rz" => MirGateKind::Rz(theta),
                _ => MirGateKind::Phase(theta),
            };
            let stmts: Vec<MirStmt> = args[1]
                .wires()
                .into_iter()
                .map(|w| MirStmt::Gate {
                    kind: kind.clone(),
                    controls: vec![],
                    targets: vec![w],
                })
                .collect();
            self.emit_or_exec_mir(stmts);
            return Ok(VmVal::Void);
        }

        // 3. Controlled phase CP, CRz
        if matches!(callee, "CP" | "CRz") && args.len() == 3 {
            let theta = args[0].as_float();
            let ctrls = args[1].wires();
            let kind = if callee == "CRz" {
                MirGateKind::Rz(theta)
            } else {
                MirGateKind::Phase(theta)
            };
            let stmts: Vec<MirStmt> = args[2]
                .wires()
                .into_iter()
                .map(|t| MirStmt::Gate {
                    kind: kind.clone(),
                    controls: ctrls.clone(),
                    targets: vec![t],
                })
                .collect();
            self.emit_or_exec_mir(stmts);
            return Ok(VmVal::Void);
        }

        // 4. CX / CNOT
        if matches!(callee, "CX" | "CNOT") && args.len() == 2 {
            let cw = args[0].wires();
            let tw = args[1].wires();
            let stmts: Vec<MirStmt> = cw
                .into_iter()
                .zip(tw.into_iter())
                .map(|(c, t)| MirStmt::Gate {
                    kind: MirGateKind::X,
                    controls: vec![c],
                    targets: vec![t],
                })
                .collect();
            self.emit_or_exec_mir(stmts);
            return Ok(VmVal::Void);
        }

        // 5. CCX / Toffoli
        if matches!(callee, "CCX" | "Toffoli") && args.len() == 3 {
            let c0 = args[0].wires();
            let c1 = args[1].wires();
            let t = args[2].wires();
            if let (Some(w0), Some(w1), Some(wt)) = (c0.first(), c1.first(), t.first()) {
                self.emit_or_exec_mir(vec![MirStmt::Gate {
                    kind: MirGateKind::X,
                    controls: vec![w0.clone(), w1.clone()],
                    targets: vec![wt.clone()],
                }]);
            }
            return Ok(VmVal::Void);
        }

        // 6. Swap / SWAP
        if matches!(callee, "Swap" | "SWAP") && args.len() == 2 {
            let w0 = args[0].wires();
            let w1 = args[1].wires();
            let stmts: Vec<MirStmt> = w0
                .into_iter()
                .zip(w1.into_iter())
                .map(|(a, b)| MirStmt::Gate {
                    kind: MirGateKind::Swap,
                    controls: vec![],
                    targets: vec![a, b],
                })
                .collect();
            self.emit_or_exec_mir(stmts);
            return Ok(VmVal::Void);
        }

        // 7. CuccaroAdd, CuccaroSub, DraperAdd, DraperSub
        if matches!(
            callee,
            "CuccaroAdd" | "CuccaroSub" | "DraperAdd" | "DraperSub"
        ) && args.len() == 2
        {
            let op = if callee.ends_with("Sub") {
                BinOp::Sub
            } else {
                BinOp::Add
            };
            self.exec_quantum_inplace_arith(op, &args[0], &args[1]);
            return Ok(VmVal::Void);
        }

        // 8. reset(q)
        if callee == "reset" && args.len() == 1 {
            let stmts: Vec<MirStmt> = args[0]
                .wires()
                .into_iter()
                .map(|qubit| MirStmt::Reset { qubit })
                .collect();
            self.emit_or_exec_mir(stmts);
            return Ok(VmVal::Void);
        }

        // 9. Classical & Hybrid Built-ins
        match callee {
            "len" => {
                let n = match args.first() {
                    Some(VmVal::Str(s)) => s.len() as i64,
                    Some(VmVal::Slice(v)) => v.len() as i64,
                    Some(VmVal::QReg { width, .. }) => *width as i64,
                    _ => 0,
                };
                return Ok(VmVal::Int(n));
            }
            "append" => {
                if let Some(VmVal::Slice(mut v)) = args.first().cloned() {
                    for elem in args.into_iter().skip(1) {
                        v.push(elem);
                    }
                    return Ok(VmVal::Slice(v));
                }
                return Ok(VmVal::Slice(Vec::new()));
            }
            "int_to_str" => {
                let n = args.first().map(|v| v.as_int()).unwrap_or(0);
                return Ok(VmVal::Str(n.to_string()));
            }
            "float_to_str" => {
                let f = args.first().map(|v| v.as_float()).unwrap_or(0.0);
                return Ok(VmVal::Str(format!("{:.8}", f)));
            }
            "str_to_int" => {
                let s = args.first().map(|v| v.as_str()).unwrap_or_default();
                let n = s.trim().parse::<i64>().unwrap_or(0);
                return Ok(VmVal::Int(n));
            }
            "char_at" => {
                let s = args.first().map(|v| v.as_str()).unwrap_or_default();
                let idx = args.get(1).map(|v| v.as_int().max(0) as usize).unwrap_or(0);
                let ch = s
                    .as_bytes()
                    .get(idx)
                    .map(|&b| (b as char).to_string())
                    .unwrap_or_default();
                return Ok(VmVal::Str(ch));
            }
            "byte_at" => {
                let s = args.first().map(|v| v.as_str()).unwrap_or_default();
                let idx = args.get(1).map(|v| v.as_int().max(0) as usize).unwrap_or(0);
                let b = s.as_bytes().get(idx).copied().unwrap_or(0) as i64;
                return Ok(VmVal::Int(b));
            }
            "from_byte" => {
                let b = args.first().map(|v| v.as_int() as u8).unwrap_or(0);
                return Ok(VmVal::Str((b as char).to_string()));
            }
            "ord" => {
                let s = args.first().map(|v| v.as_str()).unwrap_or_default();
                let b = s.as_bytes().first().copied().unwrap_or(0) as i64;
                return Ok(VmVal::Int(b));
            }
            "substr" => {
                let s = args.first().map(|v| v.as_str()).unwrap_or_default();
                let start = args.get(1).map(|v| v.as_int().max(0) as usize).unwrap_or(0);
                let end = args
                    .get(2)
                    .map(|v| v.as_int().max(0) as usize)
                    .unwrap_or(s.len());
                let st = start.min(s.len());
                let en = end.min(s.len()).max(st);
                return Ok(VmVal::Str(s[st..en].to_owned()));
            }
            "contains" => {
                let s = args.first().map(|v| v.as_str()).unwrap_or_default();
                let sub = args.get(1).map(|v| v.as_str()).unwrap_or_default();
                return Ok(VmVal::Bool(s.contains(&sub)));
            }
            "starts_with" => {
                let s = args.first().map(|v| v.as_str()).unwrap_or_default();
                let p = args.get(1).map(|v| v.as_str()).unwrap_or_default();
                return Ok(VmVal::Bool(s.starts_with(&p)));
            }
            "ends_with" => {
                let s = args.first().map(|v| v.as_str()).unwrap_or_default();
                let p = args.get(1).map(|v| v.as_str()).unwrap_or_default();
                return Ok(VmVal::Bool(s.ends_with(&p)));
            }
            "trim" => {
                let s = args.first().map(|v| v.as_str()).unwrap_or_default();
                return Ok(VmVal::Str(s.trim().to_owned()));
            }
            "split" => {
                let s = args.first().map(|v| v.as_str()).unwrap_or_default();
                let sep = args.get(1).map(|v| v.as_str()).unwrap_or_default();
                let parts: Vec<VmVal> = s
                    .split(&sep)
                    .map(|p| VmVal::Str(p.to_owned()))
                    .collect();
                return Ok(VmVal::Slice(parts));
            }
            "join" => {
                let sep = args.get(1).map(|v| v.as_str()).unwrap_or_default();
                if let Some(VmVal::Slice(items)) = args.first() {
                    let strs: Vec<String> = items.iter().map(|v| v.as_str()).collect();
                    return Ok(VmVal::Str(strs.join(&sep)));
                }
                return Ok(VmVal::Str(String::new()));
            }
            "replace" => {
                let s = args.first().map(|v| v.as_str()).unwrap_or_default();
                let from = args.get(1).map(|v| v.as_str()).unwrap_or_default();
                let to = args.get(2).map(|v| v.as_str()).unwrap_or_default();
                return Ok(VmVal::Str(s.replace(&from, &to)));
            }
            "print" => {
                let text: Vec<String> = args.iter().map(|v| v.as_str()).collect();
                self.stdout.push_str(&text.join(" "));
                return Ok(VmVal::Void);
            }
            "println" => {
                let text: Vec<String> = args.iter().map(|v| v.as_str()).collect();
                self.stdout.push_str(&text.join(" "));
                self.stdout.push('\n');
                return Ok(VmVal::Void);
            }
            "eprintln" => {
                let text: Vec<String> = args.iter().map(|v| v.as_str()).collect();
                eprintln!("{}", text.join(" "));
                return Ok(VmVal::Void);
            }
            "format" => {
                let text: Vec<String> = args.iter().map(|v| v.as_str()).collect();
                return Ok(VmVal::Str(text.join("")));
            }
            "read_file" => {
                let path = args.first().map(|v| v.as_str()).unwrap_or_default();
                let content = fs::read_to_string(&path)
                    .map_err(|e| format!("read_file(`{}`) failed: {}", path, e))?;
                return Ok(VmVal::Str(content));
            }
            "write_file" => {
                let path = args.first().map(|v| v.as_str()).unwrap_or_default();
                let content = args.get(1).map(|v| v.as_str()).unwrap_or_default();
                let ok = fs::write(&path, content).is_ok();
                return Ok(VmVal::Bool(ok));
            }
            "args" => {
                let items = self
                    .cli_args
                    .iter()
                    .map(|s| VmVal::Str(s.clone()))
                    .collect();
                return Ok(VmVal::Slice(items));
            }
            "assert" => {
                let cond = args.first().map(|v| v.as_bool()).unwrap_or(false);
                if !cond {
                    let msg = args
                        .get(1)
                        .map(|v| v.as_str())
                        .unwrap_or_else(|| "assertion failed".to_owned());
                    return Err(format!("AssertionFailed: {}", msg));
                }
                return Ok(VmVal::Void);
            }
            "assert_eq" => {
                let a = args.first().cloned().unwrap_or(VmVal::Void);
                let b = args.get(1).cloned().unwrap_or(VmVal::Void);
                if a != b {
                    return Err(format!(
                        "AssertionEqualFailed: left = `{}`, right = `{}`",
                        a.as_str(),
                        b.as_str()
                    ));
                }
                return Ok(VmVal::Void);
            }
            "prob_zero" => {
                if let Some(q) = args.first() {
                    if let Some(w) = q.wires().first() {
                        return Ok(VmVal::Float(self.sim.prob_zero(w)));
                    }
                }
                return Ok(VmVal::Float(1.0));
            }
            "prob_basis" => {
                if let (Some(q), Some(expected)) = (args.first(), args.get(1)) {
                    let wires = q.wires();
                    let val = expected.as_int().max(0) as usize;
                    return Ok(VmVal::Float(self.sim.prob_wires_basis(&wires, val)));
                }
                return Ok(VmVal::Float(0.0));
            }
            "peek_uint" => {
                if let Some(q) = args.first() {
                    let wires = q.wires();
                    return Ok(VmVal::Int(self.sim.peek_wires_uint(&wires) as i64));
                }
                return Ok(VmVal::Int(0));
            }
            _ => {}
        }

        // 10. User-defined function call
        self.call_function(callee, args)
    }
}

fn stmt_touches_ident(stmt: &Stmt, var: &str) -> bool {
    match stmt {
        Stmt::Decl { name, init, .. } => name == var || expr_touches_ident(init, var),
        Stmt::Assign { target, value, .. } => {
            expr_touches_ident(target, var) || expr_touches_ident(value, var)
        }
        Stmt::DeferAdj { call, target, .. } => {
            target.as_deref() == Some(var) || expr_touches_ident(call, var)
        }
        Stmt::QuantumMod { kind, body, .. } => {
            let c_touch = match kind {
                ModKind::Adj => false,
                ModKind::Ctrl(cs) => cs.iter().any(|e| expr_touches_ident(e, var)),
            };
            c_touch || stmt_touches_ident(body, var)
        }
        Stmt::If {
            cond,
            then_block,
            else_block,
            ..
        } => {
            expr_touches_ident(cond, var)
                || then_block.stmts.iter().any(|s| stmt_touches_ident(s, var))
                || else_block
                    .as_ref()
                    .map(|b| b.stmts.iter().any(|s| stmt_touches_ident(s, var)))
                    .unwrap_or(false)
        }
        Stmt::For {
            init,
            cond,
            post,
            body,
            ..
        } => {
            init.as_ref().map(|s| stmt_touches_ident(s, var)).unwrap_or(false)
                || cond.as_ref().map(|e| expr_touches_ident(e, var)).unwrap_or(false)
                || post.as_ref().map(|s| stmt_touches_ident(s, var)).unwrap_or(false)
                || body.stmts.iter().any(|s| stmt_touches_ident(s, var))
        }
        Stmt::Return { value, .. } => value
            .as_ref()
            .map(|e| expr_touches_ident(e, var))
            .unwrap_or(false),
        Stmt::Break(_) | Stmt::Continue(_) => false,
        Stmt::Block(b) => b.stmts.iter().any(|s| stmt_touches_ident(s, var)),
        Stmt::Expr { expr, .. } => expr_touches_ident(expr, var),
    }
}

fn expr_touches_ident(expr: &Expr, var: &str) -> bool {
    match expr {
        Expr::Ident(name, _) => name == var,
        Expr::Measure { arg, .. } => expr_touches_ident(arg, var),
        Expr::Call { callee, args, .. } => {
            if matches!(
                callee.as_str(),
                "prob_zero" | "prob_basis" | "peek_uint" | "assert" | "assert_eq"
            ) {
                false
            } else {
                args.iter().any(|a| expr_touches_ident(a, var))
            }
        }
        Expr::Index { base, index, .. } => {
            expr_touches_ident(base, var) || expr_touches_ident(index, var)
        }
        Expr::Slice {
            base, start, end, ..
        } => {
            expr_touches_ident(base, var)
                || start.as_ref().map(|e| expr_touches_ident(e, var)).unwrap_or(false)
                || end.as_ref().map(|e| expr_touches_ident(e, var)).unwrap_or(false)
        }
        Expr::Binary { lhs, rhs, .. } => {
            expr_touches_ident(lhs, var) || expr_touches_ident(rhs, var)
        }
        Expr::Unary { expr: inner, .. } => expr_touches_ident(inner, var),
        Expr::QuantumMod { kind, expr: inner, .. } => {
            let c_touch = match kind {
                ModKind::Adj => false,
                ModKind::Ctrl(cs) => cs.iter().any(|e| expr_touches_ident(e, var)),
            };
            c_touch || expr_touches_ident(inner, var)
        }
        Expr::FieldAccess { base, .. } => expr_touches_ident(base, var),
        Expr::StructLit { fields, .. } => fields.iter().any(|(_, e)| expr_touches_ident(e, var)),
        Expr::SliceLit { elems, .. } => elems.iter().any(|e| expr_touches_ident(e, var)),
        _ => false,
    }
}
