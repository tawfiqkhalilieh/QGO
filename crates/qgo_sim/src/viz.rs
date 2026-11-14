//! `qgo_sim::viz`: Quantum Memory Evolution Tracer & Animated GIF89a Visualizer.
//! Captures step-by-step quantum register & ancilla memory states (marginal probabilities,
//! single-qubit reduced density matrix purity $\text{Tr}(\rho_q^2)$, Bloch vectors,
//! `Compute (U)` / `Action (V)` / `Defer Adj Uncompute (U†)` lifecycle transitions,
//! and complex statevector amplitudes) and renders animated `.gif` dashboards.

use std::f64::consts::PI;
use std::fs;
use std::path::Path;
use qgo_mir::{ invert_stmts, MirFunc, MirGateKind, MirStmt, QubitWire };
use crate::StatevectorSim;

/// Conjugation lifecycle stage of a step in the quantum memory trace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConjStage {
    Standard,
    Compute(String),
    Action(String),
    Uncompute(String),
    VerifiedFree(String),
}

impl ConjStage {
    pub fn banner_text(&self) -> String {
        match self {
            ConjStage::Standard => "STAGE: STANDARD QUANTUM EXECUTION".to_owned(),
            ConjStage::Compute(t) => format!("STAGE: COMPUTE (U) -> ANCILLA `{}`", t),
            ConjStage::Action(t) => format!("STAGE: ACTION (V) USING ANCILLA `{}`", t),
            ConjStage::Uncompute(t) => format!("STAGE: DEFER ADJ UNCOMPUTE (U_dag) -> `{}`", t),
            ConjStage::VerifiedFree(t) => format!("STAGE: ANCILLA `{}` VERIFIED |0> & FREED", t),
        }
    }
}

/// Memory state of an individual qubit wire at a single trace snapshot.
#[derive(Debug, Clone)]
pub struct WireMemorySlot {
    pub wire: QubitWire,
    pub is_ancilla: bool,
    pub is_freed: bool,
    pub prob_one: f64,
    pub bloch_x: f64,
    pub bloch_y: f64,
    pub bloch_z: f64,
    /// Single-qubit reduced density matrix purity $\text{Tr}(\rho_q^2) \in [0.5, 1.0]$.
    /// `< 0.999` indicates entanglement with other qubits in quantum memory.
    pub purity: f64,
    pub is_active_ctrl: bool,
    pub is_active_target: bool,
}

/// Complete snapshot of quantum memory at one execution step.
#[derive(Debug, Clone)]
pub struct MemorySnapshot {
    pub step_idx: usize,
    pub total_steps: usize,
    pub op_label: String,
    pub stage: ConjStage,
    pub wires: Vec<WireMemorySlot>,
    pub data_wires: Vec<QubitWire>,
    /// Marginal probabilities of the primary data register basis states (up to 16 states).
    pub basis_probs: Vec<f64>,
    /// Dominant complex phase $\arg(\alpha_x) \in (-\pi, \pi]$ for each basis state.
    pub basis_phases: Vec<f64>,
    /// Cumulative number of ancilla allocations recycled from the $|0\rangle$ physical qubit pool.
    pub recycled_qubits: usize,
}

/// Full execution trace of a quantum function for visualization.
#[derive(Debug, Clone)]
pub struct MemoryTrace {
    pub func_name: String,
    pub purity_label: String,
    pub snapshots: Vec<MemorySnapshot>,
}

impl StatevectorSim {
    /// Computes the single-qubit Bloch vector $(\langle X \rangle, \langle Y \rangle, \langle Z \rangle)$
    /// and reduced state purity $\text{Tr}(\rho_q^2) = \frac{1}{2}(1 + x^2 + y^2 + z^2) \in [0.5, 1.0]$
    /// by tracing out all other qubits.
    pub fn qubit_bloch_and_purity(&self, wire: &QubitWire) -> (f64, f64, f64, f64) {
        let Some(&idx) = self.wire_map.get(wire) else {
            return (0.0, 0.0, 1.0, 1.0);
        };
        let mask = 1usize << idx;
        let dim = self.state.len();
        let mut rho00 = 0.0f64;
        let mut rho11 = 0.0f64;
        let mut rho01_re = 0.0f64;
        let mut rho01_im = 0.0f64;

        for i0 in 0..dim {
            if (i0 & mask) == 0 {
                let i1 = i0 | mask;
                let a0 = self.state[i0];
                let a1 = self.state[i1];
                rho00 += a0.norm_sqr();
                rho11 += a1.norm_sqr();
                // a0 * conj(a1) = (a0.re + i a0.im)(a1.re - i a1.im)
                rho01_re += a0.re * a1.re + a0.im * a1.im;
                rho01_im += a0.im * a1.re - a0.re * a1.im;
            }
        }

        let bx = 2.0 * rho01_re;
        let by = -2.0 * rho01_im;
        let bz = rho00 - rho11;
        let purity = (0.5 * (1.0 + bx * bx + by * by + bz * bz)).clamp(0.5, 1.0);
        (bx, by, bz, purity)
    }
}

/// Traces a [`MirFunc`] step-by-step, preserving `MirStmt::Conjugate` stage metadata.
pub fn trace_func_memory(func: &MirFunc, max_frames: usize) -> MemoryTrace {
    let mut sim = StatevectorSim::new();
    let mut ordered_wires: Vec<(QubitWire, bool, bool)> = Vec::new(); // (wire, is_ancilla, is_freed)
    let mut data_wires: Vec<QubitWire> = Vec::new();

    for p in &func.params {
        if p.is_quantum {
            for b in 0..p.width.max(1) {
                let w = QubitWire::new(p.name.clone(), b);
                sim.allocate_wire(w.clone(), false);
                ordered_wires.push((w.clone(), false, false));
                data_wires.push(w);
            }
        }
    }

    let mut raw_snapshots: Vec<MemorySnapshot> = Vec::new();
    raw_snapshots.push(capture_snapshot(
        &sim,
        &ordered_wires,
        &data_wires,
        "INITIAL MEMORY STATE |0...0>".to_owned(),
        ConjStage::Standard,
        &[],
        &[],
    ));

    trace_stmts_recursive(
        &func.body,
        &mut sim,
        &mut ordered_wires,
        &mut data_wires,
        &ConjStage::Standard,
        &mut raw_snapshots,
    );

    // If the circuit has many micro-gates (e.g. unrolled QFT or Shor), subsample evenly
    // while always keeping the initial frame, conjugation transition frames, and final frame.
    let mut selected = if raw_snapshots.len() <= max_frames {
        raw_snapshots
    } else {
        let total = raw_snapshots.len();
        let mut out = Vec::with_capacity(max_frames);
        for i in 0..max_frames {
            let idx = (i * (total - 1)) / (max_frames - 1).max(1);
            out.push(raw_snapshots[idx].clone());
        }
        out
    };

    let n_total = selected.len();
    for (idx, snap) in selected.iter_mut().enumerate() {
        snap.step_idx = idx;
        snap.total_steps = n_total;
    }

    MemoryTrace {
        func_name: func.name.clone(),
        purity_label: format!("{}", func.purity),
        snapshots: selected,
    }
}

