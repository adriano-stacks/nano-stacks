---
id: "130"
title: "Upgrade and minimize the Wasmtime consensus runtime"
status: pending
priority: critical
effort: large
dependencies: ["060", "078"]
tags: ["mainnet", "consensus", "vm", "security", "dependencies"]
created_at: 2026-08-14
parent: 053
type: improvement
---

# Upgrade and minimize the Wasmtime consensus runtime

## Objective

Run consensus WebAssembly on a supported, patched and deliberately configured
Wasmtime rather than inheriting a broad default feature set. Preserve every
Epoch 4.0 state root, receipt, cost and refusal while reducing the JIT and host
surface that can affect the node.

## Tasks

- [ ] Inventory the Wasmtime APIs, Cargo features and WebAssembly proposals used
      by production execution, compilation and the native-module cache. Classify
      every current RustSec advisory by production reachability.
- [ ] Select a supported patched Wasmtime line and update `nano-vm`,
      `nano-wasm-cache` and vendored `clar2wasm` together.
- [ ] Replace `Engine::default()` in production paths with one shared explicit
      engine configuration. Include the complete configuration in the compiler
      and release identity.
- [ ] Disable every unused Cargo feature and WebAssembly proposal, including
      WASI, the component model, Winch, threads, relaxed SIMD, memory64,
      multi-memory, profiling and coredumps unless a checked-in Epoch 4.0 module
      proves it is required.
- [ ] Put explicit limits on store memory, tables and instances. Any independent
      watchdog or fuel exhaustion must stop the node for investigation, never
      turn a host-speed limit into a consensus-invalid verdict.
- [ ] Version and invalidate serialized native modules across every engine,
      target and configuration change. Decide whether the first mainnet artifact
      can omit the persistent native cache without missing its catch-up bound.
- [ ] Add mandatory `cargo audit` and dependency-policy gates. Every allowed
      advisory must name its reachability, owner and expiry.
- [ ] Replay the complete root, receipt, cost and ABI corpus on x86-64 and
      AArch64, including fresh compilation and cached-module reloads.

## Acceptance Criteria

- The production dependency closure has no unaddressed known vulnerability;
  any non-production or unreachable exception is explicit and time-bounded.
- One checked-in engine configuration is used everywhere production code
  compiles, loads or executes a module.
- x86-64 and AArch64 produce identical roots, receipts, costs and refusals for
  every mandatory fixture and mainnet capture.
- Engine failure remains fail-closed with no interpreter, retry-on-another-engine
  or state-healing path.
- Formatting, strict Clippy, workspace tests, vendored compiler tests and the
  release report remain green.
