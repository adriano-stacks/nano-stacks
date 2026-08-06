//! Name the compiler this build contains, so the binary can write it down.
//!
//! `NANO_COMPILER_IDENTITY` becomes `nano_vm::COMPILER_IDENTITY`. The algorithm is
//! in `src/compiler_identity.rs`, included here rather than duplicated so that the
//! identity a build stamps and the identity a tool recomputes for another tree can
//! never drift apart.

include!("src/compiler_identity.rs");

fn main() {
    let vendor = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vendor/clarity-wasm");
    // Every file the hash covers, so a compiler change re-stamps the constant.
    // Without this the identity is whatever the tree looked like the first time
    // this crate was built, which is worse than having none.
    let mut files = Vec::new();
    if collect(&vendor, &vendor, &mut files).is_some() {
        for (_, path) in &files {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    let identity = compiler_identity_of(&vendor).unwrap_or_else(|| {
        // A build with no vendored compiler beside it: the constant says so
        // rather than naming a tree nobody can produce.
        "unknown: no vendor/clarity-wasm beside this crate".to_owned()
    });
    println!("cargo:rustc-env=NANO_COMPILER_IDENTITY={identity}");
}
