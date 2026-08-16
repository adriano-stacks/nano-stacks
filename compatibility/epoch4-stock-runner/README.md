# Epoch 4 stock runner

These two standalone crates run the checked-in Epoch 4 compatibility vectors
against independently pinned `stacks-core` revisions. They parse the profile and
vectors directly and do not depend on a nano crate.

Run either revision from the repository root:

```sh
bash scripts/build-lock.sh nix develop -c cargo run \
  --manifest-path compatibility/epoch4-stock-runner/efc34a07/Cargo.toml
bash scripts/build-lock.sh nix develop -c cargo run \
  --manifest-path compatibility/epoch4-stock-runner/6d58b498/Cargo.toml
```

Each command prints a JSON report containing the exact revision and all vector
IDs. The historical receipt vector uses the checked-in external receipt oracle
and the stock Clarity consensus codec; the stateful root-verified execution is
owned by the named Task 086 conformance gate.