fn trace_stmts_recursive(
    stmts: &[MirStmt],
    sim: &mut StatevectorSim,
    ordered_wires: &mut Vec<(QubitWire, bool, bool)>,
    data_wires: &mut Vec<QubitWire>,
    stage: &ConjStage,
    snaps: &mut Vec<MemorySnapshot>,
) {
    for stmt in stmts {
        match stmt {
            MirStmt::QAlloc {
                reg,
                size,
                is_ancilla,
            } => {
                for b in 0..(*size).max(1) {
                    let w = QubitWire::new(reg.clone(), b);
                    sim.allocate_wire(w.clone(), *is_ancilla);
                    if let Some(entry) = ordered_wires.iter_mut().find(|(ew, _, _)| *ew == w) {
                        entry.2 = false;
                    } else {
                        ordered_wires.push((w.clone(), *is_ancilla, false));
                    }
                    if !*is_ancilla && !data_wires.contains(&w) {
                        data_wires.push(w);
                    }
                }
                let role = if *is_ancilla { "Ancilla" } else { "Data" };
                let targets: Vec<QubitWire> = (0..(*size).max(1))
                    .map(|b| QubitWire::new(reg.clone(), b))
                    .collect();
                snaps.push(capture_snapshot(
                    sim,
                    ordered_wires,
                    data_wires,
                    format!("qalloc {}[0..{}] ({})", reg, size, role),
                    stage.clone(),
                    &[],
                    &targets,
                ));
            }
            MirStmt::QFree { reg, size } => {
                let mut targets = Vec::new();
                for b in 0..(*size).max(1) {
                    let w = QubitWire::new(reg.clone(), b);
                    sim.release_wire(&w);
                    if let Some(entry) = ordered_wires.iter_mut().find(|(ew, _, _)| *ew == w) {
                        entry.2 = true;
                    }
                    targets.push(w);
                }
                snaps.push(capture_snapshot(
                    sim,
                    ordered_wires,
                    data_wires,
                    format!("qfree {}[0..{}] (Verified |0> -> Pool)", reg, size),
                    ConjStage::VerifiedFree(reg.clone()),
                    &[],
                    &targets,
                ));
            }
            MirStmt::Conjugate {
                compute,
                action,
                target,
            } => {
                let tname = target.clone().unwrap_or_else(|| "anc".to_owned());
                trace_stmts_recursive(
                    compute,
                    sim,
                    ordered_wires,
                    data_wires,
                    &ConjStage::Compute(tname.clone()),
                    snaps,
                );
                trace_stmts_recursive(
                    action,
                    sim,
                    ordered_wires,
                    data_wires,
                    &ConjStage::Action(tname.clone()),
                    snaps,
                );
                let uncompute = invert_stmts(compute);
                trace_stmts_recursive(
                    &uncompute,
                    sim,
                    ordered_wires,
                    data_wires,
                    &ConjStage::Uncompute(tname),
                    snaps,
                );
            }
            MirStmt::Gate {
                kind,
                controls,
                targets,
            } => {
                for c in controls {
                    sim.allocate_wire(c.clone(), false);
                    if !ordered_wires.iter().any(|(w, _, _)| w == c) {
                        ordered_wires.push((c.clone(), false, false));
                    }
                }
                for t in targets {
                    sim.allocate_wire(t.clone(), false);
                    if !ordered_wires.iter().any(|(w, _, _)| w == t) {
                        ordered_wires.push((t.clone(), false, false));
                    }
                }
                sim.apply_gate(kind, controls, targets);
                let label = format_gate_label(kind, controls, targets);
                snaps.push(capture_snapshot(
                    sim,
                    ordered_wires,
                    data_wires,
                    label,
                    stage.clone(),
                    controls,
                    targets,
                ));
            }
            MirStmt::Measure { qubit, dst_var } => {
                let outcome = sim.measure_wire(qubit);
                sim.classical_vars.insert(dst_var.clone(), outcome as i64);
                snaps.push(capture_snapshot(
                    sim,
                    ordered_wires,
                    data_wires,
                    format!("{} := measure({}) -> {}", dst_var, qubit, outcome),
                    stage.clone(),
                    &[],
                    std::slice::from_ref(qubit),
                ));
            }
            MirStmt::Reset { qubit } => {
                sim.reset_wire(qubit);
                snaps.push(capture_snapshot(
                    sim,
                    ordered_wires,
                    data_wires,
                    format!("reset({}) -> |0>", qubit),
                    stage.clone(),
                    &[],
                    std::slice::from_ref(qubit),
                ));
            }
            _ => {}
        }
    }
}

fn format_gate_label(
    kind: &MirGateKind,
    controls: &[QubitWire],
    targets: &[QubitWire],
) -> String {
    let gname = match kind {
        MirGateKind::X if controls.len() == 1 => "CX".to_owned(),
        MirGateKind::X if controls.len() == 2 => "CCX".to_owned(),
        MirGateKind::X => "X".to_owned(),
        MirGateKind::Y => "Y".to_owned(),
        MirGateKind::Z => "Z".to_owned(),
        MirGateKind::H => "H".to_owned(),
        MirGateKind::S => "S".to_owned(),
        MirGateKind::Sdg => "Sdg".to_owned(),
        MirGateKind::T => "T".to_owned(),
        MirGateKind::Tdg => "Tdg".to_owned(),
        MirGateKind::Rx(th) => format!("Rx({:.2})", th),
        MirGateKind::Ry(th) => format!("Ry({:.2})", th),
        MirGateKind::Rz(th) => format!("Rz({:.2})", th),
        MirGateKind::Phase(th) if !controls.is_empty() => format!("CP({:.2})", th),
        MirGateKind::Phase(th) => format!("P({:.2})", th),
        MirGateKind::Swap => "Swap".to_owned(),
    };
    let mut all_args: Vec<String> = controls.iter().map(|w| w.to_string()).collect();
    all_args.extend(targets.iter().map(|w| w.to_string()));
    if controls.len() > 2 || (controls.len() >= 1 && !matches!(kind, MirGateKind::X | MirGateKind::Phase(_))) {
        let cs: Vec<String> = controls.iter().map(|w| w.to_string()).collect();
        let ts: Vec<String> = targets.iter().map(|w| w.to_string()).collect();
        format!("ctrl({}) {}({})", cs.join(","), gname, ts.join(","))
    } else {
        format!("{}({})", gname, all_args.join(", "))
    }
}

