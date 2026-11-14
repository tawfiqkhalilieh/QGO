//! `qgo_opt`: Algebraic optimization passes ($U \cdot U^\dagger$ cancellation,
//! `ctrl`-conjugation $U \cdot (\text{ctrl}_c V) \cdot U^\dagger$, and Pass 4 Quantum Expression
//! & Arithmetic synthesis using Cuccaro ripple-carry or Draper QFT adders).

use std::collections::HashMap;
use qgo_mir::{
    apply_controls_to_stmt, invert_stmts, MirFunc, MirGateKind, MirModule, MirStmt,
    QuantumCmpKind, QubitWire,
};

/// Arithmetic synthesis strategy (`--synth-mode=width|depth`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SynthMode {
    /// Cuccaro ripple-carry adder (minimizes qubit width: 1 ancilla carry qubit).
    Width,
    /// Draper QFT adder (minimizes Toffoli depth via Fourier-basis phase rotations).
    Depth,
}

/// Resource estimation report for a compiled `MirModule`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResourceReport {
    pub logical_qubits: usize,
    pub ancilla_qubits: usize,
    pub recycled_ancillas: usize,
    pub single_qubit_gates: usize,
    pub two_qubit_gates: usize,
    pub toffoli_gates: usize,
    pub t_count: usize,
    pub circuit_depth: usize,
}

/// Runs all optimization and lowering passes on a [`MirModule`].
pub fn optimize_module(module: &mut MirModule, mode: SynthMode) {
    for func in &mut module.funcs {
        optimize_func(func, mode);
    }
}

pub fn optimize_func(func: &mut MirFunc, mode: SynthMode) {
    // 1. Lower QuantumIf and QuantumArith nodes into Conjugate + Cuccaro/Draper circuits
    func.body = lower_quantum_expressions(std::mem::take(&mut func.body), mode);
    // 2. Recycle freed |0> ancilla registers across sequential live ranges via the Physical Qubit Pool
    func.body = recycle_ancillas(std::mem::take(&mut func.body)).0;
    // 3. Optimize inside Conjugate nodes and cancel adjacent inverse gates U * U_dag
    func.body = cancel_inverses_recursive(std::mem::take(&mut func.body));
}

/// Pass 4.5: Physical Qubit Pool & Ancilla Recycling Allocator.
/// Reuses uncomputed $|0\rangle$ ancilla registers released via `MirStmt::QFree` across
/// subsequent `MirStmt::QAlloc` live ranges, minimizing peak physical ancilla qubit width.
pub fn recycle_ancillas(stmts: Vec<MirStmt>) -> (Vec<MirStmt>, usize) {
    let mut recycler = AncillaPoolRecycler::default();
    let out = recycler.recycle_block(stmts);
    (out, recycler.recycled_qubits)
}

#[derive(Default)]
struct AncillaPoolRecycler {
    /// Pool of `(physical_reg_name, width)` ancilla registers verified in $|0\rangle$ and free for reuse.
    free_pool: Vec<(String, usize)>,
    /// Active mapping from logical ancilla register name to assigned physical ancilla register name.
    reg_alias: HashMap<String, String>,
    /// Cumulative number of ancilla qubits satisfied from `free_pool` without allocating new physical wires.
    recycled_qubits: usize,
}

impl AncillaPoolRecycler {
    fn map_reg(&self, reg: &str) -> String {
        self.reg_alias
            .get(reg)
            .cloned()
            .unwrap_or_else(|| reg.to_owned())
    }

    fn map_wire(&self, w: QubitWire) -> QubitWire {
        QubitWire::new(self.map_reg(&w.reg), w.index)
    }

