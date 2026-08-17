# Adversarial harnesses

`nano-adversarial` holds stable, bounded entry points shared by ordinary CI and
long-running fuzz engines. Each directory under `corpus/` is owned by one target;
a minimized finding belongs there before the external crash artifact may expire.

The `adversarial-smoke` gate replays every seed serially. It also enforces at most
32 seeds and 4 MiB per target and proves that the corpus reaches each successful
decode, validation, state-operation, or compile/load path. These are smoke limits,
not protocol limits; the harness functions apply their own input bounds.

The initial seeds are canonical bytes already exercised elsewhere in the tree:
the Nakamoto block and transaction come from the checked-in conformance capture,
the signer update is a stock signer message, and the checkpoint pair is the
published sample bundle. They are copied here so this corpus has explicit
ownership and cannot disappear when another fixture is reorganized.