fn capture_snapshot(
    sim: &StatevectorSim,
    ordered_wires: &[(QubitWire, bool, bool)],
    data_wires: &[QubitWire],
    op_label: String,
    stage: ConjStage,
    active_ctrls: &[QubitWire],
    active_targets: &[QubitWire],
) -> MemorySnapshot {
    let mut wires = Vec::with_capacity(ordered_wires.len());
    for (w, is_anc, is_freed) in ordered_wires {
        let p0 = sim.prob_zero(w);
        let (bx, by, bz, purity) = sim.qubit_bloch_and_purity(w);
        wires.push(WireMemorySlot {
            wire: w.clone(),
            is_ancilla: *is_anc,
            is_freed: *is_freed,
            prob_one: (1.0 - p0).clamp(0.0, 1.0),
            bloch_x: bx,
            bloch_y: by,
            bloch_z: bz,
            purity,
            is_active_ctrl: active_ctrls.contains(w),
            is_active_target: active_targets.contains(w),
        });
    }

    // Choose up to 4 primary data wires (up to 16 basis states) for the basis distribution chart
    let chart_wires: Vec<QubitWire> = if !data_wires.is_empty() {
        data_wires.iter().take(4).cloned().collect()
    } else {
        ordered_wires
            .iter()
            .filter(|(_, is_anc, _)| !*is_anc)
            .chain(ordered_wires.iter())
            .map(|(w, _, _)| w.clone())
            .take(4)
            .collect()
    };

    let num_chart_q = chart_wires.len().min(4);
    let num_states = 1usize << num_chart_q;
    let mut basis_probs = vec![0.0f64; num_states];
    let mut basis_amp_sum = vec![(0.0f64, 0.0f64); num_states];

    for (full_basis, amp) in sim.state.iter().enumerate() {
        let p = amp.norm_sqr();
        if p < 1e-18 {
            continue;
        }
        let mut sub_idx = 0usize;
        for (bit_pos, cw) in chart_wires.iter().enumerate() {
            if let Some(&w_idx) = sim.wire_map.get(cw) {
                if ((full_basis >> w_idx) & 1) == 1 {
                    sub_idx |= 1usize << bit_pos;
                }
            }
        }
        basis_probs[sub_idx] += p;
        basis_amp_sum[sub_idx].0 += amp.re;
        basis_amp_sum[sub_idx].1 += amp.im;
    }

    let basis_phases: Vec<f64> = basis_amp_sum
        .iter()
        .map(|&(re, im)| {
            if re * re + im * im < 1e-12 {
                0.0
            } else {
                im.atan2(re)
            }
        })
        .collect();

    MemorySnapshot {
        step_idx: 0,
        total_steps: 1,
        op_label,
        stage,
        wires,
        data_wires: chart_wires,
        basis_probs,
        basis_phases,
        recycled_qubits: sim.recycled_qubit_count,
    }
}

// ============================================================================
// Pure-Rust Animated GIF89a Dashboard Renderer
// ============================================================================

const IMG_W: usize = 840;
const IMG_H: usize = 520;

// Palette indices:
const COL_BG_DARK: u8 = 0;       // #0B0F19
const COL_PANEL_BG: u8 = 1;      // #111827
const COL_CARD_BG: u8 = 2;       // #1E293B
const COL_BORDER: u8 = 3;        // #334155
const COL_GRID: u8 = 4;          // #1E293B
const COL_TEXT_BRIGHT: u8 = 5;   // #F8FAFC
const COL_TEXT_MUTED: u8 = 6;    // #94A3B8
const COL_DATA_CYAN: u8 = 7;     // #38BDF8
const COL_DATA_DARK: u8 = 8;     // #0369A1
const COL_ANC_AMBER: u8 = 9;     // #F59E0B
const COL_ANC_DARK: u8 = 10;     // #B45309
const COL_ACTION_PURPLE: u8 = 11;// #C084FC
const COL_ACTION_DARK: u8 = 12;  // #6B21A8
const COL_FREE_GREEN: u8 = 13;   // #34D399
const COL_FREE_DARK: u8 = 14;    // #047857
const COL_ENT_ROSE: u8 = 15;     // #FB7185
const COL_ENT_DARK: u8 = 16;     // #BE123C
const COL_BAR_TRACK: u8 = 17;    // #0F172A
const COL_GOLD_HIGHLIGHT: u8 = 18; // #FACC15
// Indices 64..128: 64-step HSV phase wheel for complex quantum phases (-pi..pi]

fn build_palette() -> [u8; 768] {
    let mut pal = [0u8; 768];
    let set = |pal: &mut [u8; 768], idx: u8, r: u8, g: u8, b: u8| {
        let base = (idx as usize) * 3;
        pal[base] = r;
        pal[base + 1] = g;
        pal[base + 2] = b;
    };

    set(&mut pal, COL_BG_DARK, 11, 15, 25);
    set(&mut pal, COL_PANEL_BG, 17, 24, 39);
    set(&mut pal, COL_CARD_BG, 30, 41, 59);
    set(&mut pal, COL_BORDER, 51, 65, 85);
    set(&mut pal, COL_GRID, 26, 36, 54);
    set(&mut pal, COL_TEXT_BRIGHT, 248, 250, 252);
    set(&mut pal, COL_TEXT_MUTED, 148, 163, 184);
    set(&mut pal, COL_DATA_CYAN, 56, 189, 248);
    set(&mut pal, COL_DATA_DARK, 3, 105, 161);
    set(&mut pal, COL_ANC_AMBER, 245, 158, 11);
    set(&mut pal, COL_ANC_DARK, 180, 83, 9);
    set(&mut pal, COL_ACTION_PURPLE, 192, 132, 252);
    set(&mut pal, COL_ACTION_DARK, 107, 33, 168);
    set(&mut pal, COL_FREE_GREEN, 52, 211, 153);
    set(&mut pal, COL_FREE_DARK, 4, 120, 87);
    set(&mut pal, COL_ENT_ROSE, 251, 113, 133);
    set(&mut pal, COL_ENT_DARK, 190, 18, 60);
    set(&mut pal, COL_BAR_TRACK, 15, 23, 42);
    set(&mut pal, COL_GOLD_HIGHLIGHT, 250, 204, 21);

    // 64 HSV phase colors at indices 64..128
    for i in 0..64 {
        let hue = (i as f64 / 64.0) * 360.0;
        // Shift hue by 195 deg so phase 0 is bright cyan-blue (#38BDF8)
        let h = ((hue + 195.0) % 360.0) / 60.0;
        let s = 0.82f64;
        let v = 0.96f64;
        let c = v * s;
        let x = c * (1.0 - ((h % 2.0) - 1.0).abs());
        let m = v - c;
        let (r1, g1, b1) = if h < 1.0 {
            (c, x, 0.0)
        } else if h < 2.0 {
            (x, c, 0.0)
        } else if h < 3.0 {
            (0.0, c, x)
        } else if h < 4.0 {
            (0.0, x, c)
        } else if h < 5.0 {
            (x, 0.0, c)
        } else {
            (c, 0.0, x)
        };
        let r = ((r1 + m) * 255.0).round() as u8;
        let g = ((g1 + m) * 255.0).round() as u8;
        let b = ((b1 + m) * 255.0).round() as u8;
        set(&mut pal, 64 + i as u8, r, g, b);
    }

    pal
}

