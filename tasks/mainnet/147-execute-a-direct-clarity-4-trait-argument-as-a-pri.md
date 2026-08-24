---
title: "Execute a direct Clarity 4 trait argument as a principal"
id: "147"
status: in-progress
priority: critical
type: bug
tags: ["mainnet", "vm", "clarity-wasm", "consensus", "liveness", "release"]
created_at: "2026-08-22"
---

# Execute a direct Clarity 4 trait argument as a principal

## Objective

Restore mainnet liveness at block 8,815,026. A Clarity 4 public function takes
a direct trait reference, passes it to `contract-of`, and the compiled engine
tries to read the argument from an unmaterialized `(offset, length) = (0, 0)`
principal representation. The canonical engine succeeds, so refusing the
block strands every nano follower at its parent.

## Evidence

The node on RPC port 20492, the current witness and the packaged follower all
seal through 8,815,025 and fail transaction
`8b54004787530e3547a2a9316838375eba701a72d55e7a3a72aef2fe3c471e1d`
in the next block with:

```text
principal representation for PrincipalType at value offset 159218 points to
offset 0 with invalid length 0
```

The canonical transaction calls
`SP2VMFSHP3EGCZNSQPAA31AJZKS7V70KXY0TT08RF.reward-claim-registry`
`register-for-claims`, whose second argument is a trait reference. The contract
was published at 8,814,886 under Clarity 4 and immediately binds
`(signer (contract-of signer-manager))`. The canonical receipt is `(ok u2)`
with state root `255c6115d5038ee255afefb26ddaed565417d06e5b17f88a4cd9667e569c476b`
for block 8,815,026.

## Tasks

- [ ] Reduce the direct Clarity 4 public trait-argument path to an offline
      compiled/interpreter crosscheck.
- [x] Fix the nano-owned clarity-wasm ABI path without an interpreter fallback.
      Closed in 26067420: duck-typing an optional projects its payload's slots
      on both branches, so the `none` branch offered a tuple whose principals
      were the unmaterialised `(0, 0)`, and nano tried to materialise it into
      the runtime-shape arena. A shape that cannot be read cannot be preserved,
      so it answers with the same zero handle a value that never crossed the
      host carries. The smallest reproduction needs no trait at all.
- [ ] Replay transaction 8b540047…c471e1d and block 8,815,026 to their
      canonical receipt and state root.
- [ ] Build and restart the node on port 20492, then prove its executed height
      advances beyond 8,815,026.

## Acceptance Criteria

- The focused compiled/interpreter regression is green for Clarity 4.
- Block 8,815,026 seals with the canonical receipt and state root.
- The restarted node advances beyond the stalled height.
- `cargo clippy` reports no warnings in every changed crate.
