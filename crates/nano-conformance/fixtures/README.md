# Conformance fixtures

This directory is intentionally free of fabricated chain data. M0 has a
one-block replay baseline so the scoreboard has a deterministic starting
point: it must fail at block one.

Capture real data here before implementing consensus components. The helper uses
the Nix shell's `sqlite3` and `curl` commands:

```sh
cargo xtask capture-fixtures \
  --state-dir /path/to/hacknet-state \
  --events-dir /path/to/event-capture \
  --bitcoin-rpc http://127.0.0.1:18443 \
  --stacks-rpc http://127.0.0.1:20443 \
  --hacknet-commit <commit> \
  --checkpoint-height <height> \
  --first-height <height> \
  --replay-blocks <count> \
  --full-sortition-history true
```

The command records all live inputs plus a portable `checkpoint-H/`: a stable
SQLite backup of stacks-core's MARF index and its serialized trie blob file.
`checkpoint.toml` pins the state ID and the `state_index_root` read from the
canonical checkpoint header. The importer must select that state ID; later
stored roots are present only because MARF back-pointers require its ancestry.

A complete capture contains:

- `bitcoin/blocks/*.hex`
- `sortition/snapshots.json`
- `nakamoto/blocks/*.bin`
- `events/new_block/*.json`
- `chainstate/checkpoint-H/`
- `chainstate/checkpoint-H/native-effects.json`
- `stacker_set/cycle-N.json`

The checked-in Hacknet oracle keeps the complete sortition history so its
Bitcoin and sortition decisions can be re-derived offline. Large diagnostic
captures may omit `--full-sortition-history`; they retain only the bounded burn
window execution needs.

Replace `manifest.toml`'s `replay_blocks` with the captured block count in the
same change that adds the data and a fixture integrity test. A captured tree
must also add a nonempty `provenance.toml` describing the hacknet commit,
checkpoint height, capture time, and source endpoints.

Run `cargo xtask validate-fixtures` before treating a capture as an oracle. It
fails for the baseline and verifies all required data classes for a capture.
