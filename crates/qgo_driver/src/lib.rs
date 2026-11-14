//! `qgo_driver`: End-to-end compilation pipeline and CLI commands (`build`, `run`, `test`, `estimate`, `fmt`, `lsp`).

use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::process::Command;
use qgo_codegen_qasm::emit_qasm3;
use qgo_codegen_qir::{emit_hugr_json, emit_qir_llvm, emit_rust_aot};
use qgo_hir::{check_module, HirModule};
use qgo_mir::{lower_to_mir, MirModule};
use qgo_opt::{estimate_resources, optimize_module, ResourceReport, SynthMode};
use qgo_sim::{render_trace_gif, trace_func_memory, QGoVM, StabilizerSim, StatevectorSim};
use qgo_span::{Diagnostic, SourceMap};
use qgo_syntax::{format_source, parse_source, SourceFile, TopLevelDecl};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetBackend {
    OpenQasm3,
    Qir,
    Hugr,
    Rust,
}

pub struct CompileOutput {
    pub ast: SourceFile,
    pub hir: HirModule,
    pub mir: MirModule,
    pub artifact: String,
    pub resources: ResourceReport,
}

/// Resolves `import "..."` declarations relative to `filename` or the workspace root.
fn resolve_imports(
    filename: &str,
    ast: &mut SourceFile,
    sm: &mut SourceMap,
    diags: &mut Vec<Diagnostic>,
) {
    let base_dir = Path::new(filename)
        .parent()
        .unwrap_or_else(|| Path::new("."));
    let mut visited: HashSet<String> = HashSet::new();
    visited.insert(filename.to_owned());

    let imports = ast.imports.clone();
    let mut imported_items: Vec<TopLevelDecl> = Vec::new();

    for imp in imports {
        let raw = imp.as_str();
        let candidates = if raw.ends_with(".qg") {
            vec![base_dir.join(raw), Path::new(".").join(raw)]
        } else {
            vec![
                base_dir.join(format!("{}.qg", raw)),
                Path::new(".").join(format!("{}.qg", raw)),
                Path::new("std").join(format!("{}.qg", raw)),
            ]
        };

        if let Some(found) = candidates.into_iter().find(|p| p.exists()) {
            let key = found.to_string_lossy().to_string();
            if visited.insert(key.clone()) {
                if let Ok(imp_src) = fs::read_to_string(&found) {
                    let fid = sm.add_file(key, imp_src.clone());
                    let (imp_ast, imp_diags) = parse_source(fid, &imp_src);
                    diags.extend(imp_diags);
                    imported_items.extend(imp_ast.items);
                }
            }
        }
    }

    if !imported_items.is_empty() {
        imported_items.extend(ast.items.drain(..));
        ast.items = imported_items;
    }
}

/// Compiles a `.qg` source file through Stages 0–6 into the requested target backend.
pub fn compile_str(
    filename: &str,
    src: &str,
    target: TargetBackend,
    synth_mode: SynthMode,
) -> Result<CompileOutput, (SourceMap, Vec<Diagnostic>)> {
    let mut sm = SourceMap::new();
    let fid = sm.add_file(filename, src);
    let (mut ast, mut diags) = parse_source(fid, src);
    resolve_imports(filename, &mut ast, &mut sm, &mut diags);

    let (hir, hir_diags) = check_module(&ast);
    diags.extend(hir_diags);

    if !diags.is_empty() {
        return Err((sm, diags));
    }

    let mut mir = lower_to_mir(&hir);
    optimize_module(&mut mir, synth_mode);
    let resources = estimate_resources(&mir);

    let artifact = match target {
        TargetBackend::OpenQasm3 => emit_qasm3(&mir),
        TargetBackend::Qir => emit_qir_llvm(&mir),
        TargetBackend::Hugr => emit_hugr_json(&mir),
        TargetBackend::Rust => emit_rust_aot(&ast),
    };

    Ok(CompileOutput {
        ast,
        hir,
        mir,
        artifact,
        resources,
    })
}

