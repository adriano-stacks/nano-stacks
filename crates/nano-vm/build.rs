//! Name the compiler this build contains, so the binary can write it down.
//!
//! `NANO_COMPILER_IDENTITY` becomes `nano_vm::COMPILER_IDENTITY`. The algorithm is
//! in `src/compiler_identity.rs`, included here rather than duplicated so that the
//! identity a build stamps and the identity a tool recomputes for another tree can
//! never drift apart.

include!("src/compiler_identity.rs");

fn main() {
    // A shared target directory must not reuse absolute source watches emitted
    // by this build script from another checkout.
    println!("cargo:rerun-if-env-changed=CARGO_MANIFEST_DIR");
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR")
        .map(std::path::PathBuf::from)
        .expect("Cargo sets CARGO_MANIFEST_DIR for build scripts");
    let vendor = manifest.join("../../vendor/clarity-wasm");
    // Watching the directory itself matters when it was absent during an earlier
    // build: Cargo then has no covered files whose later appearance can rerun us.
    println!("cargo:rerun-if-changed={}", vendor.display());
    // Every file the hash covers, so a compiler change re-stamps the constant.
    // Without this the identity is whatever the tree looked like the first time
    // this crate was built, which is worse than having none.
    let mut files = Vec::new();
    if collect(&vendor, &vendor, &mut files).is_some() {
        for (_, path) in &files {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    let Some(identity) = compiler_identity_of(&vendor) else {
        panic!(
            "the vendored compiler at {} is absent or unreadable; refusing to build an artifact without its identity",
            vendor.display()
        );
    };
    println!("cargo:rustc-env=NANO_COMPILER_IDENTITY={identity}");
}
