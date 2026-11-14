# Autonomous Quantum Compiler Architect (`QGo`)

This repository contains the multi-crate Rust compiler workspace for **QGo** (`.qg`), a high-level, statically-typed, Go-inspired quantum programming language that lowers high-level quantum abstractions into **OpenQASM 3.0** and **QIR (Quantum Intermediate Representation) / HUGR**.

## Workspace Layout

- `crates/qgo_span`: Source spans, file IDs, string interning (`Interner`), and Ariadne-style diagnostics (`Diagnostic::render`).
- `crates/qgo_lexer`: Tokenizer and Pass 1 Go Automatic Semicolon Insertion (ASI) filter.
- `crates/qgo_syntax`: Typed AST, recursive-descent + Pratt expression parser, and `qgo fmt` canonical code formatter.
- `crates/qgo_hir`: Symbol resolution, type inference, Purity hierarchy (`qfree` < `unitary` < `hybrid`), Disjoint Aliasing Checker (`E0400`), Linear Copy Checker (`E0402`), and CFG Ancilla-Leak Checker (`E0401`).
- `crates/qgo_mir`: Mid-Level Quantum IR featuring explicit `MirStmt::Conjugate` nodes (`defer adj` lowering) and structural adjoint inversion.
- `crates/qgo_opt`: Algebraic passes ($U \cdot U^\dagger$ cancellation, `ctrl`-conjugation theorem $\text{ctrl}_c(U V U^\dagger) = U (\text{ctrl}_c V) U^\dagger$, Cuccaro ripple-carry & Draper QFT arithmetic synthesis, and static resource estimation).
- `crates/qgo_codegen_qasm`: OpenQASM 3.0 backend emitter (`OPENQASM 3.0; include "stdgates.inc";`).
- `crates/qgo_codegen_qir`: QIR Base/Adaptive Profile LLVM IR and HUGR JSON emitter.
- `crates/qgo_sim`: Exact complex statevector simulator verifying circuit semantics and ancilla uncomputation ($P(|0\rangle) \ge 1 - 10^{-12}$).
- `crates/qgo_lsp`: Language Server Protocol (JSON-RPC 2.0) diagnostics engine.
- `crates/qgo_driver`: CLI binary (`qgo build`, `qgo run`, `qgo test`, `qgo estimate`, `qgo fmt`, `qgo lsp`).
- `std/`: Standard library (`qft.qg`, `arith.qg`, `grover.qg`, `phase_est.qg`).