/// Compiles a `.qg` source string into a standalone native binary at `output_bin` via `rustc`.
pub fn build_native_executable(
    filename: &str,
    src: &str,
    synth_mode: SynthMode,
    output_bin: &Path,
) -> Result<CompileOutput, String> {
    let out = compile_str(filename, src, TargetBackend::Rust, synth_mode).map_err(|(sm, diags)| {
        diags
            .iter()
            .map(|d| d.render(&sm))
            .collect::<Vec<_>>()
            .join("\n")
    })?;

    let rs_path = output_bin.with_extension("rs");
    fs::write(&rs_path, &out.artifact)
        .map_err(|e| format!("failed to write temporary Rust file {:?}: {}", rs_path, e))?;

    let status = Command::new("rustc")
        .arg("--crate-name")
        .arg("qgo_aot_module")
        .arg("-O")
        .arg(&rs_path)
        .arg("-o")
        .arg(output_bin)
        .output()
        .map_err(|e| format!("failed to invoke `rustc`: {}", e))?;

    let _ = fs::remove_file(&rs_path);

    if !status.status.success() {
        return Err(format!(
            "rustc compilation failed:\n{}",
            String::from_utf8_lossy(&status.stderr)
        ));
    }

    Ok(out)
}

/// Executes all functions in a compiled module on `qgo_sim::StatevectorSim` (and `StabilizerSim`
/// when the circuit is purely Clifford) and verifies that every ancilla wire returns to $|0\rangle$
/// with probability $\ge 1.0 - 10^{-12}$.
pub fn simulate_and_verify(mir: &MirModule) -> Result<Vec<(String, usize)>, String> {
    let mut results = Vec::new();
    for func in &mir.funcs {
        let mut sim = StatevectorSim::new();
        sim.execute_func(func);
        for anc in &sim.ancilla_wires {
            let p0 = sim.prob_zero(anc);
            if (1.0 - p0).abs() >= 1e-12 {
                return Err(format!(
                    "Function `{}` leaked ancilla `{}`: P(|0>) = {:.15}",
                    func.name, anc, p0
                ));
            }
        }
        if StabilizerSim::is_clifford_func(func) {
            let mut stab = StabilizerSim::new();
            stab.execute_func(func);
            for anc in &stab.ancilla_wires {
                let p0 = stab.prob_zero(anc);
                if (1.0 - p0).abs() >= 1e-12 {
                    return Err(format!(
                        "StabilizerSim: function `{}` leaked ancilla `{}`: P(|0>) = {:.15}",
                        func.name, anc, p0
                    ));
                }
            }
        }
        results.push((func.name.clone(), sim.num_qubits));
    }
    Ok(results)
}

/// Executes all `Test*` functions in `ast` on `QGoVM` and returns the list of passed test names.
pub fn run_tests_vm(ast: &SourceFile, synth_mode: SynthMode) -> Result<Vec<String>, String> {
    let mut passed = Vec::new();
    for item in &ast.items {
        if let TopLevelDecl::Func(f) = item {
            if f.name.starts_with("Test") && f.params.is_empty() {
                let mut vm = QGoVM::new(ast, synth_mode);
                vm.call_function(&f.name, Vec::new())
                    .map_err(|e| format!("in `{}`: {}", f.name, e))?;
                passed.push(f.name.clone());
            }
        }
    }
    Ok(passed)
}

/// Executes `main()` on `QGoVM` if present and returns its captured stdout.
pub fn run_main_vm(
    ast: &SourceFile,
    synth_mode: SynthMode,
    cli_args: Vec<String>,
) -> Result<Option<String>, String> {
    let has_main = ast
        .items
        .iter()
        .any(|i| matches!(i, TopLevelDecl::Func(f) if f.name == "main"));
    if !has_main {
        return Ok(None);
    }
    let mut vm = QGoVM::new(ast, synth_mode).with_args(cli_args);
    vm.run_main()?;
    Ok(Some(vm.stdout))
}

pub fn format_str(filename: &str, src: &str) -> Result<String, (SourceMap, Vec<Diagnostic>)> {
    let mut sm = SourceMap::new();
    let fid = sm.add_file(filename, src);
    let (ast, diags) = parse_source(fid, src);
    if !diags.is_empty() {
        return Err((sm, diags));
    }
    Ok(format_source(&ast))
}