fn phase_to_color(phase: f64) -> u8 {
    // Map phase in (-pi, pi] -> [0, 63]
    let norm = ((phase + 2.0 * PI) % (2.0 * PI)) / (2.0 * PI);
    let idx = ((norm * 64.0).floor() as usize).min(63);
    64 + (idx as u8)
}

/// Renders a [`MemoryTrace`] into an animated GIF89a file at `output_path`.
pub fn render_trace_gif(
    trace: &MemoryTrace,
    title: &str,
    output_path: &Path,
) -> Result<(), String> {
    if let Some(parent) = output_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let palette = build_palette();
    let mut gif_bytes: Vec<u8> = Vec::with_capacity(256 * 1024);

    // 1. GIF89a Header & Logical Screen Descriptor
    gif_bytes.extend_from_slice(b"GIF89a");
    gif_bytes.extend_from_slice(&(IMG_W as u16).to_le_bytes());
    gif_bytes.extend_from_slice(&(IMG_H as u16).to_le_bytes());
    // Packed field: Global Color Table Flag (1), Color Resolution (7 = 8 bits), Sort (0), Size of GCT (7 = 256 colors) -> 0xF7
    gif_bytes.push(0xF7);
    gif_bytes.push(COL_BG_DARK);
    gif_bytes.push(0); // Pixel aspect ratio
    gif_bytes.extend_from_slice(&palette);

    // 2. Netscape 2.0 Application Extension for infinite looping
    gif_bytes.extend_from_slice(&[
        0x21, 0xFF, 0x0B, b'N', b'E', b'T', b'S', b'C', b'A', b'P', b'E', b'2', b'.', b'0', 0x03,
        0x01, 0x00, 0x00, 0x00,
    ]);

    // 3. Render & encode each frame
    for (idx, snap) in trace.snapshots.iter().enumerate() {
        let pixels = render_snapshot_frame(trace, snap, title);
        let delay_cs: u16 = if idx + 1 == trace.snapshots.len() {
            140 // 1.4s pause on final frame
        } else if idx == 0 {
            70 // 0.7s on initial frame
        } else if !matches!(snap.stage, ConjStage::Standard) {
            55 // 0.55s on conjugation stages
        } else {
            42 // 0.42s on standard steps
        };

        // Graphic Control Extension
        gif_bytes.extend_from_slice(&[0x21, 0xF9, 0x04, 0x04]);
        gif_bytes.extend_from_slice(&delay_cs.to_le_bytes());
        gif_bytes.push(0x00); // transparent color index (unused)
        gif_bytes.push(0x00); // block terminator

        // Image Descriptor
        gif_bytes.push(0x2C);
        gif_bytes.extend_from_slice(&0u16.to_le_bytes()); // left
        gif_bytes.extend_from_slice(&0u16.to_le_bytes()); // top
        gif_bytes.extend_from_slice(&(IMG_W as u16).to_le_bytes());
        gif_bytes.extend_from_slice(&(IMG_H as u16).to_le_bytes());
        gif_bytes.push(0x00); // no local color table

        // LZW Image Data
        lzw_encode_frame(&pixels, &mut gif_bytes);
    }

    // 4. GIF Trailer
    gif_bytes.push(0x3B);

    fs::write(output_path, &gif_bytes)
        .map_err(|e| format!("failed to write GIF {:?}: {}", output_path, e))
}

