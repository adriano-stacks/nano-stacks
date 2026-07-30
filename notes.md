# Code Review

## 2026-07-29

- Why ./Cargo.toml include a Rust version, if we also have a ./rust-toolchain.toml file?
- We need to make sure that the Rust version used by the flake matches rust-toolchain.
- We need to make sure that Clippy lints are strict enough to provide good quality code.
  - Directives like `#![forbid(unsafe_code)]` should be workspace global
- There are dependencies that aren't bumped to latest, we need to fix that.
- There's an empty `examples` directory in `nano-chainstate`

### `nano-address`

The PoX-5 / sBTC related derivations don't seem to belong there?

### `nano-chainstate`

Some comments refer to `M0`, which doesn't make sense in the context of the codebase.
