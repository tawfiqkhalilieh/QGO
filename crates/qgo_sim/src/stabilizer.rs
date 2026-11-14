//! Aaronson-Gottesman CHP Stabilizer Tableau Simulator (`StabilizerSim`).
//! Simulates Clifford circuits (`H`, `S`, `Sdg`, `X`, `Y`, `Z`, `CX`, `Swap`, `Measure`, `Reset`)
//! in $O(n^2)$ time using a $2n \times (2n + 1)$ binary symplectic tableau.

use std::collections::HashMap;
use qgo_mir::{MirFunc, MirGateKind, MirStmt, QubitWire};
use qgo_opt::flatten_conjugates;

#[derive(Debug, Clone)]
pub struct StabilizerSim {
    pub num_qubits: usize,
    /// `2 * n` rows of `n` X-bits. Rows `0..n` are destabilizers; rows `n..2n` are stabilizers.
    pub x: Vec<Vec<bool>>,
    /// `2 * n` rows of `n` Z-bits.
    pub z: Vec<Vec<bool>>,
    /// `2 * n` phase bits (`false` = +1, `true` = -1).
    pub r: Vec<bool>,
    pub wire_map: HashMap<QubitWire, usize>,
    pub classical_vars: HashMap<String, i64>,
    pub ancilla_wires: Vec<QubitWire>,
    pub free_qubit_pool: Vec<usize>,
    pub recycled_qubit_count: usize,
}

impl StabilizerSim {
    pub fn new() -> Self {
        Self {
            num_qubits: 0,
            x: Vec::new(),
            z: Vec::new(),
            r: Vec::new(),
            wire_map: HashMap::new(),
            classical_vars: HashMap::new(),
            ancilla_wires: Vec::new(),
            free_qubit_pool: Vec::new(),
            recycled_qubit_count: 0,
        }
    }

    /// Allocates a new qubit wire initialized to $|0\rangle$ in the symplectic tableau,
    /// recycling a released physical qubit index from `free_qubit_pool` when available.
    pub fn allocate_wire(&mut self, wire: QubitWire, is_ancilla: bool) -> usize {
        if let Some(&idx) = self.wire_map.get(&wire) {
            return idx;
        }
        if is_ancilla && !self.ancilla_wires.contains(&wire) {
            self.ancilla_wires.push(wire.clone());
        }
        if let Some(recycled_idx) = self.free_qubit_pool.pop() {
            self.wire_map.insert(wire, recycled_idx);
            self.recycled_qubit_count += 1;
            return recycled_idx;
        }
        let old_n = self.num_qubits;
        let new_n = old_n + 1;
        self.num_qubits = new_n;
        self.wire_map.insert(wire.clone(), old_n);

        // Expand existing rows by 1 column (false)
        for row in &mut self.x {
            row.push(false);
        }
        for row in &mut self.z {
            row.push(false);
        }

        // Insert new destabilizer row at `old_n` (with X_{old_n} = true)
        let mut new_destab_x = vec![false; new_n];
        new_destab_x[old_n] = true;
        let new_destab_z = vec![false; new_n];
        self.x.insert(old_n, new_destab_x);
        self.z.insert(old_n, new_destab_z);
        self.r.insert(old_n, false);

        // Append new stabilizer row at `2 * new_n - 1` (with Z_{old_n} = true)
        let new_stab_x = vec![false; new_n];
        let mut new_stab_z = vec![false; new_n];
        new_stab_z[old_n] = true;
        self.x.push(new_stab_x);
        self.z.push(new_stab_z);
        self.r.push(false);

        old_n
    }

    pub fn execute_func(&mut self, func: &MirFunc) {
        for p in &func.params {
            if p.is_quantum {
                for b in 0..p.width.max(1) {
                    self.allocate_wire(QubitWire::new(p.name.clone(), b), false);
                }
            }
        }
        let flat = flatten_conjugates(&func.body);
        self.execute_stmts(&flat);
    }

    /// Returns `true` if all gates in `func` (after flattening conjugates) belong to the Clifford group.
    pub fn is_clifford_func(func: &MirFunc) -> bool {
        let flat = flatten_conjugates(&func.body);
        Self::are_stmts_clifford(&flat)
    }

    fn are_stmts_clifford(stmts: &[MirStmt]) -> bool {
        for stmt in stmts {
            match stmt {
                MirStmt::Gate { kind, controls, .. } => {
                    let ok = match (kind, controls.len()) {
                        (
                            MirGateKind::H
                            | MirGateKind::S
                            | MirGateKind::Sdg
                            | MirGateKind::X
                            | MirGateKind::Y
                            | MirGateKind::Z
                            | MirGateKind::Swap,
                            0,
                        ) => true,
                        (MirGateKind::X | MirGateKind::Z, 1) => true,
                        _ => false,
                    };
                    if !ok {
                        return false;
                    }
                }
                MirStmt::ClassicalIf {
                    then_body,
                    else_body,
                    ..
                } => {
                    if !Self::are_stmts_clifford(then_body) || !Self::are_stmts_clifford(else_body)
                    {
                        return false;
                    }
                }
                MirStmt::While { body, .. } => {
                    if !Self::are_stmts_clifford(body) {
                        return false;
                    }
                }
                _ => {}
            }
        }
        true
    }