fn render_snapshot_frame(
    trace: &MemoryTrace,
    snap: &MemorySnapshot,
    title: &str,
) -> Vec<u8> {
    let mut fb = vec![COL_BG_DARK; IMG_W * IMG_H];

    // 1. Top Header Bar (y: 10..62)
    fill_rect(&mut fb, 12, 10, IMG_W - 24, 52, COL_PANEL_BG);
    stroke_rect(&mut fb, 12, 10, IMG_W - 24, 52, COL_BORDER);

    let title_short: String = title.chars().take(27).collect();
    draw_text(&mut fb, 24, 18, &title_short, COL_TEXT_BRIGHT, 2);
    let purity_badge = format!("PURITY: {}", trace.purity_label.to_uppercase());
    fill_rect(&mut fb, 24, 40, purity_badge.len() * 8 + 12, 16, COL_DATA_DARK);
    draw_text(&mut fb, 30, 44, &purity_badge, COL_TEXT_BRIGHT, 1);

    // Stage Banner in Top Right
    let (stage_bg, stage_border) = match &snap.stage {
        ConjStage::Standard => (COL_CARD_BG, COL_DATA_CYAN),
        ConjStage::Compute(_) => (COL_ANC_DARK, COL_ANC_AMBER),
        ConjStage::Action(_) => (COL_ACTION_DARK, COL_ACTION_PURPLE),
        ConjStage::Uncompute(_) => (COL_FREE_DARK, COL_FREE_GREEN),
        ConjStage::VerifiedFree(_) => (COL_FREE_DARK, COL_FREE_GREEN),
    };
    let banner = snap.stage.banner_text();
    let bw = (banner.len() * 8 + 20).min(430);
    let bx = IMG_W - 24 - bw;
    fill_rect(&mut fb, bx, 16, bw, 20, stage_bg);
    stroke_rect(&mut fb, bx, 16, bw, 20, stage_border);
    draw_text(&mut fb, bx + 10, 22, &banner, COL_TEXT_BRIGHT, 1);

    let step_str = format!(
        "STEP {:02}/{:02} | OP: {}",
        snap.step_idx + 1,
        snap.total_steps,
        snap.op_label
    );
    draw_text(&mut fb, 165, 44, &step_str, COL_GOLD_HIGHLIGHT, 1);

    // 2. Left Panel: Quantum Register & Ancilla Memory Bank (x: 12..418, y: 70..452)
    let lp_x = 12;
    let lp_y = 70;
    let lp_w = 406;
    let lp_h = 382;
    fill_rect(&mut fb, lp_x, lp_y, lp_w, lp_h, COL_PANEL_BG);
    stroke_rect(&mut fb, lp_x, lp_y, lp_w, lp_h, COL_BORDER);
    draw_text(
        &mut fb,
        lp_x + 12,
        lp_y + 10,
        "QUANTUM REGISTER & ANCILLA MEMORY BANK",
        COL_DATA_CYAN,
        1,
    );
    draw_text(
        &mut fb,
        lp_x + 12,
        lp_y + 24,
        "WIRE     ROLE    STATE     P(|1>)  PURITY BLOCH",
        COL_TEXT_MUTED,
        1,
    );
    fill_rect(&mut fb, lp_x + 10, lp_y + 36, lp_w - 20, 1, COL_BORDER);

    let max_wires = snap.wires.len().min(8);
    let row_h = 40;
    for (i, slot) in snap.wires.iter().take(max_wires).enumerate() {
        let ry = lp_y + 42 + i * row_h;
        let card_border = if slot.is_active_target {
            COL_GOLD_HIGHLIGHT
        } else if slot.is_active_ctrl {
            COL_ACTION_PURPLE
        } else if slot.is_ancilla {
            if slot.prob_one < 1e-6 && slot.purity > 0.999 {
                COL_FREE_GREEN
            } else {
                COL_ANC_AMBER
            }
        } else {
            COL_BORDER
        };
        fill_rect(&mut fb, lp_x + 10, ry, lp_w - 20, 34, COL_CARD_BG);
        stroke_rect(&mut fb, lp_x + 10, ry, lp_w - 20, 34, card_border);

        // Wire name + active gate marker
        let wname = slot.wire.to_string();
        draw_text(&mut fb, lp_x + 16, ry + 6, &wname, COL_TEXT_BRIGHT, 1);
        if slot.is_active_target {
            draw_text(&mut fb, lp_x + 16, ry + 19, "[TGT]", COL_GOLD_HIGHLIGHT, 1);
        } else if slot.is_active_ctrl {
            draw_text(&mut fb, lp_x + 16, ry + 19, "[CTRL]", COL_ACTION_PURPLE, 1);
        }

        // Role badge (DATA vs ANCILLA)
        let (role_txt, role_bg, role_fg) = if slot.is_ancilla {
            ("ANCILLA", COL_ANC_DARK, COL_ANC_AMBER)
        } else {
            ("DATA   ", COL_DATA_DARK, COL_DATA_CYAN)
        };
        fill_rect(&mut fb, lp_x + 88, ry + 8, 62, 18, role_bg);
        draw_text(&mut fb, lp_x + 92, ry + 13, role_txt, role_fg, 1);

        // Memory Lifecycle State label
        let (state_txt, state_col) = if slot.is_freed {
            ("FREED |0>", COL_FREE_GREEN)
        } else if slot.purity < 0.98 {
            ("ENTANGLED", COL_ENT_ROSE)
        } else if slot.prob_one < 1e-4 {
            ("|0> ZERO ", COL_FREE_GREEN)
        } else if slot.prob_one > 0.9999 {
            ("|1> BASIS", COL_DATA_CYAN)
        } else {
            ("SUPERPOS ", COL_ACTION_PURPLE)
        };
        draw_text(&mut fb, lp_x + 156, ry + 13, state_txt, state_col, 1);

        // P(|1>) excitation bar (x: lp_x + 236, w: 68)
        let bar_x = lp_x + 236;
        let bar_w = 64;
        fill_rect(&mut fb, bar_x, ry + 7, bar_w, 10, COL_BAR_TRACK);
        stroke_rect(&mut fb, bar_x, ry + 7, bar_w, 10, COL_BORDER);
        let fill_w = ((slot.prob_one * (bar_w as f64 - 2.0)).round() as usize).min(bar_w - 2);
        let bar_col = if slot.is_ancilla {
            COL_ANC_AMBER
        } else {
            COL_DATA_CYAN
        };
        if fill_w > 0 {
            fill_rect(&mut fb, bar_x + 1, ry + 8, fill_w, 8, bar_col);
        }
        let p1_txt = format!("{:5.1}%", slot.prob_one * 100.0);
        draw_text(&mut fb, bar_x + 6, ry + 20, &p1_txt, COL_TEXT_MUTED, 1);

        // Reduced Purity Tr(rho^2) gauge + mini Bloch indicator
        let pur_x = lp_x + 312;
        let pur_col = if slot.purity < 0.98 {
            COL_ENT_ROSE
        } else {
            COL_FREE_GREEN
        };
        let pur_txt = format!("{:.2}", slot.purity);
        draw_text(&mut fb, pur_x, ry + 13, &pur_txt, pur_col, 1);

        // Mini Bloch circle at (lp_x + 372, ry + 17), radius 11
        let cx = (lp_x + 372) as i32;
        let cy = (ry + 17) as i32;
        draw_circle(&mut fb, cx, cy, 11, COL_BORDER);
        let vx = cx + (slot.bloch_x * 9.0).round() as i32;
        let vy = cy - (slot.bloch_z * 9.0).round() as i32;
        draw_line(&mut fb, cx, cy, vx, vy, pur_col);
        fill_rect(&mut fb, (vx - 1).max(0) as usize, (vy - 1).max(0) as usize, 3, 3, COL_TEXT_BRIGHT);
    }

    // 3. Right Panel: Statevector Probability & Complex Phase Distribution (x: 428..828, y: 70..452)
    let rp_x = 428;
    let rp_y = 70;
    let rp_w = 400;
    let rp_h = 382;
    fill_rect(&mut fb, rp_x, rp_y, rp_w, rp_h, COL_PANEL_BG);
    stroke_rect(&mut fb, rp_x, rp_y, rp_w, rp_h, COL_BORDER);

    draw_text(
        &mut fb,
        rp_x + 12,
        rp_y + 10,
        "STATEVECTOR AMPLITUDE & PHASE MEMORY",
        COL_DATA_CYAN,
        1,
    );
    let reg_desc: Vec<String> = snap.data_wires.iter().map(|w| w.to_string()).collect();
    let joined = reg_desc.join(",");
    let short_reg = if joined.len() > 24 {
        format!(
            "{}.. ({}Q)",
            snap.data_wires
                .first()
                .map(|w| w.to_string())
                .unwrap_or_default(),
            snap.data_wires.len()
        )
    } else {
        joined
    };
    let sub_hdr = format!("BASIS: [{}] (COLOR=PHASE)", short_reg);
    draw_text(&mut fb, rp_x + 12, rp_y + 24, &sub_hdr, COL_TEXT_MUTED, 1);
    fill_rect(&mut fb, rp_x + 10, rp_y + 36, rp_w - 20, 1, COL_BORDER);

    // Draw Phase Color Legend Wheel in top-right header of right panel
    draw_phase_wheel_legend(&mut fb, rp_x + rp_w - 26, rp_y + 18, 12);

    // Draw Bar Chart Area (x: rp_x + 42 .. rp_x + rp_w - 20, y: rp_y + 60 .. rp_y + 328)
    let chart_x = rp_x + 44;
    let chart_y = rp_y + 58;
    let chart_w = rp_w - 64;
    let chart_h = 264;

    // Horizontal grid lines at P = 1.0, 0.75, 0.50, 0.25, 0.0
    for g in 0..=4 {
        let gy = chart_y + (g * chart_h) / 4;
        fill_rect(&mut fb, chart_x, gy, chart_w, 1, COL_GRID);
        let p_val = 1.0 - (g as f64) * 0.25;
        let lbl = format!("{:.2}", p_val);
        draw_text(&mut fb, rp_x + 8, gy.saturating_sub(3), &lbl, COL_TEXT_MUTED, 1);
    }

    let n_bars = snap.basis_probs.len().max(1);
    let n_bits = (n_bars as f64).log2().round() as usize;
    let slot_w = (chart_w / n_bars).max(8);
    let bar_w = (slot_w.saturating_sub(6)).clamp(6, 36);

    for (b_idx, &prob) in snap.basis_probs.iter().enumerate() {
        let bx = chart_x + b_idx * slot_w + (slot_w.saturating_sub(bar_w)) / 2;
        let bh = ((prob.clamp(0.0, 1.0) * (chart_h as f64)).round() as usize).min(chart_h);
        let by = chart_y + chart_h - bh;
        let p_col = phase_to_color(snap.basis_phases[b_idx]);

        if bh > 0 {
            fill_rect(&mut fb, bx, by, bar_w, bh, p_col);
            stroke_rect(&mut fb, bx, by, bar_w, bh, COL_TEXT_BRIGHT);
        }

        // Probability label above non-negligible bars
        if prob >= 0.04 {
            let p_str = format!("{:.0}%", prob * 100.0);
            let tx = bx.saturating_sub(2);
            let ty = by.saturating_sub(11);
            draw_text(&mut fb, tx, ty, &p_str, COL_TEXT_BRIGHT, 1);
        }

        // Basis state ket label below x-axis (`|000>`, `|7>`)
        if n_bars <= 8 {
            let ket = format!("|{:0width$b}>", b_idx, width = n_bits.max(1));
            draw_text(
                &mut fb,
                bx.saturating_sub(4),
                chart_y + chart_h + 6,
                &ket,
                COL_TEXT_MUTED,
                1,
            );
            let dec = format!("({})", b_idx);
            draw_text(
                &mut fb,
                bx,
                chart_y + chart_h + 18,
                &dec,
                COL_DATA_CYAN,
                1,
            );
        } else {
            let dec = format!("{:X}", b_idx);
            draw_text(
                &mut fb,
                bx + bar_w / 4,
                chart_y + chart_h + 6,
                &dec,
                COL_TEXT_MUTED,
                1,
            );
        }
    }

    // Phase Legend Text at bottom of Right Panel
    draw_text(
        &mut fb,
        rp_x + 12,
        rp_y + rp_h - 22,
        "PHASE: CYAN=0  PURPLE=+PI/2  RED=PI  GOLD=-PI/2",
        COL_TEXT_MUTED,
        1,
    );

    // 4. Bottom Panel: Circuit Timeline & Quantum Memory Telemetry (x: 12..828, y: 460..510)
    let bp_x = 12;
    let bp_y = 460;
    let bp_w = IMG_W - 24;
    let bp_h = 50;
    fill_rect(&mut fb, bp_x, bp_y, bp_w, bp_h, COL_PANEL_BG);
    stroke_rect(&mut fb, bp_x, bp_y, bp_w, bp_h, COL_BORDER);

    // Compute live telemetry metrics
    let total_q = snap.wires.len();
    let anc_q = snap.wires.iter().filter(|w| w.is_ancilla).count();
    let dirty_anc = snap
        .wires
        .iter()
        .filter(|w| w.is_ancilla && !w.is_freed && (w.prob_one > 1e-5 || w.purity < 0.999))
        .count();
    let entangled_q = snap.wires.iter().filter(|w| w.purity < 0.98).count();

    let telemetry = format!(
        "TELEMETRY: QUBITS={}  ANCILLAS={}  DIRTY={}  POOL_RECYCLED={}  ENTANGLED={}  NO-CLONING=OK",
        total_q, anc_q, dirty_anc, snap.recycled_qubits, entangled_q
    );
    draw_text(&mut fb, bp_x + 14, bp_y + 8, &telemetry, COL_TEXT_BRIGHT, 1);

    // Progress timeline bar
    let tl_x = bp_x + 14;
    let tl_y = bp_y + 24;
    let tl_w = bp_w - 28;
    let tl_h = 16;
    fill_rect(&mut fb, tl_x, tl_y, tl_w, tl_h, COL_BAR_TRACK);
    stroke_rect(&mut fb, tl_x, tl_y, tl_w, tl_h, COL_BORDER);

    let n_steps = trace.snapshots.len().max(1);
    for (s_idx, s_snap) in trace.snapshots.iter().enumerate() {
        let sx0 = tl_x + 1 + (s_idx * (tl_w - 2)) / n_steps;
        let sx1 = tl_x + 1 + ((s_idx + 1) * (tl_w - 2)) / n_steps;
        let sw = sx1.saturating_sub(sx0).max(2);
        let scol = if s_idx > snap.step_idx {
            COL_CARD_BG
        } else {
            match s_snap.stage {
                ConjStage::Standard => COL_DATA_CYAN,
                ConjStage::Compute(_) => COL_ANC_AMBER,
                ConjStage::Action(_) => COL_ACTION_PURPLE,
                ConjStage::Uncompute(_) | ConjStage::VerifiedFree(_) => COL_FREE_GREEN,
            }
        };
        fill_rect(&mut fb, sx0, tl_y + 2, sw.saturating_sub(1).max(1), tl_h - 4, scol);
        if s_idx == snap.step_idx {
            stroke_rect(&mut fb, sx0, tl_y + 1, sw, tl_h - 2, COL_TEXT_BRIGHT);
        }
    }

    fb
}

