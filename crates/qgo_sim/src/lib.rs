//! `qgo_sim`: Built-in exact complex statevector and Clifford/stabilizer simulator for `qgo test` and `qgo run`.
//! Supports end-to-end verification of `MirFunc` circuits, including verifying that `defer adj`
//! uncomputes ancilla qubits back to exact state $|0\rangle$ with probability $1.0 - \epsilon$ ($\epsilon < 10^{-12}$).

use std::collections::HashMap;
use qgo_mir::{MirFunc, MirGateKind, MirStmt, QubitWire};
use qgo_opt::flatten_conjugates;

pub mod stabilizer;
pub mod viz;
pub mod vm;

pub use stabilizer::StabilizerSim;
pub use viz::{
    render_trace_gif, trace_func_memory, ConjStage, MemorySnapshot, MemoryTrace, WireMemorySlot,
};
pub use vm::{QGoVM, VmVal};

/// Double-precision complex amplitude $a + b i$.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Complex {
    pub re: f64,
    pub im: f64,
}

impl Complex {
    pub const ZERO: Self = Self { re: 0.0, im: 0.0 };
    pub const ONE: Self = Self { re: 1.0, im: 0.0 };
    pub const I: Self = Self { re: 0.0, im: 1.0 };

    pub fn new(re: f64, im: f64) -> Self {
        Self { re, im }
    }

    pub fn from_polar(r: f64, theta: f64) -> Self {
        Self {
            re: r * theta.cos(),
            im: r * theta.sin(),
        }
    }

    pub fn norm_sqr(self) -> f64 {
        self.re * self.re + self.im * self.im
    }

    pub fn conj(self) -> Self {
        Self {
            re: self.re,
            im: -self.im,
        }
    }
}

impl std::ops::Add for Complex {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        Self::new(self.re + rhs.re, self.im + rhs.im)
    }
}

impl std::ops::Sub for Complex {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        Self::new(self.re - rhs.re, self.im - rhs.im)
    }
}

impl std::ops::Mul for Complex {
    type Output = Self;
    fn mul(self, rhs: Self) -> Self {
        Self::new(
            self.re * rhs.re - self.im * rhs.im,
            self.re * rhs.im + self.im * rhs.re,
        )
    }
}

impl std::ops::Mul<f64> for Complex {
    type Output = Self;
    fn mul(self, rhs: f64) -> Self {
        Self::new(self.re * rhs, self.im * rhs)
    }
}

/// Exact $2^N$-amplitude statevector simulator.
#[derive(Debug, Clone)]
pub struct StatevectorSim {
    pub num_qubits: usize,
    pub state: Vec<Complex>,
    pub wire_map: HashMap<QubitWire, usize>,
    pub classical_vars: HashMap<String, i64>,
    pub ancilla_wires: Vec<QubitWire>,
}

impl StatevectorSim {
    pub fn new() -> Self {
        Self {
            num_qubits: 0,
            state: vec![Complex::ONE],
            wire_map: HashMap::new(),
            classical_vars: HashMap::new(),
            ancilla_wires: Vec::new(),
        }
    }

    /// Allocates a new qubit wire initialized to $|0\rangle$, expanding the statevector tensor product.
    pub fn allocate_wire(&mut self, wire: QubitWire, is_ancilla: bool) -> usize {
        if let Some(&idx) = self.wire_map.get(&wire) {
            return idx;
        }
        let idx = self.num_qubits;
        self.num_qubits += 1;
        self.wire_map.insert(wire.clone(), idx);
        if is_ancilla {
            self.ancilla_wires.push(wire);
        }
        // Tensor product |psi> (x) |0>: new bit `idx` is 0 for existing amplitudes, 1 for upper half
        let old_len = self.state.len();
        self.state.resize(old_len * 2, Complex::ZERO);
        idx
    }

    /// Prepares a register in a classical unsigned integer basis state $|val\rangle$.
    pub fn set_register_uint(&mut self, reg: &str, width: usize, val: usize) {
        for b in 0..width {
            let w = QubitWire::new(reg.to_owned(), b);
            self.allocate_wire(w.clone(), false);
            if ((val >> b) & 1) == 1 {
                self.apply_gate(&MirGateKind::X, &[], &[w]);
            }
        }
    }