    pub fn execute_stmts(&mut self, stmts: &[MirStmt]) {
        for stmt in stmts {
            match stmt {
                MirStmt::QAlloc {
                    reg,
                    size,
                    is_ancilla,
                } => {
                    for b in 0..(*size).max(1) {
                        self.allocate_wire(QubitWire::new(reg.clone(), b), *is_ancilla);
                    }
                }
                MirStmt::QFree { reg, size } => {
                    for b in 0..*size {
                        let w = QubitWire::new(reg.clone(), b);
                        let p0 = self.prob_zero(&w);
                        assert!(
                            (1.0 - p0).abs() < 1e-12,
                            "StabilizerSim: ancilla wire {} not uncomputed to |0>! P(0) = {}",
                            w,
                            p0
                        );
                        if let Some(idx) = self.wire_map.remove(&w) {
                            if !self.free_qubit_pool.contains(&idx) {
                                self.free_qubit_pool.push(idx);
                            }
                        }
                    }
                }
                MirStmt::Gate {
                    kind,
                    controls,
                    targets,
                } => {
                    for c in controls {
                        self.allocate_wire(c.clone(), false);
                    }
                    for t in targets {
                        self.allocate_wire(t.clone(), false);
                    }
                    self.apply_gate(kind, controls, targets);
                }
                MirStmt::Measure { qubit, dst_var } => {
                    self.allocate_wire(qubit.clone(), false);
                    let outcome = self.measure(qubit);
                    self.classical_vars.insert(dst_var.clone(), outcome as i64);
                }
                MirStmt::Reset { qubit } => {
                    self.allocate_wire(qubit.clone(), false);
                    self.reset(qubit);
                }
                MirStmt::ClassicalIf {
                    cond_var,
                    then_body,
                    else_body,
                } => {
                    let v = self.classical_vars.get(cond_var).copied().unwrap_or(0);
                    if v != 0 {
                        self.execute_stmts(then_body);
                    } else {
                        self.execute_stmts(else_body);
                    }
                }
                MirStmt::While { cond_var, body } => {
                    let mut guard = 0;
                    while self.classical_vars.get(cond_var).copied().unwrap_or(0) != 0
                        && guard < 64
                    {
                        self.execute_stmts(body);
                        guard += 1;
                    }
                }
                _ => {}
            }
        }
    }

    pub fn h(&mut self, a: usize) {
        let rows = 2 * self.num_qubits;
        for i in 0..rows {
            let xa = self.x[i][a];
            let za = self.z[i][a];
            self.r[i] ^= xa & za;
            self.x[i][a] = za;
            self.z[i][a] = xa;
        }
    }

    pub fn s(&mut self, a: usize) {
        let rows = 2 * self.num_qubits;
        for i in 0..rows {
            let xa = self.x[i][a];
            let za = self.z[i][a];
            self.r[i] ^= xa & za;
            self.z[i][a] ^= xa;
        }
    }

    pub fn sdg(&mut self, a: usize) {
        self.s(a);
        self.s(a);
        self.s(a);
    }

    pub fn x_gate(&mut self, a: usize) {
        let rows = 2 * self.num_qubits;
        for i in 0..rows {
            self.r[i] ^= self.z[i][a];
        }
    }

    pub fn z_gate(&mut self, a: usize) {
        let rows = 2 * self.num_qubits;
        for i in 0..rows {
            self.r[i] ^= self.x[i][a];
        }
    }

    pub fn y_gate(&mut self, a: usize) {
        let rows = 2 * self.num_qubits;
        for i in 0..rows {
            self.r[i] ^= self.x[i][a] ^ self.z[i][a];
        }
    }

    pub fn cx(&mut self, a: usize, b: usize) {
        if a == b {
            return;
        }
        let rows = 2 * self.num_qubits;
        for i in 0..rows {
            let xa = self.x[i][a];
            let za = self.z[i][a];
            let xb = self.x[i][b];
            let zb = self.z[i][b];
            self.r[i] ^= xa & zb & (xb ^ za ^ true);
            self.x[i][b] ^= xa;
            self.z[i][a] ^= zb;
        }
    }

    pub fn swap(&mut self, a: usize, b: usize) {
        if a == b {
            return;
        }
        self.cx(a, b);
        self.cx(b, a);
        self.cx(a, b);
    }