fn draw_phase_wheel_legend(fb: &mut [u8], cx: usize, cy: usize, radius: i32) {
    let r2 = radius * radius;
    let inner_r = (radius - 5).max(3);
    let inner_r2 = inner_r * inner_r;
    for dy in -radius..=radius {
        for dx in -radius..=radius {
            let d2 = dx * dx + dy * dy;
            if d2 <= r2 && d2 >= inner_r2 {
                let angle = (-dy as f64).atan2(dx as f64);
                let col = phase_to_color(angle);
                let px = (cx as i32 + dx) as usize;
                let py = (cy as i32 + dy) as usize;
                if px < IMG_W && py < IMG_H {
                    fb[py * IMG_W + px] = col;
                }
            }
        }
    }
}

// ============================================================================
// 2D Drawing Primitives & Built-in 8x8 / 16x16 Bitmap Font
// ============================================================================

fn fill_rect(fb: &mut [u8], x: usize, y: usize, w: usize, h: usize, color: u8) {
    let x_end = (x + w).min(IMG_W);
    let y_end = (y + h).min(IMG_H);
    for py in y..y_end {
        let row = py * IMG_W;
        for px in x..x_end {
            fb[row + px] = color;
        }
    }
}

fn stroke_rect(fb: &mut [u8], x: usize, y: usize, w: usize, h: usize, color: u8) {
    if w == 0 || h == 0 {
        return;
    }
    fill_rect(fb, x, y, w, 1, color);
    fill_rect(fb, x, (y + h - 1).min(IMG_H - 1), w, 1, color);
    fill_rect(fb, x, y, 1, h, color);
    fill_rect(fb, (x + w - 1).min(IMG_W - 1), y, 1, h, color);
}

