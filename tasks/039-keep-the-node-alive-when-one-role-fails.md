---
id: "039"
title: "Keep the node alive when one role fails"
status: completed
priority: high
effort: small
type: bug
dependencies: ["030"]
tags: ["node", "robustness"]
created_at: 2026-07-30
completed_at: 2026-07-30
---

# Keep the node alive when one role fails

## Objective

`stacks-node start` runs following, signing and mining in one process. If any
one of them returns an error the whole process ends, and the roles that were
working stop with it.

Observed on Hacknet: a miner configured with a `key_txid` from a Bitcoin chain
that had since been wiped failed its first lookup, and the node exited —

```
stacks-node: the miner stopped: JSON-RPC error: RPC error response:
RpcError { code: -5, message: "No such mempool or blockchain transaction..." }
```

The signer had already taken its slot for the reward cycle. On a network whose
threshold needs every signer, a miner misconfiguration therefore stops the
chain, and the operator's mistake is indistinguishable from a consensus fault.

Signing is the role a network depends on. It should outlive a miner that cannot
find its leader key.

## Tasks

- [x] Decide, per role, whether a failure is fatal to the node or only to that
      role, and say why in the code.
- [x] Keep the process running when a non-fatal role fails, reporting the role
      and the reason.
- [x] Check the miner's Bitcoin identity — the leader key transaction and the
      wallet — at start-up, so it fails while starting rather than mid-tenure.
- [ ] Refuse a configuration whose miner identity does not exist on the
      burnchain it names, rather than starting and dying.

## Acceptance Criteria

- A node whose miner is misconfigured still follows and still signs.
- The failure names the role, the reason, and what the node is still doing.
- A miner identity that does not resolve is refused at start-up.

## Outcome

`Job::is_fatal` decides it: signing and following end the node, the miner and
the RPC server do not. A node whose miner dies now says so and carries on
signing, which is what the Hacknet stall needed.

The miner's identity is resolved before it commits, and a `key_txid` the wallet
has never seen now names the configuration key and says what to do, instead of
surfacing a bare JSON-RPC code -5.

Refusing such a configuration outright, rather than starting and reporting,
still wants the burnchain reachable at load time — the config layer does not
talk to Bitcoin today.