    pub fn apply_gate(
        &mut self,
        kind: &MirGateKind,
        controls: &[QubitWire],
        targets: &[QubitWire],
    ) {
        if matches!(kind, MirGateKind::Swap) && targets.len() == 2 && controls.is_empty() {
            let t0 = self.wire_map[&targets[0]];
            let t1 = self.wire_map[&targets[1]];
            self.swap(t0, t1);
            return;
        }
        let Some(tw) = targets.first() else { return };
        let t = self.wire_map[tw];

        match (kind, controls) {
            (MirGateKind::H, []) => self.h(t),
            (MirGateKind::S, []) => self.s(t),
            (MirGateKind::Sdg, []) => self.sdg(t),
            (MirGateKind::X, []) => self.x_gate(t),
            (MirGateKind::Y, []) => self.y_gate(t),
            (MirGateKind::Z, []) => self.z_gate(t),
            (MirGateKind::X, [c0]) => {
                let c = self.wire_map[c0];
                self.cx(c, t);
            }
            (MirGateKind::Z, [c0]) => {
                // CZ(c, t) = H(t); CX(c, t); H(t)
                let c = self.wire_map[c0];
                self.h(t);
                self.cx(c, t);
                self.h(t);
            }
            _ => {}
        }
    }

    fn pauli_phase_exp(x1: bool, z1: bool, x2: bool, z2: bool) -> i32 {
        match (x1, z1) {
            (false, false) => 0,
            (true, true) => (z2 as i32) - (x2 as i32),
            (true, false) => (z2 as i32) * (2 * (x2 as i32) - 1),
            (false, true) => (x2 as i32) * (1 - 2 * (z2 as i32)),
        }
    }

    fn row_mult(
        n: usize,
        xh: &mut [bool],
        zh: &mut [bool],
        rh: &mut bool,
        xi: &[bool],
        zi: &[bool],
        ri: bool,
    ) {
        let mut exp_sum = 2 * (*rh as i32) + 2 * (ri as i32);
        for j in 0..n {
            exp_sum += Self::pauli_phase_exp(xi[j], zi[j], xh[j], zh[j]);
            xh[j] ^= xi[j];
            zh[j] ^= zi[j];
        }
        let m = exp_sum.rem_euclid(4);
        *rh = m == 2;
    }

    fn rowsum(&mut self, h: usize, i: usize) {
        let n = self.num_qubits;
        let xi = self.x[i].clone();
        let zi = self.z[i].clone();
        let ri = self.r[i];
        Self::row_mult(
            n,
            &mut self.x[h],
            &mut self.z[h],
            &mut self.r[h],
            &xi,
            &zi,
            ri,
        );
    }

    /// Returns $P(q = |0\rangle) \in \{0.0, 0.5, 1.0\}$ for a stabilizer state.
    pub fn prob_zero(&self, wire: &QubitWire) -> f64 {
        let Some(&a) = self.wire_map.get(wire) else {
            return 1.0;
        };
        let n = self.num_qubits;
        // Check if any stabilizer generator anticommutes with Z_a (i.e. has x[p][a] == true)
        if (n..2 * n).any(|p| self.x[p][a]) {
            return 0.5;
        }
        // Deterministic outcome: multiply stabilizers k + n where destabilizer k has x[k][a] == true
        let mut sx = vec![false; n];
        let mut sz = vec![false; n];
        let mut sr = false;
        for k in 0..n {
            if self.x[k][a] {
                Self::row_mult(
                    n,
                    &mut sx,
                    &mut sz,
                    &mut sr,
                    &self.x[k + n],
                    &self.z[k + n],
                    self.r[k + n],
                );
            }
        }
        if sr {
            0.0
        } else {
            1.0
        }
    }

    /// Projective computational-basis measurement on `wire` (deterministic 0-branch on 50/50 split).
    pub fn measure(&mut self, wire: &QubitWire) -> u8 {
        let a = self.wire_map[wire];
        let n = self.num_qubits;
        if let Some(p) = (n..2 * n).find(|&row| self.x[row][a]) {
            // Random outcome: update tableau according to Aaronson-Gottesman CHP algorithm
            for i in 0..2 * n {
                if i != p && self.x[i][a] {
                    self.rowsum(i, p);
                }
            }
            self.x[p - n] = self.x[p].clone();
            self.z[p - n] = self.z[p].clone();
            self.r[p - n] = self.r[p];
            self.x[p] = vec![false; n];
            self.z[p] = vec![false; n];
            self.z[p][a] = true;
            self.r[p] = false; // deterministic 0-branch
            0
        } else {
            let p0 = self.prob_zero(wire);
            if p0 >= 0.5 {
                0
            } else {
                1
            }
        }
    }

    pub fn reset(&mut self, wire: &QubitWire) {
        let outcome = self.measure(wire);
        if outcome == 1 {
            let a = self.wire_map[wire];
            self.x_gate(a);
        }
    }
}

impl Default for StabilizerSim {
    fn default() -> Self {
        Self::new()
    }
}