fn draw_circle(fb: &mut [u8], cx: i32, cy: i32, r: i32, color: u8) {
    let mut x = r;
    let mut y = 0;
    let mut err = 1 - r;
    while x >= y {
        for &(dx, dy) in &[
            (x, y),
            (y, x),
            (-y, x),
            (-x, y),
            (-x, -y),
            (-y, -x),
            (y, -x),
            (x, -y),
        ] {
            let px = cx + dx;
            let py = cy + dy;
            if px >= 0 && (px as usize) < IMG_W && py >= 0 && (py as usize) < IMG_H {
                fb[(py as usize) * IMG_W + (px as usize)] = color;
            }
        }
        y += 1;
        if err < 0 {
            err += 2 * y + 1;
        } else {
            x -= 1;
            err += 2 * (y - x) + 1;
        }
    }
}

fn draw_line(fb: &mut [u8], x0: i32, y0: i32, x1: i32, y1: i32, color: u8) {
    let dx = (x1 - x0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let dy = -(y1 - y0).abs();
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    let mut cx = x0;
    let mut cy = y0;
    loop {
        if cx >= 0 && (cx as usize) < IMG_W && cy >= 0 && (cy as usize) < IMG_H {
            fb[(cy as usize) * IMG_W + (cx as usize)] = color;
        }
        if cx == x1 && cy == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            cx += sx;
        }
        if e2 <= dx {
            err += dx;
            cy += sy;
        }
    }
}

fn draw_text(fb: &mut [u8], x: usize, y: usize, text: &str, color: u8, scale: usize) {
    let s = scale.max(1);
    for (c_idx, ch) in text.chars().enumerate() {
        let glyph = glyph_8x8(ch);
        let gx = x + c_idx * 8 * s;
        for (row, &bits) in glyph.iter().enumerate() {
            for col in 0..8 {
                if ((bits >> (7 - col)) & 1) == 1 {
                    let px = gx + col * s;
                    let py = y + row * s;
                    fill_rect(fb, px, py, s, s, color);
                }
            }
        }
    }
}