    fn recycle_block(&mut self, stmts: Vec<MirStmt>) -> Vec<MirStmt> {
        let mut out = Vec::with_capacity(stmts.len());
        for stmt in stmts {
            match stmt {
                MirStmt::QAlloc {
                    reg,
                    size,
                    is_ancilla,
                } => {
                    if is_ancilla {
                        if let Some(pos) = self.free_pool.iter().position(|(_, s)| *s == size) {
                            let (phys_reg, _) = self.free_pool.remove(pos);
                            self.reg_alias.insert(reg, phys_reg.clone());
                            self.recycled_qubits += size;
                            out.push(MirStmt::QAlloc {
                                reg: phys_reg,
                                size,
                                is_ancilla: true,
                            });
                        } else {
                            self.reg_alias.insert(reg.clone(), reg.clone());
                            out.push(MirStmt::QAlloc {
                                reg,
                                size,
                                is_ancilla: true,
                            });
                        }
                    } else {
                        out.push(MirStmt::QAlloc {
                            reg,
                            size,
                            is_ancilla: false,
                        });
                    }
                }
                MirStmt::QFree { reg, size } => {
                    let phys_reg = self.reg_alias.remove(&reg).unwrap_or(reg);
                    if !self.free_pool.iter().any(|(r, _)| *r == phys_reg) {
                        self.free_pool.push((phys_reg.clone(), size));
                    }
                    out.push(MirStmt::QFree {
                        reg: phys_reg,
                        size,
                    });
                }
                MirStmt::Gate {
                    kind,
                    controls,
                    targets,
                } => {
                    out.push(MirStmt::Gate {
                        kind,
                        controls: controls.into_iter().map(|w| self.map_wire(w)).collect(),
                        targets: targets.into_iter().map(|w| self.map_wire(w)).collect(),
                    });
                }
                MirStmt::Conjugate {
                    compute,
                    action,
                    target,
                } => {
                    let mapped_target = target.map(|t| self.map_reg(&t));
                    let new_compute = self.recycle_block(compute);
                    let new_action = self.recycle_block(action);
                    out.push(MirStmt::Conjugate {
                        compute: new_compute,
                        action: new_action,
                        target: mapped_target,
                    });
                }
                MirStmt::Measure { qubit, dst_var } => {
                    out.push(MirStmt::Measure {
                        qubit: self.map_wire(qubit),
                        dst_var,
                    });
                }
                MirStmt::Reset { qubit } => {
                    out.push(MirStmt::Reset {
                        qubit: self.map_wire(qubit),
                    });
                }
                MirStmt::ClassicalIf {
                    cond_var,
                    then_body,
                    else_body,
                } => {
                    out.push(MirStmt::ClassicalIf {
                        cond_var,
                        then_body: self.recycle_block(then_body),
                        else_body: self.recycle_block(else_body),
                    });
                }
                MirStmt::While { cond_var, body } => {
                    out.push(MirStmt::While {
                        cond_var,
                        body: self.recycle_block(body),
                    });
                }
                other => out.push(other),
            }
        }
        out
    }
}

/// Flattens `MirStmt::Conjugate` nodes into elementary gate sequences $(U, V, U^\dagger)$
/// after all `ctrl`-conjugation and algebraic optimizations have been applied, then runs a final
/// peephole $U \cdot U^\dagger$ cancellation pass.
pub fn flatten_conjugates(stmts: &[MirStmt]) -> Vec<MirStmt> {
    let mut out = Vec::new();
    for s in stmts {
        match s {
            MirStmt::Conjugate {
                compute, action, ..
            } => {
                let flat_u = flatten_conjugates(compute);
                let flat_v = flatten_conjugates(action);
                let flat_u_dag = invert_stmts(&flat_u);
                out.extend(flat_u);
                out.extend(flat_v);
                out.extend(flat_u_dag);
            }
            MirStmt::ClassicalIf {
                cond_var,
                then_body,
                else_body,
            } => {
                out.push(MirStmt::ClassicalIf {
                    cond_var: cond_var.clone(),
                    then_body: flatten_conjugates(then_body),
                    else_body: flatten_conjugates(else_body),
                });
            }
            MirStmt::While { cond_var, body } => {
                out.push(MirStmt::While {
                    cond_var: cond_var.clone(),
                    body: flatten_conjugates(body),
                });
            }
            other => out.push(other.clone()),
        }
    }
    cancel_inverses_peephole(out)
}