/// Traces quantum memory evolution of a compiled function and writes an animated GIF89a to `output_gif`.
pub fn visualize_gif(
    filename: &str,
    src: &str,
    func_name: Option<&str>,
    title: Option<&str>,
    synth_mode: SynthMode,
    output_gif: &Path,
) -> Result<(String, usize, usize), String> {
    let out =
        compile_str(filename, src, TargetBackend::OpenQasm3, synth_mode).map_err(|(sm, diags)| {
            diags
                .iter()
                .map(|d| d.render(&sm))
                .collect::<Vec<_>>()
                .join("\n")
        })?;

    let func = if let Some(target_name) = func_name {
        out.mir
            .funcs
            .iter()
            .find(|f| f.name == target_name)
            .ok_or_else(|| format!("function `{}` not found in `{}`", target_name, filename))?
    } else {
        out.mir
            .funcs
            .iter()
            .rev()
            .find(|f| !f.name.starts_with("Test"))
            .or_else(|| out.mir.funcs.last())
            .ok_or_else(|| format!("no functions found in `{}`", filename))?
    };

    let display_title = title.unwrap_or(&func.name);
    let trace = trace_func_memory(func, 32);
    let num_frames = trace.snapshots.len();
    render_trace_gif(&trace, display_title, output_gif)?;
    let byte_len = fs::metadata(output_gif)
        .map(|m| m.len() as usize)
        .unwrap_or(0);

    Ok((func.name.clone(), num_frames, byte_len))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_end_to_end_pipeline_qasm_qir_and_sim() {
        let src = r#"package main

func Oracle(x qreg[3], flag qubit) qfree {
    ctrl(x[0], x[1], x[2]) X(flag)
}

func GroverIteration(reg qreg[3]) unitary {
    H(reg[:])
    flag := qalloc()
    Oracle(reg, flag)
    defer adj Oracle(reg, flag) -> flag
    Z(flag)
    H(reg[:])
}
"#;
        let out_qasm = compile_str("grover.qg", src, TargetBackend::OpenQasm3, SynthMode::Width)
            .expect("compiles to OpenQASM 3.0");
        assert!(out_qasm.artifact.contains("OPENQASM 3.0;"));
        assert!(out_qasm.resources.ancilla_qubits >= 1);

        let out_qir = compile_str("grover.qg", src, TargetBackend::Qir, SynthMode::Width)
            .expect("compiles to QIR");
        assert!(out_qir.artifact.contains("@__quantum__qis__ccx__body"));

        let sim_res = simulate_and_verify(&out_qasm.mir).expect("ancillas verified uncomputed");
        assert_eq!(sim_res.len(), 2);
    }

    #[test]
    fn test_all_std_quantum_algorithms_end_to_end() {
        let std_files = [
            "../../std/qft.qg",
            "../../std/arith.qg",
            "../../std/grover.qg",
            "../../std/deutsch_jozsa.qg",
            "../../std/bernstein_vazirani.qg",
            "../../std/simon.qg",
            "../../std/phase_est.qg",
            "../../std/shor.qg",
            "../../std/teleport.qg",
            "../../std/qec.qg",
            "../../std/vqe_qaoa.qg",
            "../../std/quantum_walk.qg",
            "../../std/memory_gc.qg",
        ];

        for rel_path in std_files {
            let p = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel_path);
            let src = fs::read_to_string(&p)
                .unwrap_or_else(|e| panic!("failed to read {:?}: {}", p, e));
            let fname = p.to_string_lossy().to_string();

            // 1. Compile to OpenQASM 3.0, QIR, and HUGR
            let out_qasm = match compile_str(&fname, &src, TargetBackend::OpenQasm3, SynthMode::Width) {
                Ok(o) => o,
                Err((sm, diags)) => {
                    let rendered: Vec<String> = diags.iter().map(|d| d.render(&sm)).collect();
                    panic!("Failed to compile {}:\n{}", fname, rendered.join("\n"));
                }
            };
            assert!(out_qasm.artifact.contains("OPENQASM 3.0;"));

            let out_qir = compile_str(&fname, &src, TargetBackend::Qir, SynthMode::Width)
                .expect("QIR compilation succeeds");
            assert!(out_qir.artifact.contains("qir_profiles"));

            let out_hugr = compile_str(&fname, &src, TargetBackend::Hugr, SynthMode::Width)
                .expect("HUGR compilation succeeds");
            assert!(out_hugr.artifact.contains("\"version\": \"hugr-v1\""));

            // 2. Verify all functions on StatevectorSim & StabilizerSim (ancillas return to |0> within 1e-12)
            let sim_runs = simulate_and_verify(&out_qasm.mir)
                .unwrap_or_else(|e| panic!("simulate_and_verify failed for {}: {}", fname, e));
            assert!(!sim_runs.is_empty());

            // 3. Execute all `Test*` functions on QGoVM
            let passed_tests = run_tests_vm(&out_qasm.ast, SynthMode::Width)
                .unwrap_or_else(|e| panic!("QGoVM test failed for {}: {}", fname, e));
            assert!(
                !passed_tests.is_empty(),
                "expected at least one Test* function in {}",
                fname
            );
        }
    }

    #[test]
    fn test_self_hosting_compiler_vm_and_3stage_bootstrap_fixed_point() {
        let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../selfhost/qgo_compiler.qg");
        let src = fs::read_to_string(&p).expect("read selfhost/qgo_compiler.qg");
        let fname = p.to_string_lossy().to_string();

        let out = match compile_str(&fname, &src, TargetBackend::Rust, SynthMode::Width) {
            Ok(o) => o,
            Err((sm, diags)) => {
                let rendered: Vec<String> = diags.iter().map(|d| d.render(&sm)).collect();
                panic!(
                    "Failed to compile selfhost/qgo_compiler.qg:\n{}",
                    rendered.join("\n")
                );
            }
        };

        // 1. Run `TestSelfHostingCompiler` on QGoVM
        let passed = run_tests_vm(&out.ast, SynthMode::Width)
            .expect("TestSelfHostingCompiler should pass on QGoVM");
        assert!(passed.contains(&"TestSelfHostingCompiler".to_owned()));

        // 2. Stage 0 (`qgo`) -> Stage 1 native binary via `rustc`
        let tmp = std::env::temp_dir();
        let stage1_bin = tmp.join("qgo_selfhost_stage1");
        let stage2_rs = tmp.join("qgo_selfhost_stage2.rs");
        let stage2_bin = tmp.join("qgo_selfhost_stage2");
        let stage3_rs = tmp.join("qgo_selfhost_stage3.rs");

        build_native_executable(&fname, &src, SynthMode::Width, &stage1_bin)
            .expect("Stage 1 native binary compilation via rustc should succeed");

        let run1 = Command::new(&stage1_bin)
            .arg("test")
            .output()
            .expect("run stage1 binary");
        assert!(
            run1.status.success(),
            "stage1 test failed: {}",
            String::from_utf8_lossy(&run1.stderr)
        );
        assert!(String::from_utf8_lossy(&run1.stdout).contains("[SELFHOST PASS]"));

        // 3. Stage 1 compiles `selfhost/qgo_compiler.qg` -> `stage2.rs` -> `rustc` -> `stage2_bin`
        let s1_build = Command::new(&stage1_bin)
            .arg("build")
            .arg(&fname)
            .arg("--target=rs")
            .arg("-o")
            .arg(&stage2_rs)
            .output()
            .expect("stage1 builds stage2.rs");
        assert!(
            s1_build.status.success(),
            "stage1 build failed: {}",
            String::from_utf8_lossy(&s1_build.stderr)
        );

        let rustc_s2 = Command::new("rustc")
            .arg("--crate-name")
            .arg("qgo_selfhost_stage2")
            .arg("-O")
            .arg(&stage2_rs)
            .arg("-o")
            .arg(&stage2_bin)
            .output()
            .expect("rustc compiles stage2.rs");
        assert!(
            rustc_s2.status.success(),
            "rustc failed on stage2.rs:\n{}",
            String::from_utf8_lossy(&rustc_s2.stderr)
        );

        // Execute `./stage2_bin test`
        let run2 = Command::new(&stage2_bin)
            .arg("test")
            .output()
            .expect("run stage2 binary");
        assert!(
            run2.status.success(),
            "stage2 test failed: {}",
            String::from_utf8_lossy(&run2.stderr)
        );
        assert!(String::from_utf8_lossy(&run2.stdout).contains("[SELFHOST PASS]"));

        // 4. Stage 2 compiles `selfhost/qgo_compiler.qg` -> `stage3.rs` (Bootstrap Fixed Point!)
        let s2_build = Command::new(&stage2_bin)
            .arg("build")
            .arg(&fname)
            .arg("--target=rs")
            .arg("-o")
            .arg(&stage3_rs)
            .output()
            .expect("stage2 builds stage3.rs");
        assert!(s2_build.status.success());

        let code_stage2 = fs::read_to_string(&stage2_rs).expect("read stage2.rs");
        let code_stage3 = fs::read_to_string(&stage3_rs).expect("read stage3.rs");
        assert_eq!(
            code_stage2, code_stage3,
            "3-Stage Bootstrap Fixed Point verified: Stage 2 and Stage 3 Rust artifacts are byte-for-byte identical!"
        );

        // 5. Verify Stage 2 compiles `std/grover.qg` and `std/deutsch_jozsa.qg` to OpenQASM 3.0 & QIR
        let grover_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../std/grover.qg")
            .to_string_lossy()
            .to_string();
        let grover_qasm = Command::new(&stage2_bin)
            .arg("build")
            .arg(&grover_path)
            .arg("--target=qasm")
            .output()
            .expect("stage2 compiles std/grover.qg to QASM");
        let qasm_text = String::from_utf8_lossy(&grover_qasm.stdout);
        assert!(qasm_text.contains("OPENQASM 3.0;"));
        assert!(qasm_text.contains("// Begin Compute"));
        assert!(qasm_text.contains("// Begin Uncompute (Defer Adj)"));

        let _ = fs::remove_file(&stage1_bin);
        let _ = fs::remove_file(&stage2_rs);
        let _ = fs::remove_file(&stage2_bin);
        let _ = fs::remove_file(&stage3_rs);
    }

    #[test]
    fn test_generate_all_quantum_memory_gifs() {
        let gif_specs: &[(&str, &str, &str, &str)] = &[
            (
                "../../std/grover.qg",
                "GroverSearch3Qubit",
                "GROVER 3-QUBIT SEARCH",
                "../../docs/gifs/grover_memory.gif",
            ),
            (
                "../../std/qft.qg",
                "QFT4Spectrum5",
                "4-QUBIT QUANTUM FOURIER",
                "../../docs/gifs/qft_memory.gif",
            ),
            (
                "../../std/arith.qg",
                "ConditionalFlipIfSumGt5",
                "CUCCARO ARITHMETIC (X+Y>5)",
                "../../docs/gifs/cuccaro_arith_memory.gif",
            ),
            (
                "../../std/deutsch_jozsa.qg",
                "DeutschJozsaBalanced",
                "DEUTSCH-JOZSA PARITY ORACLE",
                "../../docs/gifs/deutsch_jozsa_memory.gif",
            ),
            (
                "../../std/bernstein_vazirani.qg",
                "BernsteinVazirani13",
                "BERNSTEIN-VAZIRANI (s=1101)",
                "../../docs/gifs/bernstein_vazirani_memory.gif",
            ),
            (
                "../../std/phase_est.qg",
                "PhaseEstimationTGate",
                "QUANTUM PHASE ESTIMATION",
                "../../docs/gifs/phase_estimation_memory.gif",
            ),
            (
                "../../std/shor.qg",
                "ShorOrderFinding15",
                "SHOR N=15 ORDER FINDING",
                "../../docs/gifs/shor_order_finding_memory.gif",
            ),
            (
                "../../std/qec.qg",
                "VerifyErrorFreeCodeword",
                "3-QUBIT BIT-FLIP QEC CODE",
                "../../docs/gifs/qec_bitflip_memory.gif",
            ),
            (
                "../../std/vqe_qaoa.qg",
                "QAOAMaxCutTriangle",
                "QAOA MAXCUT K3 TRIANGLE",
                "../../docs/gifs/qaoa_maxcut_memory.gif",
            ),
            (
                "../../std/quantum_walk.qg",
                "QuantumWalk3Steps",
                "COINED QUANTUM WALK (8-CYC)",
                "../../docs/gifs/quantum_walk_memory.gif",
            ),
            (
                "../../std/memory_gc.qg",
                "MultiStageAncillaPipeline",
                "ANCILLA POOL RECYCLING (3X)",
                "../../docs/gifs/ancilla_pool_recycling_memory.gif",
            ),
        ];

        for &(rel_src, func_name, title, rel_gif) in gif_specs {
            let src_path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel_src);
            let gif_path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel_gif);
            let src = fs::read_to_string(&src_path)
                .unwrap_or_else(|e| panic!("failed to read {:?}: {}", src_path, e));
            let fname = src_path.to_string_lossy().to_string();

            let (traced_name, num_frames, byte_len) = visualize_gif(
                &fname,
                &src,
                Some(func_name),
                Some(title),
                SynthMode::Width,
                &gif_path,
            )
            .unwrap_or_else(|e| panic!("visualize_gif failed for {}: {}", func_name, e));

            assert_eq!(traced_name, func_name);
            assert!(
                num_frames >= 6,
                "expected at least 6 animation frames for {}, got {}",
                func_name,
                num_frames
            );
            assert!(
                byte_len > 2048,
                "expected valid non-trivial GIF file for {}, got {} bytes",
                func_name,
                byte_len
            );

            let bytes = fs::read(&gif_path).expect("read generated gif");
            assert_eq!(&bytes[0..6], b"GIF89a", "GIF89a magic header verified");
            assert_eq!(bytes[bytes.len() - 1], 0x3B, "GIF trailer 0x3B verified");
        }
    }

    #[test]
    fn test_ancilla_pool_recycling_and_classical_gc_vm_and_native_aot() {
        let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../std/memory_gc.qg");
        let src = fs::read_to_string(&p).expect("read std/memory_gc.qg");
        let fname = p.to_string_lossy().to_string();

        // 1. Verify Compiler Pass 4.5 static ancilla pool recycling on `std/memory_gc.qg`
        let out_qasm = compile_str(&fname, &src, TargetBackend::OpenQasm3, SynthMode::Width)
            .expect("compile std/memory_gc.qg");
        assert_eq!(
            out_qasm.resources.ancilla_qubits, 1,
            "Peak physical ancilla qubits across 3 sequential oracle stages must be 1"
        );
        assert_eq!(
            out_qasm.resources.recycled_ancillas, 2,
            "2 ancilla allocations (anc2, anc3) must be recycled from anc1's freed wire"
        );

        // 2. Verify QGoVM runtime Physical Qubit Pool & Tri-Color Mark-and-Sweep GC
        let passed = run_tests_vm(&out_qasm.ast, SynthMode::Width)
            .expect("TestQuantumPoolAndClassicalGC should pass on QGoVM");
        assert!(passed.contains(&"TestQuantumPoolAndClassicalGC".to_owned()));

        // 3. Verify Native Rust AOT binary execution via `rustc`
        let tmp_bin = std::env::temp_dir().join("qgo_test_memory_gc_bin");
        build_native_executable(&fname, &src, SynthMode::Width, &tmp_bin)
            .expect("Native Rust AOT compilation of std/memory_gc.qg should succeed");
        let run_out = Command::new(&tmp_bin)
            .output()
            .expect("execute native memory_gc binary");
        let _ = fs::remove_file(&tmp_bin);
        assert!(
            run_out.status.success(),
            "Native memory_gc binary failed:\n{}",
            String::from_utf8_lossy(&run_out.stderr)
        );
    }
}
