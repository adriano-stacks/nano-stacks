---
id: "051"
title: "Enforce checkpoint attestation during startup"
status: completed
priority: high
effort: medium
type: bug
group: mainnet
dependencies: ["031"]
tags: ["mainnet", "checkpoint", "trust"]
created_at: 2026-08-02
---

# Enforce checkpoint attestation during startup

## Objective

[[031-establish-a-trust-root-for-the-checkpoint]] implemented and documented
`attest_checkpoint` and `adopt_checkpoint`, but the binary bypasses both. Runtime
passes the configured source and root directly to `open_from_checkpoint`, and
the mainnet state has no `checkpoint-provenance.toml`.

Make the documented trust procedure the only production import path.

## Tasks

- [x] Add configuration for the checkpoint manifest, attesting header and a
      reward set obtained independently of the checkpoint.
- [x] Call `adopt_checkpoint` before any imported state is opened or copied.
- [x] Refuse missing, unsigned, wrong-height, wrong-state or wrong-root inputs.
- [x] Record provenance in the role's state directory and verify it on restart.
- [x] Refuse to reuse a directory descended from a different checkpoint.
- [x] Keep `docs/checkpoint-trust.md` executable as an operator procedure.

## Acceptance Criteria

- The shipped binary cannot import an unattested checkpoint.
- Provenance survives restart and names the manifest, signed header, signer
  weight and threshold used for adoption.
- Tests cover a valid import and every mismatch before any chainstate mutation.

## It attests mainnet

The mainnet checkpoint now goes through `adopt_checkpoint` before any of it is
opened, against the block that sealed it and a reward set fetched from
`/v3/stacker_set/:cycle` rather than from the checkpoint:

```
checkpoint a87338900f279efc1b1df130004238cac8e09a2a4244fea39436fc66afae932d
    attested by 2708 of 2599 signer weight
```

A checkpoint stating its own root is not evidence of anything; a Nakamoto header
at that height carries the same root and a reward set put threshold weight
behind it, which is what makes one trustworthy. A directory that already carries
provenance is not re-adopted, and refuses to be reused for a different
checkpoint — its trie stands on the first one's state, and nothing later would
notice.

## All five refusals now have a test

`attest_checkpoint` refused all five already; three of them had nothing proving
it. `a_signed_header_attests_the_checkpoint_it_sealed` now tampers each input in
turn and asserts the specific error:

| tampered | refused with |
|---|---|
| `state_index_root` | `StateRoot` |
| `stacks_height` | `Height` |
| `source_state_id` | `StateId` |
| the header's signatures | `Signers` |
| the attesting block or reward set absent | `runtime::adopt` returns before anything is opened |

The `StateId` case is the one worth having. It is what an operator gets wrong by
copying a configuration, and it is the one that fails silently: the trie imports,
the root matches, and every block after it is computed against somebody else's
ancestry.

## The procedure cannot drift from the node any more

`docs/checkpoint-trust.md`'s worked example of `checkpoint-provenance.toml` is the
part an operator compares their own file against by eye, so a renamed field would
make the document quietly wrong in exactly the place it is trusted.
`the_checkpoint_procedure_names_the_fields_the_node_writes` records a provenance
file through the real `CheckpointProvenance::record`, reads it back, and asserts
every key it contains appears in the document. Verified non-vacuous by misspelling
one key in the document and watching it fail.
