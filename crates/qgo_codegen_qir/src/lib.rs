//! `qgo_codegen_qir`: Pass 5.2 QIR (Quantum Intermediate Representation) LLVM IR
//! and HUGR (Hierarchical Unified Graph Representation) emitter.
//! Conforms to the QIR Base/Adaptive Profile (`%Qubit*`, `%Result*`,
//! `__quantum__qis__h__body`, `__quantum__qis__cnot__body`, `__quantum__rt__qubit_allocate`).

pub mod rust_backend;
pub use rust_backend::emit_rust_aot;

use std::collections::HashMap;
use qgo_mir::{MirFunc, MirGateKind, MirModule, MirStmt, QubitWire};
use qgo_opt::flatten_conjugates;

/// Emits LLVM IR conforming to the QIR Base/Adaptive Profile.
pub fn emit_qir_llvm(module: &MirModule) -> String {
    let mut out = String::new();
    out.push_str("; ModuleID = 'qgo_qir_module'\n");
    out.push_str(&format!("source_filename = \"{}.qg\"\n\n", module.package));
    out.push_str("%Qubit = type opaque\n");
    out.push_str("%Result = type opaque\n\n");

    // External QIR runtime and intrinsic declarations
    out.push_str("declare %Qubit* @__quantum__rt__qubit_allocate()\n");
    out.push_str("declare void @__quantum__rt__qubit_release(%Qubit*)\n");
    out.push_str("declare void @__quantum__qis__h__body(%Qubit*)\n");
    out.push_str("declare void @__quantum__qis__x__body(%Qubit*)\n");
    out.push_str("declare void @__quantum__qis__y__body(%Qubit*)\n");
    out.push_str("declare void @__quantum__qis__z__body(%Qubit*)\n");
    out.push_str("declare void @__quantum__qis__s__body(%Qubit*)\n");
    out.push_str("declare void @__quantum__qis__s__adj(%Qubit*)\n");
    out.push_str("declare void @__quantum__qis__t__body(%Qubit*)\n");
    out.push_str("declare void @__quantum__qis__t__adj(%Qubit*)\n");
    out.push_str("declare void @__quantum__qis__rx__body(double, %Qubit*)\n");
    out.push_str("declare void @__quantum__qis__ry__body(double, %Qubit*)\n");
    out.push_str("declare void @__quantum__qis__rz__body(double, %Qubit*)\n");
    out.push_str("declare void @__quantum__qis__cnot__body(%Qubit*, %Qubit*)\n");
    out.push_str("declare void @__quantum__qis__ccx__body(%Qubit*, %Qubit*, %Qubit*)\n");
    out.push_str("declare void @__quantum__qis__swap__body(%Qubit*, %Qubit*)\n");
    out.push_str("declare %Result* @__quantum__qis__mz__body(%Qubit*)\n");
    out.push_str("declare void @__quantum__qis__reset__body(%Qubit*)\n");
    out.push_str("declare i1 @__quantum__rt__result_equal(%Result*, %Result*)\n");
    out.push_str("declare %Result* @__quantum__rt__result_get_one()\n\n");

    for func in &module.funcs {
        emit_func_qir(func, &mut out);
        out.push('\n');
    }

    out
}

fn alloc_wire(
    w: &QubitWire,
    wire_ssa: &mut HashMap<QubitWire, String>,
    next_ssa: &mut usize,
    out: &mut String,
) -> String {
    if let Some(existing) = wire_ssa.get(w) {
        return existing.clone();
    }
    let ssa = format!("%q_{}_{}_{}", w.reg, w.index, *next_ssa);
    *next_ssa += 1;
    out.push_str(&format!(
        "  {} = call %Qubit* @__quantum__rt__qubit_allocate()\n",
        ssa
    ));
    wire_ssa.insert(w.clone(), ssa.clone());
    ssa
}