    /// Executes a [`MirFunc`] on this simulator instance.
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
                    // Verify uncomputed ancilla is in state |0> within tolerance 1e-12
                    for b in 0..*size {
                        let w = QubitWire::new(reg.clone(), b);
                        let p0 = self.prob_zero(&w);
                        debug_assert!(
                            (1.0 - p0).abs() < 1e-12,
                            "Ancilla wire {} was not uncomputed to |0>! P(0) = {}",
                            w,
                            p0
                        );
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
                    let outcome = self.measure_deterministic_max_prob(qubit);
                    self.classical_vars.insert(dst_var.clone(), outcome as i64);
                }
                MirStmt::Reset { qubit } => {
                    self.allocate_wire(qubit.clone(), false);
                    self.reset_wire(qubit);
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

    pub fn apply_gate(
        &mut self,
        kind: &MirGateKind,
        controls: &[QubitWire],
        targets: &[QubitWire],
    ) {
        let mut ctrl_mask = 0usize;
        for c in controls {
            if let Some(&idx) = self.wire_map.get(c) {
                ctrl_mask |= 1usize << idx;
            }
        }

        if matches!(kind, MirGateKind::Swap) && targets.len() == 2 {
            let t0 = self.wire_map[&targets[0]];
            let t1 = self.wire_map[&targets[1]];
            let dim = 1usize << self.num_qubits;
            for basis in 0..dim {
                if (basis & ctrl_mask) == ctrl_mask {
                    let b0 = (basis >> t0) & 1;
                    let b1 = (basis >> t1) & 1;
                    if b0 == 0 && b1 == 1 {
                        let partner = (basis | (1 << t0)) & !(1 << t1);
                        self.state.swap(basis, partner);
                    }
                }
            }
            return;
        }

        let Some(target_wire) = targets.first() else {
            return;
        };
        let t_idx = self.wire_map[target_wire];
        let t_bit = 1usize << t_idx;

        let s2 = std::f64::consts::FRAC_1_SQRT_2;
        let (u00, u01, u10, u11) = match kind {
            MirGateKind::X => (Complex::ZERO, Complex::ONE, Complex::ONE, Complex::ZERO),
            MirGateKind::Y => (
                Complex::ZERO,
                Complex::new(0.0, -1.0),
                Complex::I,
                Complex::ZERO,
            ),
            MirGateKind::Z => (
                Complex::ONE,
                Complex::ZERO,
                Complex::ZERO,
                Complex::new(-1.0, 0.0),
            ),
            MirGateKind::H => (
                Complex::new(s2, 0.0),
                Complex::new(s2, 0.0),
                Complex::new(s2, 0.0),
                Complex::new(-s2, 0.0),
            ),
            MirGateKind::S => (Complex::ONE, Complex::ZERO, Complex::ZERO, Complex::I),
            MirGateKind::Sdg => (
                Complex::ONE,
                Complex::ZERO,
                Complex::ZERO,
                Complex::new(0.0, -1.0),
            ),
            MirGateKind::T => (
                Complex::ONE,
                Complex::ZERO,
                Complex::ZERO,
                Complex::from_polar(1.0, std::f64::consts::FRAC_PI_4),
            ),
            MirGateKind::Tdg => (
                Complex::ONE,
                Complex::ZERO,
                Complex::ZERO,
                Complex::from_polar(1.0, -std::f64::consts::FRAC_PI_4),
            ),
            MirGateKind::Rx(theta) => {
                let c = (theta / 2.0).cos();
                let s = (theta / 2.0).sin();
                (
                    Complex::new(c, 0.0),
                    Complex::new(0.0, -s),
                    Complex::new(0.0, -s),
                    Complex::new(c, 0.0),
                )
            }
            MirGateKind::Ry(theta) => {
                let c = (theta / 2.0).cos();
                let s = (theta / 2.0).sin();
                (
                    Complex::new(c, 0.0),
                    Complex::new(-s, 0.0),
                    Complex::new(s, 0.0),
                    Complex::new(c, 0.0),
                )
            }
            MirGateKind::Rz(theta) => (
                Complex::from_polar(1.0, -theta / 2.0),
                Complex::ZERO,
                Complex::ZERO,
                Complex::from_polar(1.0, theta / 2.0),
            ),
            MirGateKind::Phase(theta) => (
                Complex::ONE,
                Complex::ZERO,
                Complex::ZERO,
                Complex::from_polar(1.0, *theta),
            ),
            MirGateKind::Swap => return,
        };

        let dim = 1usize << self.num_qubits;
        for i0 in 0..dim {
            if (i0 & t_bit) == 0 && (i0 & ctrl_mask) == ctrl_mask {
                let i1 = i0 | t_bit;
                let a0 = self.state[i0];
                let a1 = self.state[i1];
                self.state[i0] = u00 * a0 + u01 * a1;
                self.state[i1] = u10 * a0 + u11 * a1;
            }
        }
    }

    /// Returns the marginal probability $P(q = |0\rangle)$ for a specific [`QubitWire`].
    pub fn prob_zero(&self, wire: &QubitWire) -> f64 {
        let Some(&idx) = self.wire_map.get(wire) else {
            return 1.0;
        };
        let mask = 1usize << idx;
        let mut p0 = 0.0;
        for (basis, amp) in self.state.iter().enumerate() {
            if (basis & mask) == 0 {
                p0 += amp.norm_sqr();
            }
        }
        p0
    }

    /// Reads the most probable unsigned integer value of a register `reg[0..width]`.
    pub fn measure_register_uint(&self, reg: &str, width: usize) -> usize {
        let wires: Vec<QubitWire> = (0..width)
            .map(|b| QubitWire::new(reg.to_owned(), b))
            .collect();
        self.peek_wires_uint(&wires)
    }

    /// Reads the most probable unsigned integer value across an arbitrary slice of [`QubitWire`]s.
    pub fn peek_wires_uint(&self, wires: &[QubitWire]) -> usize {
        let (best_basis, _) = self
            .state
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| {
                a.norm_sqr()
                    .partial_cmp(&b.norm_sqr())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .unwrap_or((0, &Complex::ONE));
        let mut val = 0usize;
        for (b, w) in wires.iter().enumerate() {
            if let Some(&idx) = self.wire_map.get(w) {
                if ((best_basis >> idx) & 1) == 1 {
                    val |= 1 << b;
                }
            }
        }
        val
    }

    /// Returns the marginal probability that `wires` are in computational basis state `expected_val`.
    pub fn prob_wires_basis(&self, wires: &[QubitWire], expected_val: usize) -> f64 {
        let mut mask = 0usize;
        let mut target_bits = 0usize;
        for (b, w) in wires.iter().enumerate() {
            if let Some(&idx) = self.wire_map.get(w) {
                mask |= 1usize << idx;
                if ((expected_val >> b) & 1) == 1 {
                    target_bits |= 1usize << idx;
                }
            }
        }
        let mut prob = 0.0;
        for (basis, amp) in self.state.iter().enumerate() {
            if (basis & mask) == target_bits {
                prob += amp.norm_sqr();
            }
        }
        prob
    }

    pub fn measure_wire(&mut self, wire: &QubitWire) -> u8 {
        self.allocate_wire(wire.clone(), false);
        self.measure_deterministic_max_prob(wire)
    }

    pub fn reset_wire(&mut self, wire: &QubitWire) {
        self.allocate_wire(wire.clone(), false);
        let outcome = self.measure_deterministic_max_prob(wire);
        if outcome == 1 {
            self.apply_gate(&MirGateKind::X, &[], std::slice::from_ref(wire));
        }
    }

    fn measure_deterministic_max_prob(&mut self, wire: &QubitWire) -> u8 {
        let p0 = self.prob_zero(wire);
        let outcome = if p0 >= 0.5 { 0u8 } else { 1u8 };
        let idx = self.wire_map[wire];
        let mask = 1usize << idx;
        let norm = if outcome == 0 {
            p0.sqrt()
        } else {
            (1.0 - p0).sqrt()
        };
        for (basis, amp) in self.state.iter_mut().enumerate() {
            let bit = ((basis & mask) != 0) as u8;
            if bit != outcome {
                *amp = Complex::ZERO;
            } else if norm > 1e-15 {
                *amp = *amp * (1.0 / norm);
            }
        }
        outcome
    }
}

impl Default for StatevectorSim {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qgo_hir::check_module;
    use qgo_mir::lower_to_mir;
    use qgo_opt::{optimize_module, SynthMode};
    use qgo_span::FileId;
    use qgo_syntax::parse_source;

    #[test]
    fn test_defer_adj_returns_ancilla_to_exact_zero_state() {
        let src = r#"package main
func ComputeParity(x qreg[3], anc qubit) qfree {
    CX(x[0], anc)
    CX(x[1], anc)
    CX(x[2], anc)
}

func PhaseOracle(x qreg[3], out qubit) unitary {
    H(x[:])
    anc := qalloc()
    ComputeParity(x, anc)
    defer adj ComputeParity(x, anc) -> anc
    CX(anc, out)
}
"#;
        let (ast, pdiags) = parse_source(FileId(0), src);
        assert!(pdiags.is_empty());
        let (hir, hdiags) = check_module(&ast);
        assert!(hdiags.is_empty());
        let mut mir = lower_to_mir(&hir);
        optimize_module(&mut mir, SynthMode::Width);

        let func = mir.funcs.iter().find(|f| f.name == "PhaseOracle").unwrap();
        let mut sim = StatevectorSim::new();
        sim.execute_func(func);

        let anc_wire = QubitWire::new("anc", 0);
        let p0 = sim.prob_zero(&anc_wire);
        assert!(
            (1.0 - p0).abs() < 1e-12,
            "ancilla must return to |0> with probability 1.0 - eps (eps < 1e-12), got {}",
            p0
        );
    }

    #[test]
    fn test_cuccaro_and_draper_adders_on_statevector() {
        let src = r#"package arith
func AddTest(a qint[3], b qint[3]) unitary {
    CuccaroAdd(a, b)
}
"#;
        let (ast, _) = parse_source(FileId(0), src);
        let (hir, diags) = check_module(&ast);
        assert!(diags.is_empty());

        for mode in [SynthMode::Width, SynthMode::Depth] {
            let mut mir = lower_to_mir(&hir);
            optimize_module(&mut mir, mode);
            let func = &mir.funcs[0];
            let mut sim = StatevectorSim::new();
            // Prepare a = 3 (0b011), b = 4 (0b100) -> expected b = 7 (0b111)
            sim.set_register_uint("a", 3, 3);
            sim.set_register_uint("b", 3, 4);
            sim.execute_func(func);
            assert_eq!(sim.measure_register_uint("a", 3), 3);
            assert_eq!(sim.measure_register_uint("b", 3), 7);
        }
    }

    #[test]
    fn test_stabilizer_sim_clifford_equivalence() {
        let src = r#"package main
func MakeGHZ(reg qreg[4]) unitary {
    H(reg[0])
    CX(reg[0], reg[1])
    CX(reg[1], reg[2])
    CX(reg[2], reg[3])
    S(reg[0])
    Sdg(reg[0])
}
"#;
        let (ast, _) = parse_source(FileId(0), src);
        let (hir, diags) = check_module(&ast);
        assert!(diags.is_empty());
        let mut mir = lower_to_mir(&hir);
        optimize_module(&mut mir, SynthMode::Width);

        let func = &mir.funcs[0];
        let mut sv = StatevectorSim::new();
        sv.execute_func(func);

        let mut stab = StabilizerSim::new();
        assert!(StabilizerSim::is_clifford_func(func));
        stab.execute_func(func);

        for i in 0..4 {
            let w = QubitWire::new("reg", i);
            assert!((sv.prob_zero(&w) - 0.5).abs() < 1e-12);
            assert!((stab.prob_zero(&w) - 0.5).abs() < 1e-12);
        }
        // Collapse wire 0 -> all wires in GHZ must collapse to the same outcome
        let m0 = stab.measure(&QubitWire::new("reg", 0));
        for i in 1..4 {
            let w = QubitWire::new("reg", i);
            let expected_p0 = if m0 == 0 { 1.0 } else { 0.0 };
            assert!((stab.prob_zero(&w) - expected_p0).abs() < 1e-12);
        }
    }

    #[test]
    fn test_qgovm_hybrid_quantum_and_classical_execution() {
        let src = r#"package main

type Pair struct {
    a int
    b int
}

func Oracle(x qreg[2], anc qubit) qfree {
    CCX(x[0], x[1], anc)
}

func main() {
    p := Pair{a: 3, b: 4}
    assert_eq(p.a + p.b, 7)

    reg := qalloc[2]()
    X(reg[0])
    X(reg[1])
    anc := qalloc()
    Oracle(reg, anc)
    defer adj Oracle(reg, anc) -> anc
    out := qalloc()
    CX(anc, out)
    m := measure(out)
    assert(prob_zero(anc) > 0.999999999)
    assert_eq(m, 1)
    println("QGoVM ok:", m)
}
"#;
        let (ast, pdiags) = parse_source(FileId(0), src);
        assert!(pdiags.is_empty(), "{:?}", pdiags);
        let (_hir, hdiags) = check_module(&ast);
        assert!(hdiags.is_empty(), "{:?}", hdiags);

        let mut vm = QGoVM::new(&ast, SynthMode::Width);
        vm.run_main().expect("QGoVM execution should succeed");
        assert_eq!(vm.stdout.trim(), "QGoVM ok: 1");
    }
}
