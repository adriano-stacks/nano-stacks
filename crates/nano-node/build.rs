use std::{env, process::Command};

fn command_output(program: &str, arguments: &[&str]) -> Option<String> {
    let output = Command::new(program).args(arguments).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn main() {
    println!("cargo:rerun-if-env-changed=NANO_SOURCE_REVISION");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");

    let revision = env::var("NANO_SOURCE_REVISION")
        .ok()
        .filter(|revision| !revision.is_empty())
        .or_else(|| command_output("git", &["rev-parse", "HEAD"]))
        .expect("a release binary must name its source revision");
    let rustc = env::var("RUSTC")
        .ok()
        .and_then(|rustc| command_output(&rustc, &["--version"]))
        .expect("rustc must report its version");
    let target = env::var("TARGET").expect("Cargo sets TARGET for build scripts");

    println!("cargo:rustc-env=NANO_SOURCE_REVISION={revision}");
    println!("cargo:rustc-env=NANO_RUSTC_VERSION={rustc}");
    println!("cargo:rustc-env=NANO_BUILD_TARGET={target}");
}
