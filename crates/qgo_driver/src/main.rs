use std::env;
use std::fs;
use std::path::PathBuf;
use std::process;
use qgo_driver::{
    build_native_executable, compile_str, format_str, run_main_vm, run_tests_vm,
    simulate_and_verify, visualize_gif, TargetBackend,
};
use qgo_lsp::publish_diagnostics_json;
use qgo_opt::SynthMode;

fn print_usage() {
    eprintln!("QGo Quantum Compiler (qgo v0.1.0)");
    eprintln!("Usage:");
    eprintln!("  qgo build <file.qg> [--target=qasm|qir|hugr|rs|native] [-o <out>] [--synth-mode=width|depth]");
    eprintln!("  qgo run   <file.qg> [--synth-mode=width|depth]");
    eprintln!("  qgo test  <file.qg>");
    eprintln!("  qgo estimate <file.qg> [--synth-mode=width|depth]");
    eprintln!("  qgo viz   <file.qg> [func_name] [-o <out.gif>]");
    eprintln!("  qgo fmt   <file.qg>");
    eprintln!("  qgo lsp   <file.qg>");
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        print_usage();
        process::exit(1);
    }

    let subcmd = args[1].as_str();
    let filepath = &args[2];
    let mut target = TargetBackend::OpenQasm3;
    let mut synth_mode = SynthMode::Width;
    let mut native_bin = false;
    let mut out_path: Option<PathBuf> = None;

    let mut i = 3;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--target=qir" {
            target = TargetBackend::Qir;
        } else if arg == "--target=hugr" {
            target = TargetBackend::Hugr;
        } else if arg == "--target=qasm" {
            target = TargetBackend::OpenQasm3;
        } else if arg == "--target=rs" {
            target = TargetBackend::Rust;
        } else if arg == "--target=native" {
            target = TargetBackend::Rust;
            native_bin = true;
        } else if arg == "--synth-mode=depth" {
            synth_mode = SynthMode::Depth;
        } else if arg == "--synth-mode=width" {
            synth_mode = SynthMode::Width;
        } else if arg == "-o" && i + 1 < args.len() {
            out_path = Some(PathBuf::from(&args[i + 1]));
            i += 1;
        }
        i += 1;
    }

    let src = match fs::read_to_string(filepath) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: failed to read `{}`: {}", filepath, e);
            process::exit(1);
        }
    };

    match subcmd {
        "build" => {
            if native_bin {
                let bin_dest = out_path.unwrap_or_else(|| PathBuf::from("a.out"));
                if let Err(err) = build_native_executable(filepath, &src, synth_mode, &bin_dest) {
                    eprintln!("{}", err);
                    process::exit(1);
                }
            } else {
                match compile_str(filepath, &src, target, synth_mode) {
                    Ok(out) => {
                        if let Some(dest) = out_path {
                            let _ = fs::write(dest, &out.artifact);
                        } else {
                            print!("{}", out.artifact);
                        }
                    }
                    Err((sm, diags)) => {
                        for d in diags {
                            eprint!("{}", d.render(&sm));
                        }
                        process::exit(1);
                    }
                }
            }
        }
        "estimate" => match compile_str(filepath, &src, target, synth_mode) {
            Ok(out) => {
                println!("=== QGo Quantum Resource Estimate ({}) ===", filepath);
                println!("Logical Qubits     : {}", out.resources.logical_qubits);
                println!("Peak Ancilla Qubits: {}", out.resources.ancilla_qubits);
                println!("Recycled Ancillas  : {}", out.resources.recycled_ancillas);
                println!("1-Qubit Gates      : {}", out.resources.single_qubit_gates);
                println!("2-Qubit Gates      : {}", out.resources.two_qubit_gates);
                println!("Toffoli (CCX) Gates: {}", out.resources.toffoli_gates);
                println!("Estimated T-Count  : {}", out.resources.t_count);
                println!("Circuit Gate Depth : {}", out.resources.circuit_depth);
            }
            Err((sm, diags)) => {
                for d in diags {
                    eprint!("{}", d.render(&sm));
                }
                process::exit(1);
            }
        },
        "run" => match compile_str(filepath, &src, target, synth_mode) {
            Ok(out) => {
                let extra_args: Vec<String> = args[3..]
                    .iter()
                    .filter(|a| !a.starts_with("--target=") && !a.starts_with("--synth-mode="))
                    .cloned()
                    .collect();
                match run_main_vm(&out.ast, synth_mode, extra_args) {
                    Ok(Some(stdout)) => {
                        print!("{}", stdout);
                    }
                    Ok(None) => match simulate_and_verify(&out.mir) {
                        Ok(runs) => {
                            for (fname, nq) in runs {
                                println!(
                                    "[PASS] {} ({} qubits simulated, all ancillas verified |0>)",
                                    fname, nq
                                );
                            }
                        }
                        Err(msg) => {
                            eprintln!("[FAIL] {}", msg);
                            process::exit(1);
                        }
                    },
                    Err(msg) => {
                        eprintln!("[FAIL] {}", msg);
                        process::exit(1);
                    }
                }
            }
            Err((sm, diags)) => {
                for d in diags {
                    eprint!("{}", d.render(&sm));
                }
                process::exit(1);
            }
        },
        "test" => match compile_str(filepath, &src, target, synth_mode) {
            Ok(out) => {
                match simulate_and_verify(&out.mir) {
                    Ok(runs) => {
                        for (fname, nq) in runs {
                            println!(
                                "[PASS] {} ({} qubits simulated, all ancillas verified |0>)",
                                fname, nq
                            );
                        }
                    }
                    Err(msg) => {
                        eprintln!("[FAIL] {}", msg);
                        process::exit(1);
                    }
                }
                match run_tests_vm(&out.ast, synth_mode) {
                    Ok(tests) => {
                        for tname in tests {
                            println!("[PASS] {} (QGoVM statevector + classical assertions)", tname);
                        }
                    }
                    Err(msg) => {
                        eprintln!("[FAIL] {}", msg);
                        process::exit(1);
                    }
                }
            }
            Err((sm, diags)) => {
                for d in diags {
                    eprint!("{}", d.render(&sm));
                }
                process::exit(1);
            }
        },
        "viz" => {
            let func_arg = args[3..]
                .iter()
                .find(|a| !a.starts_with('-') && Some(PathBuf::from(a.as_str())) != out_path)
                .map(|s| s.as_str());
            let gif_dest = out_path.unwrap_or_else(|| PathBuf::from("memory_trace.gif"));
            match visualize_gif(filepath, &src, func_arg, None, synth_mode, &gif_dest) {
                Ok((fname, nframes, nbytes)) => {
                    println!(
                        "[VIZ] Generated {:?} for `{}` ({} frames, {} bytes)",
                        gif_dest, fname, nframes, nbytes
                    );
                }
                Err(msg) => {
                    eprintln!("{}", msg);
                    process::exit(1);
                }
            }
        }
        "fmt" => match format_str(filepath, &src) {
            Ok(formatted) => print!("{}", formatted),
            Err((sm, diags)) => {
                for d in diags {
                    eprint!("{}", d.render(&sm));
                }
                process::exit(1);
            }
        },
        "lsp" => {
            println!("{}", publish_diagnostics_json(filepath, &src));
        }
        _ => {
            print_usage();
            process::exit(1);
        }
    }
}