/// Pass 4: Quantum Expression & Arithmetic Lowering (`if <qfree_expr> { Body }` and `qint[N]` adders).
fn lower_quantum_expressions(stmts: Vec<MirStmt>, mode: SynthMode) -> Vec<MirStmt> {
    let mut out = Vec::with_capacity(stmts.len());
    for stmt in stmts {
        match stmt {
            MirStmt::QuantumArith {
                op,
                lhs_reg,
                rhs_reg,
                width,
            } => match op {
                QuantumCmpKind::Add => match mode {
                    SynthMode::Width => out.extend(synthesize_cuccaro_adder(&lhs_reg, &rhs_reg, width)),
                    SynthMode::Depth => out.extend(synthesize_draper_qft_adder(&lhs_reg, &rhs_reg, width)),
                },
                QuantumCmpKind::Sub => {
                    let add_stmts = match mode {
                        SynthMode::Width => synthesize_cuccaro_adder(&lhs_reg, &rhs_reg, width),
                        SynthMode::Depth => synthesize_draper_qft_adder(&lhs_reg, &rhs_reg, width),
                    };
                    out.extend(invert_stmts(&add_stmts));
                }
                QuantumCmpKind::Xor => {
                    for bit in 0..width {
                        out.push(MirStmt::Gate {
                            kind: MirGateKind::X,
                            controls: vec![QubitWire::new(lhs_reg.clone(), bit)],
                            targets: vec![QubitWire::new(rhs_reg.clone(), bit)],
                        });
                    }
                }
                _ => match mode {
                    SynthMode::Width => out.extend(synthesize_cuccaro_adder(&lhs_reg, &rhs_reg, width)),
                    SynthMode::Depth => out.extend(synthesize_draper_qft_adder(&lhs_reg, &rhs_reg, width)),
                },
            },
            MirStmt::QuantumIf {
                op,
                lhs_reg,
                rhs_reg,
                rhs_const,
                width,
                then_body,
            } => {
                let flag_reg = format!("__cmp_flag_{}", lhs_reg);
                let flag_wire = QubitWire::new(flag_reg.clone(), 0);

                out.push(MirStmt::QAlloc {
                    reg: flag_reg.clone(),
                    size: 1,
                    is_ancilla: true,
                });

                // Synthesize reversible comparator/arithmetic into `compute` block
                let mut compute = Vec::new();
                if let (Some(r_reg), Some(_)) = (&rhs_reg, rhs_const) {
                    // `if lhs_reg + r_reg <op> cval`: add `r_reg` into `lhs_reg` inside `compute`
                    match mode {
                        SynthMode::Width => {
                            compute.extend(synthesize_cuccaro_adder(r_reg, &lhs_reg, width))
                        }
                        SynthMode::Depth => {
                            compute.extend(synthesize_draper_qft_adder(r_reg, &lhs_reg, width))
                        }
                    }
                } else if let (Some(r_reg), None) = (&rhs_reg, rhs_const) {
                    // Register-to-register comparison `if lhs_reg == r_reg`: XOR `r_reg` into `lhs_reg`
                    for bit in 0..width {
                        compute.push(MirStmt::Gate {
                            kind: MirGateKind::X,
                            controls: vec![QubitWire::new(r_reg.clone(), bit)],
                            targets: vec![QubitWire::new(lhs_reg.clone(), bit)],
                        });
                    }
                }

                let effective_const = rhs_const.unwrap_or(0);
                let max_basis = 1i64 << width.min(8);
                let matching_values: Vec<i64> = (0..max_basis)
                    .filter(|&v| match op {
                        QuantumCmpKind::Eq => v == effective_const,
                        QuantumCmpKind::Lt => v < effective_const,
                        QuantumCmpKind::Gt => v > effective_const,
                        _ => v == effective_const,
                    })
                    .collect();

                for val in matching_values {
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

                // Controlled body conditioned on `flag_wire`
                let lowered_then = lower_quantum_expressions(then_body, mode);
                let controlled_then: Vec<MirStmt> = lowered_then
                    .into_iter()
                    .map(|s| apply_controls_to_stmt(s, std::slice::from_ref(&flag_wire)))
                    .collect();

                // Wrap evaluation, controlled body, and automatic uncomputation in a `Conjugate` node!
                out.push(MirStmt::Conjugate {
                    compute,
                    action: controlled_then,
                    target: Some(flag_reg.clone()),
                });
                out.push(MirStmt::QFree {
                    reg: flag_reg,
                    size: 1,
                });
            }
            MirStmt::Conjugate {
                compute,
                action,
                target,
            } => {
                out.push(MirStmt::Conjugate {
                    compute: lower_quantum_expressions(compute, mode),
                    action: lower_quantum_expressions(action, mode),
                    target,
                });
            }
            other => out.push(other),
        }
    }
    out
}

/// Synthesizes Cuccaro et al. (2004) in-place reversible ripple-carry adder (`b = a + b mod 2^n`).
/// Uses `MAJ` (Majority) forward cascade and `UMA` (UnMajority and Add) reverse cascade.
pub fn synthesize_cuccaro_adder(a_reg: &str, b_reg: &str, width: usize) -> Vec<MirStmt> {
    let n = width.max(1);
    let carry_reg = format!("__cuccaro_cin_{}_{}", a_reg, b_reg);
    let mut stmts = vec![MirStmt::QAlloc {
        reg: carry_reg.clone(),
        size: 1,
        is_ancilla: true,
    }];

    let mut compute_maj = Vec::new();
    let mut action_uma = Vec::new();

    // Forward MAJ cascade: MAJ(c_i, b_i, a_i)
    for i in 0..n {
        let c_in = if i == 0 {
            QubitWire::new(carry_reg.clone(), 0)
        } else {
            QubitWire::new(a_reg.to_owned(), i - 1)
        };
        let b_i = QubitWire::new(b_reg.to_owned(), i);
        let a_i = QubitWire::new(a_reg.to_owned(), i);

        // MAJ(c_in, b_i, a_i): CX(a_i, b_i); CX(a_i, c_in); CCX(c_in, b_i, a_i)
        compute_maj.push(MirStmt::Gate {
            kind: MirGateKind::X,
            controls: vec![a_i.clone()],
            targets: vec![b_i.clone()],
        });
        compute_maj.push(MirStmt::Gate {
            kind: MirGateKind::X,
            controls: vec![a_i.clone()],
            targets: vec![c_in.clone()],
        });
        compute_maj.push(MirStmt::Gate {
            kind: MirGateKind::X,
            controls: vec![c_in, b_i],
            targets: vec![a_i],
        });
    }

    // Reverse UMA cascade: UMA(c_i, b_i, a_i)
    for i in (0..n).rev() {
        let c_in = if i == 0 {
            QubitWire::new(carry_reg.clone(), 0)
        } else {
            QubitWire::new(a_reg.to_owned(), i - 1)
        };
        let b_i = QubitWire::new(b_reg.to_owned(), i);
        let a_i = QubitWire::new(a_reg.to_owned(), i);

        // UMA(c_in, b_i, a_i): CCX(c_in, b_i, a_i); CX(a_i, c_in); CX(c_in, b_i)
        action_uma.push(MirStmt::Gate {
            kind: MirGateKind::X,
            controls: vec![c_in.clone(), b_i.clone()],
            targets: vec![a_i.clone()],
        });
        action_uma.push(MirStmt::Gate {
            kind: MirGateKind::X,
            controls: vec![a_i],
            targets: vec![c_in.clone()],
        });
        action_uma.push(MirStmt::Gate {
            kind: MirGateKind::X,
            controls: vec![c_in],
            targets: vec![b_i],
        });
    }

    stmts.extend(compute_maj);
    stmts.extend(action_uma);
    stmts.push(MirStmt::QFree {
        reg: carry_reg,
        size: 1,
    });
    stmts
}

/// Synthesizes Draper (2000) Quantum Fourier Transform (QFT) adder (`b = a + b mod 2^n`).
/// Wraps $QFT(b)$ and $QFT^\dagger(b)$ in a `MirStmt::Conjugate` node so `ctrl(c)` only controls
/// the conditional phase rotations in the middle!
pub fn synthesize_draper_qft_adder(a_reg: &str, b_reg: &str, width: usize) -> Vec<MirStmt> {
    let n = width.max(1);
    let mut qft_compute = Vec::new();
    for i in 0..n {
        let b_i = QubitWire::new(b_reg.to_owned(), i);
        qft_compute.push(MirStmt::Gate {
            kind: MirGateKind::H,
            controls: vec![],
            targets: vec![b_i.clone()],
        });
        for j in (i + 1)..n {
            let theta = std::f64::consts::PI / (1usize << (j - i)) as f64;
            let b_j = QubitWire::new(b_reg.to_owned(), j);
            qft_compute.push(MirStmt::Gate {
                kind: MirGateKind::Phase(theta),
                controls: vec![b_j],
                targets: vec![b_i.clone()],
            });
        }
    }

    let mut phase_action = Vec::new();
    for i in 0..n {
        let b_i = QubitWire::new(b_reg.to_owned(), i);
        for j in 0..=(n - 1 - i) {
            let theta = std::f64::consts::PI / (1usize << j) as f64;
            let a_idx = i + j;
            if a_idx < n {
                let a_w = QubitWire::new(a_reg.to_owned(), a_idx);
                phase_action.push(MirStmt::Gate {
                    kind: MirGateKind::Phase(theta),
                    controls: vec![a_w],
                    targets: vec![b_i.clone()],
                });
            }
        }
    }

    vec![MirStmt::Conjugate {
        compute: qft_compute,
        action: phase_action,
        target: None,
    }]
}

fn cancel_inverses_recursive(stmts: Vec<MirStmt>) -> Vec<MirStmt> {
    let mapped: Vec<MirStmt> = stmts
        .into_iter()
        .map(|s| match s {
            MirStmt::Conjugate {
                compute,
                action,
                target,
            } => MirStmt::Conjugate {
                compute: cancel_inverses_recursive(compute),
                action: cancel_inverses_recursive(action),
                target,
            },
            MirStmt::ClassicalIf {
                cond_var,
                then_body,
                else_body,
            } => MirStmt::ClassicalIf {
                cond_var,
                then_body: cancel_inverses_recursive(then_body),
                else_body: cancel_inverses_recursive(else_body),
            },
            MirStmt::While { cond_var, body } => MirStmt::While {
                cond_var,
                body: cancel_inverses_recursive(body),
            },
            other => other,
        })
        .collect();
    cancel_inverses_peephole(mapped)
}

fn gates_disjoint(
    c1: &[QubitWire],
    t1: &[QubitWire],
    c2: &[QubitWire],
    t2: &[QubitWire],
) -> bool {
    let iter1 = c1.iter().chain(t1.iter());
    for w1 in iter1 {
        if c2.contains(w1) || t2.contains(w1) {
            return false;
        }
    }
    true
}

fn try_merge_rotations(k1: &MirGateKind, k2: &MirGateKind) -> Option<Option<MirGateKind>> {
    match (k1, k2) {
        (MirGateKind::Rx(a), MirGateKind::Rx(b)) => {
            let sum = a + b;
            Some(if sum.abs() < 1e-12 {
                None
            } else {
                Some(MirGateKind::Rx(sum))
            })
        }
        (MirGateKind::Ry(a), MirGateKind::Ry(b)) => {
            let sum = a + b;
            Some(if sum.abs() < 1e-12 {
                None
            } else {
                Some(MirGateKind::Ry(sum))
            })
        }
        (MirGateKind::Rz(a), MirGateKind::Rz(b)) => {
            let sum = a + b;
            Some(if sum.abs() < 1e-12 {
                None
            } else {
                Some(MirGateKind::Rz(sum))
            })
        }
        (MirGateKind::Phase(a), MirGateKind::Phase(b)) => {
            let sum = a + b;
            Some(if sum.abs() < 1e-12 {
                None
            } else {
                Some(MirGateKind::Phase(sum))
            })
        }
        _ => None,
    }
}

fn cancel_inverses_peephole(stmts: Vec<MirStmt>) -> Vec<MirStmt> {
    let mut stack: Vec<MirStmt> = Vec::with_capacity(stmts.len());
    for stmt in stmts {
        if let MirStmt::Gate {
            kind: ref k2,
            controls: ref c2,
            targets: ref t2,
        } = stmt
        {
            // Scan backwards through `stack` past any gates acting on completely disjoint wires
            let mut match_idx = None;
            let mut merged_replacement = None;
            for idx in (0..stack.len()).rev() {
                match &stack[idx] {
                    MirStmt::Gate {
                        kind: k1,
                        controls: c1,
                        targets: t1,
                    } => {
                        if c1 == c2 && t1 == t2 {
                            if k1.is_inverse_of(k2) {
                                match_idx = Some(idx);
                                break;
                            } else if let Some(merged) = try_merge_rotations(k1, k2) {
                                match_idx = Some(idx);
                                merged_replacement = merged;
                                break;
                            } else {
                                break;
                            }
                        } else if !gates_disjoint(c1, t1, c2, t2) {
                            break;
                        }
                    }
                    _ => break,
                }
            }
            if let Some(idx) = match_idx {
                if let Some(new_kind) = merged_replacement {
                    stack[idx] = MirStmt::Gate {
                        kind: new_kind,
                        controls: c2.clone(),
                        targets: t2.clone(),
                    };
                } else {
                    stack.remove(idx);
                }
                continue;
            }
        }
        stack.push(stmt);
    }
    stack
}

/// Computes static quantum resource estimates (`qgo estimate`) across a `MirModule`.
pub fn estimate_resources(module: &MirModule) -> ResourceReport {
    let mut report = ResourceReport::default();
    for func in &module.funcs {
        for p in &func.params {
            if p.is_quantum {
                report.logical_qubits += p.width;
            }
        }
        let flat = flatten_conjugates(&func.body);
        let mut live_anc = 0usize;
        let mut peak_anc = 0usize;
        let mut total_anc_allocs = 0usize;
        for s in &flat {
            accumulate_stmt_resources(
                s,
                &mut report,
                &mut live_anc,
                &mut peak_anc,
                &mut total_anc_allocs,
            );
        }
        report.ancilla_qubits += peak_anc;
        report.recycled_ancillas += total_anc_allocs.saturating_sub(peak_anc);
    }
    report
}

fn accumulate_stmt_resources(
    stmt: &MirStmt,
    report: &mut ResourceReport,
    live_anc: &mut usize,
    peak_anc: &mut usize,
    total_anc_allocs: &mut usize,
) {
    match stmt {
        MirStmt::QAlloc {
            size, is_ancilla, ..
        } => {
            if *is_ancilla {
                *total_anc_allocs += *size;
                *live_anc += *size;
                *peak_anc = (*peak_anc).max(*live_anc);
            } else {
                report.logical_qubits += *size;
            }
        }
        MirStmt::QFree { size, .. } => {
            *live_anc = live_anc.saturating_sub(*size);
        }
        MirStmt::Gate {
            kind,
            controls,
            targets,
        } => {
            report.circuit_depth += 1;
            let total_wires = controls.len() + targets.len();
            if controls.len() >= 2 {
                report.toffoli_gates += 1;
                // Standard Clifford+T decomposition of N-controlled Toffoli uses 7 T-gates for CCX
                report.t_count += 7 * (controls.len() - 1);
            } else if total_wires == 2 {
                report.two_qubit_gates += 1;
            } else {
                report.single_qubit_gates += 1;
                if matches!(kind, MirGateKind::T | MirGateKind::Tdg) {
                    report.t_count += 1;
                }
            }
        }
        MirStmt::ClassicalIf {
            then_body,
            else_body,
            ..
        } => {
            for s in then_body {
                accumulate_stmt_resources(s, report, live_anc, peak_anc, total_anc_allocs);
            }
            for s in else_body {
                accumulate_stmt_resources(s, report, live_anc, peak_anc, total_anc_allocs);
            }
        }
        MirStmt::While { body, .. } => {
            for s in body {
                accumulate_stmt_resources(s, report, live_anc, peak_anc, total_anc_allocs);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qgo_hir::check_module;
    use qgo_mir::lower_to_mir;
    use qgo_span::FileId;
    use qgo_syntax::parse_source;

    #[test]
    fn test_ctrl_conjugation_leaves_compute_uncontrolled() {
        let src = r#"package main
func Compute(x qubit, anc qubit) qfree {
    CX(x, anc)
}
func ControlledConj(c qubit, x qubit, target qubit) unitary {
    anc := qalloc()
    Compute(x, anc)
    defer adj Compute(x, anc) -> anc
    CX(anc, target)
}
"#;
        let (ast, _) = parse_source(FileId(0), src);
        let (hir, diags) = check_module(&ast);
        assert!(diags.is_empty());
        let mir = lower_to_mir(&hir);
        let func = mir.funcs.iter().find(|f| f.name == "ControlledConj").expect("func");

        // Verify a Conjugate node was synthesized
        let conj = func
            .body
            .iter()
            .find(|s| matches!(s, MirStmt::Conjugate { .. }))
            .expect("Conjugate node exists")
            .clone();

        // Apply `ctrl(c)` to the Conjugate node
        let c_wire = QubitWire::new("c", 0);
        let controlled = apply_controls_to_stmt(conj, std::slice::from_ref(&c_wire));

        if let MirStmt::Conjugate {
            compute, action, ..
        } = controlled
        {
            // `compute` (U) must NOT contain control wire `c`!
            for s in &compute {
                if let MirStmt::Gate { controls, .. } = s {
                    assert!(
                        !controls.contains(&c_wire),
                        "compute block must remain uncontrolled under ctrl-conjugation theorem!"
                    );
                }
            }
            // `action` (V) MUST contain control wire `c`!
            for s in &action {
                if let MirStmt::Gate { controls, .. } = s {
                    assert!(
                        controls.contains(&c_wire),
                        "action block must receive control wire c!"
                    );
                }
            }
        } else {
            panic!("expected Conjugate node");
        }
    }

    #[test]
    fn test_inverse_gate_cancellation() {
        let w = QubitWire::new("q", 0);
        let stmts = vec![
            MirStmt::Gate {
                kind: MirGateKind::H,
                controls: vec![],
                targets: vec![w.clone()],
            },
            MirStmt::Gate {
                kind: MirGateKind::H,
                controls: vec![],
                targets: vec![w.clone()],
            },
            MirStmt::Gate {
                kind: MirGateKind::S,
                controls: vec![],
                targets: vec![w.clone()],
            },
            MirStmt::Gate {
                kind: MirGateKind::Sdg,
                controls: vec![],
                targets: vec![w],
            },
        ];
        let opt = cancel_inverses_peephole(stmts);
        assert!(opt.is_empty(), "H*H and S*Sdg should cancel completely");
    }

    #[test]
    fn test_commutation_aware_slice_broadcast_cancellation_and_rotation_merge() {
        let src = r#"package main
func BroadcastCancel(reg qreg[4], q qubit) unitary {
    H(reg[:])
    H(reg[:])
    Rz(0.5, q)
    Rz(0.25, q)
    Rz(-0.75, q)
}
"#;
        let (ast, _) = parse_source(FileId(0), src);
        let (hir, diags) = check_module(&ast);
        assert!(diags.is_empty());
        let mut mir = lower_to_mir(&hir);
        optimize_module(&mut mir, SynthMode::Width);
        let flat = flatten_conjugates(&mir.funcs[0].body);
        assert!(
            flat.is_empty(),
            "H(reg[:]); H(reg[:]) and Rz(0.5)*Rz(0.25)*Rz(-0.75) should cancel to 0 gates, got {:?}",
            flat
        );
    }

    #[test]
    fn test_physical_ancilla_pool_recycling() {
        let src = r#"package main
func OracleA(x qreg[2], anc qubit) qfree {
    CCX(x[0], x[1], anc)
}
func OracleB(x qreg[2], anc qubit) qfree {
    CX(x[0], anc)
}
func SequentialAncillas(x qreg[2]) unitary {
    a1 := qalloc()
    OracleA(x, a1)
    defer adj OracleA(x, a1) -> a1
    Z(a1)

    a2 := qalloc()
    OracleB(x, a2)
    defer adj OracleB(x, a2) -> a2
    Z(a2)

    a3 := qalloc()
    OracleA(x, a3)
    defer adj OracleA(x, a3) -> a3
    Z(a3)
}
"#;
        let (ast, _) = parse_source(FileId(0), src);
        let (hir, diags) = check_module(&ast);
        assert!(diags.is_empty());
        let mut mir = lower_to_mir(&hir);
        optimize_module(&mut mir, SynthMode::Width);

        let res = estimate_resources(&mir);
        // Only 1 peak physical ancilla qubit is needed across all 3 sequential conjugations,
        // with 2 ancilla allocations recycled from the |0> pool!
        assert_eq!(res.ancilla_qubits, 1);
        assert_eq!(res.recycled_ancillas, 2);
    }
}