fn emit_func_qir(func: &MirFunc, out: &mut String) {
    out.push_str(&format!(
        "define void @{}() #0 {{\nentry:\n",
        func.name
    ));
    let mut wire_ssa: HashMap<QubitWire, String> = HashMap::new();
    let mut next_ssa = 0usize;

    for p in &func.params {
        if p.is_quantum {
            for b in 0..p.width.max(1) {
                let w = QubitWire::new(p.name.clone(), b);
                let _ = alloc_wire(&w, &mut wire_ssa, &mut next_ssa, out);
            }
        }
    }

    let flat = flatten_conjugates(&func.body);
    for stmt in &flat {
        match stmt {
            MirStmt::QAlloc { reg, size, .. } => {
                for b in 0..(*size).max(1) {
                    let w = QubitWire::new(reg.clone(), b);
                    let _ = alloc_wire(&w, &mut wire_ssa, &mut next_ssa, out);
                }
            }
            MirStmt::QFree { reg, size } => {
                for b in 0..(*size).max(1) {
                    let w = QubitWire::new(reg.clone(), b);
                    if let Some(ssa) = wire_ssa.remove(&w) {
                        out.push_str(&format!(
                            "  call void @__quantum__rt__qubit_release(%Qubit* {})\n",
                            ssa
                        ));
                    }
                }
            }
            MirStmt::Gate {
                kind,
                controls,
                targets,
            } => {
                let mut c_ssas = Vec::new();
                for c in controls {
                    c_ssas.push(alloc_wire(c, &mut wire_ssa, &mut next_ssa, out));
                }
                let mut t_ssas = Vec::new();
                for t in targets {
                    t_ssas.push(alloc_wire(t, &mut wire_ssa, &mut next_ssa, out));
                }
                let Some(t0) = t_ssas.first() else { continue };

                match (kind, c_ssas.as_slice()) {
                    (MirGateKind::H, []) => {
                        out.push_str(&format!(
                            "  call void @__quantum__qis__h__body(%Qubit* {})\n",
                            t0
                        ));
                    }
                    (MirGateKind::X, []) => {
                        out.push_str(&format!(
                            "  call void @__quantum__qis__x__body(%Qubit* {})\n",
                            t0
                        ));
                    }
                    (MirGateKind::X, [c0]) => {
                        out.push_str(&format!(
                            "  call void @__quantum__qis__cnot__body(%Qubit* {}, %Qubit* {})\n",
                            c0, t0
                        ));
                    }
                    (MirGateKind::X, [c0, c1, ..]) => {
                        out.push_str(&format!(
                            "  call void @__quantum__qis__ccx__body(%Qubit* {}, %Qubit* {}, %Qubit* {})\n",
                            c0, c1, t0
                        ));
                    }
                    (MirGateKind::Y, _) => {
                        out.push_str(&format!(
                            "  call void @__quantum__qis__y__body(%Qubit* {})\n",
                            t0
                        ));
                    }
                    (MirGateKind::Z, _) => {
                        out.push_str(&format!(
                            "  call void @__quantum__qis__z__body(%Qubit* {})\n",
                            t0
                        ));
                    }
                    (MirGateKind::S, _) => {
                        out.push_str(&format!(
                            "  call void @__quantum__qis__s__body(%Qubit* {})\n",
                            t0
                        ));
                    }
                    (MirGateKind::Sdg, _) => {
                        out.push_str(&format!(
                            "  call void @__quantum__qis__s__adj(%Qubit* {})\n",
                            t0
                        ));
                    }
                    (MirGateKind::T, _) => {
                        out.push_str(&format!(
                            "  call void @__quantum__qis__t__body(%Qubit* {})\n",
                            t0
                        ));
                    }
                    (MirGateKind::Tdg, _) => {
                        out.push_str(&format!(
                            "  call void @__quantum__qis__t__adj(%Qubit* {})\n",
                            t0
                        ));
                    }
                    (MirGateKind::Rx(theta), _) => {
                        out.push_str(&format!(
                            "  call void @__quantum__qis__rx__body(double {:.8}, %Qubit* {})\n",
                            theta, t0
                        ));
                    }
                    (MirGateKind::Ry(theta), _) => {
                        out.push_str(&format!(
                            "  call void @__quantum__qis__ry__body(double {:.8}, %Qubit* {})\n",
                            theta, t0
                        ));
                    }
                    (MirGateKind::Rz(theta) | MirGateKind::Phase(theta), _) => {
                        out.push_str(&format!(
                            "  call void @__quantum__qis__rz__body(double {:.8}, %Qubit* {})\n",
                            theta, t0
                        ));
                    }
                    (MirGateKind::Swap, _) if t_ssas.len() >= 2 => {
                        out.push_str(&format!(
                            "  call void @__quantum__qis__swap__body(%Qubit* {}, %Qubit* {})\n",
                            t_ssas[0], t_ssas[1]
                        ));
                    }
                    _ => {}
                }
            }
            MirStmt::Measure { qubit, dst_var } => {
                let q_ssa = alloc_wire(qubit, &mut wire_ssa, &mut next_ssa, out);
                out.push_str(&format!(
                    "  %res_{} = call %Result* @__quantum__qis__mz__body(%Qubit* {})\n",
                    dst_var, q_ssa
                ));
            }
            MirStmt::Reset { qubit } => {
                let q_ssa = alloc_wire(qubit, &mut wire_ssa, &mut next_ssa, out);
                out.push_str(&format!(
                    "  call void @__quantum__qis__reset__body(%Qubit* {})\n",
                    q_ssa
                ));
            }
            _ => {}
        }
    }

    out.push_str("  ret void\n}\n\n");
    out.push_str("attributes #0 = { \"EntryPoint\" \"qir_profiles\"=\"adaptive_profile\" }\n");
}

/// Emits a serialized HUGR (Hierarchical Unified Graph Representation) JSON envelope for `MirModule`.
pub fn emit_hugr_json(module: &MirModule) -> String {
    let mut nodes = Vec::new();
    for func in &module.funcs {
        let flat = flatten_conjugates(&func.body);
        nodes.push(format!(
            "    {{\"func\": \"{}\", \"purity\": \"{}\", \"op_count\": {}}}",
            func.name,
            func.purity,
            flat.len()
        ));
    }
    format!(
        "{{\n  \"version\": \"hugr-v1\",\n  \"package\": \"{}\",\n  \"modules\": [\n{}\n  ]\n}}\n",
        module.package,
        nodes.join(",\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use qgo_hir::check_module;
    use qgo_mir::lower_to_mir;
    use qgo_span::FileId;
    use qgo_syntax::parse_source;

    #[test]
    fn test_emit_qir_llvm_profile() {
        let src = r#"package main
func Entangle() {
    q := qalloc[2]()
    H(q[0])
    CX(q[0], q[1])
    b := measure(q[0])
}
"#;
        let (ast, _) = parse_source(FileId(0), src);
        let (hir, diags) = check_module(&ast);
        assert!(diags.is_empty());
        let mir = lower_to_mir(&hir);
        let qir = emit_qir_llvm(&mir);
        assert!(qir.contains("%Qubit = type opaque"));
        assert!(qir.contains("@__quantum__rt__qubit_allocate()"));
        assert!(qir.contains("@__quantum__qis__h__body"));
        assert!(qir.contains("@__quantum__qis__cnot__body"));
        assert!(qir.contains("@__quantum__qis__mz__body"));
    }
}
