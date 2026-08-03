
## All 27 contracts heal, and the compiler stopped being able to stop the chain

The last four could not be rebuilt by deploying into a throwaway store, however
many dependencies were put in first: they name contracts this node's state does
not hold, and a contract cannot be deployed beside nothing. `Contract` has no
public `deserialize`, so seeding the store from the side store was closed too.

The route that needs no `Contract` works. Parse the source, build each function
with `DefinedFunction::new`, and merge them into the contract's *own* stored
context — reachable because `From<ContractContext> for Contract` exists and
`Deref` gives the context back. Nothing is deployed, which matters twice: no
other contract has to be present, and no top-level expression runs. Re-running
them would reset every data variable the contract has changed since, corrupting
state rather than healing it. A second pass reports `0 contracts`.

That moved the node 8,666,816 → 8,667,466 and then stopped on a different
fault, which turned out to be the more important one.

### A clarity-wasm codegen bug can no longer stop a mainnet replay

Block 8,667,467 failed on `v0-egroup`, which clar2wasm builds into a module
wasmtime refuses: "expected i64, found i32". Delta-debugging its 49 top-level
forms to four, then by hand to two, names the cause exactly:

```clarity
(define-private (it (m uint) (acc {t: uint, r: (optional uint)})) acc)
(define-private (f (target uint) (masks (list 128 uint)))
  (let ((init { t: target, r: none }))     ;; <- (optional NoType), one slot
    (get r (fold it masks init))))          ;; <- read as three
```

Passing the same tuple *inline* compiles, because `fold` sets the expected type
on the expression it is about to lay out; a `let` has already stored the narrow
one by then. `words/tuples.rs` carries two workarounds for this same
typechecker limitation. Fixing it properly is unification, and the chain does
not need it — **mainnet runs the interpreter**. So where the compiler cannot
build a loadable module, the interpreter decides, at both boundaries:

- **deploy** — it stores what is sound and rejects what is not
- **call** — that one failure is answered by it

Deliberately narrower than the `NANO_INTERPRETER_FALLBACK` beside it, which
replaces any runtime failure. A genuine runtime error is a real answer;
substituting it would hide a divergence instead of carrying one forward.

### A consensus gap the fallback exposed

The interpreter deploy path called `initialize_versioned_contract` **without
ever running the static analysis stacks-core runs first**, so it accepted
contracts the chain rejects — a contract naming a map that does not exist
deployed cleanly. Found only because the fallback made that path load-bearing.
It now type-checks first, and `compiler_refusal_fallback.rs` pins all three
cases, including that an unsound contract is still refused.

### Where replay stands

**8,666,680 → 8,668,160** this session (+1,480). The node then stops at
**8,668,161**, and this one is a different class: *every receipt succeeds and
only the state root differs*. Per plan.md that reads as MARF or write ordering
rather than execution — the first divergence in a while that is not a VM bug,
and the next thing to look at.

