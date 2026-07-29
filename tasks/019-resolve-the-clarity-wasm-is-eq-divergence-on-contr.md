---
id: "019"
title: "Resolve the clarity-wasm is-eq divergence on contract principals"
status: completed
priority: medium
dependencies: []
tags: []
created_at: 2026-07-29
completed_at: 2026-07-29
---

# Resolve the clarity-wasm is-eq divergence on contract principals

## Objective

`clar2wasm`'s own generation suite intermittently reports that `is-eq` over a
list of contract principals disagrees between the compiler and the interpreter:

```
Compiled and interpreted results diverge!
(is-eq (list 'SH205N8RY76BDEA8Q0VP13GNS70M3CSA42KPRX0MB.A
             'S720QWDM2GQYP70TPDH62VHCCTWZ8Q4RC6YFH8BW3.A))
```

The inputs are generated, so the failure only appears on some runs. Equality on
principals is consensus-visible, so a real divergence would show up as a wrong
receipt or a wrong state root.

## Tasks

- [ ] Reproduce it deterministically from the reported inputs.
- [ ] Decide which side is wrong against the interpreter's own semantics.
- [ ] Fix the vendored compiler and keep the case as a regression test.

## Acceptance Criteria

- The generation suite passes repeatedly.
- A hardcoded case covers equality over contract-principal lists.
