# Structured fuzz targets

This directory is a separate Cargo workspace so `libfuzzer-sys` and the fuzz
toolchain do not enter the production workspace or lockfile.

The six targets derive structured inputs and call the stable bounded harnesses
in `crates/nano-adversarial`. Canonical seed material lives in that crate's
`corpus/` directory. Runs must copy seeds to a temporary corpus because
libFuzzer updates the corpus it receives.

Build every target with the pinned development environment:

```sh
nix develop --command cargo fuzz build --fuzz-dir fuzz --sanitizer none
```

Run one bounded local check against a disposable corpus:

```sh
corpus=$(mktemp -d)
trap 'rm -rf -- "$corpus"' EXIT
cp crates/nano-adversarial/corpus/p2p/* "$corpus"/
nix develop --command cargo fuzz run --fuzz-dir fuzz --sanitizer none \
  p2p_frame_and_protocol "$corpus" -- -runs=100 -max_len=4096
```

When a run finds a reproducible failure, minimize it and add the smallest case
to the owning checked-in corpus before removing the temporary artifact.