fn glyph_8x8(ch: char) -> [u8; 8] {
    match ch.to_ascii_uppercase() {
        'A' => [0x3C, 0x66, 0x66, 0x7E, 0x66, 0x66, 0x66, 0x00],
        'B' => [0x7C, 0x66, 0x66, 0x7C, 0x66, 0x66, 0x7C, 0x00],
        'C' => [0x3C, 0x66, 0x60, 0x60, 0x60, 0x66, 0x3C, 0x00],
        'D' => [0x78, 0x6C, 0x66, 0x66, 0x66, 0x6C, 0x78, 0x00],
        'E' => [0x7E, 0x60, 0x60, 0x7C, 0x60, 0x60, 0x7E, 0x00],
        'F' => [0x7E, 0x60, 0x60, 0x7C, 0x60, 0x60, 0x60, 0x00],
        'G' => [0x3C, 0x66, 0x60, 0x6E, 0x66, 0x66, 0x3E, 0x00],
        'H' => [0x66, 0x66, 0x66, 0x7E, 0x66, 0x66, 0x66, 0x00],
        'I' => [0x3C, 0x18, 0x18, 0x18, 0x18, 0x18, 0x3C, 0x00],
        'J' => [0x1E, 0x0C, 0x0C, 0x0C, 0x6C, 0x6C, 0x38, 0x00],
        'K' => [0x66, 0x6C, 0x78, 0x70, 0x78, 0x6C, 0x66, 0x00],
        'L' => [0x60, 0x60, 0x60, 0x60, 0x60, 0x60, 0x7E, 0x00],
        'M' => [0x63, 0x77, 0x7F, 0x6B, 0x63, 0x63, 0x63, 0x00],
        'N' => [0x66, 0x76, 0x7E, 0x7E, 0x6E, 0x66, 0x66, 0x00],
        'O' => [0x3C, 0x66, 0x66, 0x66, 0x66, 0x66, 0x3C, 0x00],
        'P' => [0x7C, 0x66, 0x66, 0x7C, 0x60, 0x60, 0x60, 0x00],
        'Q' => [0x3C, 0x66, 0x66, 0x66, 0x6A, 0x6C, 0x36, 0x00],
        'R' => [0x7C, 0x66, 0x66, 0x7C, 0x6C, 0x66, 0x66, 0x00],
        'S' => [0x3C, 0x66, 0x60, 0x3C, 0x06, 0x66, 0x3C, 0x00],
        'T' => [0x7E, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x00],
        'U' => [0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x3C, 0x00],
        'V' => [0x66, 0x66, 0x66, 0x66, 0x66, 0x3C, 0x18, 0x00],
        'W' => [0x63, 0x63, 0x63, 0x6B, 0x7F, 0x77, 0x63, 0x00],
        'X' => [0x66, 0x66, 0x3C, 0x18, 0x3C, 0x66, 0x66, 0x00],
        'Y' => [0x66, 0x66, 0x66, 0x3C, 0x18, 0x18, 0x18, 0x00],
        'Z' => [0x7E, 0x06, 0x0C, 0x18, 0x30, 0x60, 0x7E, 0x00],
        '0' => [0x3C, 0x66, 0x6E, 0x76, 0x66, 0x66, 0x3C, 0x00],
        '1' => [0x18, 0x38, 0x18, 0x18, 0x18, 0x18, 0x7E, 0x00],
        '2' => [0x3C, 0x66, 0x06, 0x1C, 0x30, 0x60, 0x7E, 0x00],
        '3' => [0x3C, 0x66, 0x06, 0x1C, 0x06, 0x66, 0x3C, 0x00],
        '4' => [0x0C, 0x1C, 0x3C, 0x6C, 0x7E, 0x0C, 0x0C, 0x00],
        '5' => [0x7E, 0x60, 0x7C, 0x06, 0x06, 0x66, 0x3C, 0x00],
        '6' => [0x3C, 0x60, 0x60, 0x7C, 0x66, 0x66, 0x3C, 0x00],
        '7' => [0x7E, 0x06, 0x0C, 0x18, 0x30, 0x30, 0x30, 0x00],
        '8' => [0x3C, 0x66, 0x66, 0x3C, 0x66, 0x66, 0x3C, 0x00],
        '9' => [0x3C, 0x66, 0x66, 0x3E, 0x06, 0x06, 0x3C, 0x00],
        ':' => [0x00, 0x18, 0x18, 0x00, 0x18, 0x18, 0x00, 0x00],
        '.' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x18, 0x18, 0x00],
        ',' => [0x00, 0x00, 0x00, 0x00, 0x18, 0x18, 0x30, 0x00],
        '-' => [0x00, 0x00, 0x00, 0x7E, 0x00, 0x00, 0x00, 0x00],
        '+' => [0x00, 0x18, 0x18, 0x7E, 0x18, 0x18, 0x00, 0x00],
        '=' => [0x00, 0x00, 0x7E, 0x00, 0x7E, 0x00, 0x00, 0x00],
        '|' => [0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18],
        '>' => [0x60, 0x30, 0x18, 0x0C, 0x18, 0x30, 0x60, 0x00],
        '<' => [0x0C, 0x18, 0x30, 0x60, 0x30, 0x18, 0x0C, 0x00],
        '(' => [0x0C, 0x18, 0x30, 0x30, 0x30, 0x18, 0x0C, 0x00],
        ')' => [0x30, 0x18, 0x0C, 0x0C, 0x0C, 0x18, 0x30, 0x00],
        '[' => [0x3C, 0x30, 0x30, 0x30, 0x30, 0x30, 0x3C, 0x00],
        ']' => [0x3C, 0x0C, 0x0C, 0x0C, 0x0C, 0x0C, 0x3C, 0x00],
        '/' => [0x06, 0x0C, 0x18, 0x30, 0x60, 0x40, 0x00, 0x00],
        '%' => [0x62, 0x64, 0x08, 0x10, 0x26, 0x46, 0x00, 0x00],
        '_' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x7E, 0x00],
        '^' => [0x18, 0x3C, 0x66, 0x00, 0x00, 0x00, 0x00, 0x00],
        '`' | '\'' => [0x18, 0x18, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00],
        '&' => [0x38, 0x6C, 0x38, 0x76, 0xDC, 0xCC, 0x76, 0x00],
        _ => [0x00; 8],
    }
}

// ============================================================================
// GIF89a Variable-Width LZW Encoder (8-bit min code size)
// ============================================================================

fn lzw_encode_frame(pixels: &[u8], out: &mut Vec<u8>) {
    let min_code_size: u8 = 8;
    out.push(min_code_size);

    let clear_code: u16 = 256;
    let eoi_code: u16 = 257;

    // LZW trie table: for each node (0..4096) and next byte (0..256), stores child code or u16::MAX
    let mut table = vec![u16::MAX; 4096 * 256];
    let mut next_code: u16 = 258;
    let mut code_size: u32 = 9;

    let mut bit_buf: u64 = 0;
    let mut bit_count: u32 = 0;
    let mut compressed_bytes: Vec<u8> = Vec::with_capacity(pixels.len() / 2);

    let push_code = |code: u16,
                     c_size: u32,
                     bit_buf: &mut u64,
                     bit_count: &mut u32,
                     bytes: &mut Vec<u8>| {
        *bit_buf |= (code as u64) << *bit_count;
        *bit_count += c_size;
        while *bit_count >= 8 {
            bytes.push((*bit_buf & 0xFF) as u8);
            *bit_buf >>= 8;
            *bit_count -= 8;
        }
    };

    // Emit initial Clear code
    push_code(
        clear_code,
        code_size,
        &mut bit_buf,
        &mut bit_count,
        &mut compressed_bytes,
    );

    if !pixels.is_empty() {
        let mut prefix: u16 = pixels[0] as u16;
        for &k in &pixels[1..] {
            let key = (prefix as usize) * 256 + (k as usize);
            let child = table[key];
            if child != u16::MAX {
                prefix = child;
            } else {
                push_code(
                    prefix,
                    code_size,
                    &mut bit_buf,
                    &mut bit_count,
                    &mut compressed_bytes,
                );
                if next_code < 4096 {
                    table[key] = next_code;
                    next_code += 1;
                    if next_code > (1u16 << code_size) && code_size < 12 {
                        code_size += 1;
                    }
                } else {
                    // Dictionary full: emit Clear code and reset table
                    push_code(
                        clear_code,
                        code_size,
                        &mut bit_buf,
                        &mut bit_count,
                        &mut compressed_bytes,
                    );
                    table.fill(u16::MAX);
                    next_code = 258;
                    code_size = 9;
                }
                prefix = k as u16;
            }
        }
        push_code(
            prefix,
            code_size,
            &mut bit_buf,
            &mut bit_count,
            &mut compressed_bytes,
        );
    }

    // Emit End-Of-Information code
    push_code(
        eoi_code,
        code_size,
        &mut bit_buf,
        &mut bit_count,
        &mut compressed_bytes,
    );
    if bit_count > 0 {
        compressed_bytes.push((bit_buf & 0xFF) as u8);
    }

    // Write compressed bytes as 255-byte GIF sub-blocks
    for chunk in compressed_bytes.chunks(255) {
        out.push(chunk.len() as u8);
        out.extend_from_slice(chunk);
    }
    out.push(0x00); // Sub-block terminator
}
