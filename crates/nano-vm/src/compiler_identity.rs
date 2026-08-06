// What identifies the compiler a binary contains.
//
// tasks/060 asks that checkpoint provenance and every reusable compiler fixture
// be bound to the exact clarity-wasm revision that produced it. That needs a
// name the *binary* knows, not one a git command produces beside it: the release
// report can run `git rev-parse HEAD:vendor/clarity-wasm`, but a node writing its
// own provenance cannot, and a tree hash of `HEAD` is wrong in exactly the tree
// where somebody is changing the compiler.
//
// So the name is a content hash of the sources that were compiled:
//
// ```text
// identity = sha256( for each file, sorted by relative path:
//                        relative path ‖ 0x00 ‖ sha256(contents) ‖ 0x00 )
// ```
//
// over every file under `vendor/clarity-wasm`, skipping `target` and `.git`
// directories and `*.wasm` files — the last because `standard.wasm` is generated
// from `standard.wat` by clar2wasm's own build script, so its content is implied
// by a file that is already counted and its presence depends on whether anything
// has been built yet.
//
// Two properties this has that a git tree hash does not: it describes a dirty tree
// as truthfully as a clean one, and it needs no git at all, so it survives a
// tarball, a vendored crate and a docker build.
//
// This file is both a module of `nano-vm` and `include!`d by its build script,
// which is why it takes a directory rather than reading a constant. The build
// script names the tree it is compiling; the same function recomputes the name of
// any other tree — an older revision extracted with `git archive`, for instance,
// which is how a fixture frozen before this existed gets a name that can be
// checked rather than taken on trust.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// The content identity of a clarity-wasm source tree, or `None` if it is absent.
///
/// Prefixed `sha256:` so that a value which is not one of these is obviously not
/// one, rather than merely a hash of something else.
#[must_use]
pub fn compiler_identity_of(root: &Path) -> Option<String> {
    let mut files = Vec::new();
    collect(root, root, &mut files)?;
    files.sort();
    let mut preimage = Vec::new();
    for (relative, path) in files {
        let contents = std::fs::read(&path).ok()?;
        preimage.extend_from_slice(relative.as_bytes());
        preimage.push(0);
        preimage.extend_from_slice(&Sha256::digest(&contents));
        preimage.push(0);
    }
    Some(format!("sha256:{}", hex::encode(Sha256::digest(&preimage))))
}

/// Every file under `directory`, as `(path relative to root, full path)`.
fn collect(root: &Path, directory: &Path, found: &mut Vec<(String, PathBuf)>) -> Option<()> {
    for entry in std::fs::read_dir(directory).ok()? {
        let path = entry.ok()?.path();
        let name = path.file_name()?.to_str()?.to_owned();
        if path.is_dir() {
            if name == "target" || name == ".git" {
                continue;
            }
            collect(root, &path, found)?;
        } else {
            if path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("wasm"))
            {
                continue;
            }
            let relative = path.strip_prefix(root).ok()?.to_str()?.to_owned();
            found.push((relative, path));
        }
    }
    Some(())
}
