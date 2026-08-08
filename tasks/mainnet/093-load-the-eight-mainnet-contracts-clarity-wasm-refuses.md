---
id: "093"
group: mainnet
title: "Load the eight mainnet contracts clarity-wasm refuses"
status: in-progress
priority: critical
effort: large
dependencies: ["073"]
tags: ["mainnet", "vm", "clarity", "wasm", "conformance", "release"]
created_at: 2026-08-08
type: bug
---

# Load the eight mainnet contracts clarity-wasm refuses

## Objective

Eight contracts the network deployed and accepted cannot be compiled by
clarity-wasm. Task 073's margin sweep found them and left them unclassified;
they are classified now, and they are conformance bugs rather than harness
artifacts or invalid sources. Under the release rule — production never falls
back to the interpreter, so a mainnet-valid contract that cannot load is a
code-generation bug — each one blocks the release.

## Classified 2026-08-08, on the production path

Every one was re-run through nano-vm's own `compile_under` via
`cargo xtask check-module`, not through the sweep's `clar2wasm::compile`. All
eight reproduce, so **none is a sweep-harness fault**.

And none is a Clarity analysis failure either, which was the other way out. Every
error string exists **only in clarity-wasm** and nowhere in clarity's shared
analyzer:

| error | lives in | contracts |
|---|---|---|
| `Not implemented` | `clar2wasm/src/wasm_generator.rs` | `SPXWGJQ101N1C1FYHK64TGTHN4793CHVKTJAT7VQ.amm-swap003`, `SP93HY1S36HJXC5MY9TKKKPDM2YZSKK385VBV15P.pool`, `SP11KTHSX0QSQWB4KRAH2S6BA058XPEZW4HT0D55W.pool` |
| `Incompatible types for duck typing: buff 1 against string-ascii 256` | `clar2wasm/src/duck_type.rs` | `SP2BRB6P0BK6T35DHTGXCV6MZ5TGRN5E0RKZ1T8B5.gated-pages`, `.gated-pages-004`, `.gated-pages-005`, `.gated-page-006` |
| `Tuples fields should be typed` | `clar2wasm/src/words/tuples.rs` | `SP1J70VWT7MRRP635NZ6E3J86PFE78JFXS0QR5ZAH.trajan-endorsement-alpha` — since traced to [[068]], see below |

So this is task 073's own phrasing, confirmed: **the network accepts what nano
cannot load.** Three distinct defects, eight contracts.

The sources are preserved at `/home/aldur/nano-073-sources/`, dumped read-only
through `NANO_DUMP_SOURCE` from `/home/aldur/mainnet-8716986/state`, 15–35 KB
each. They are the reproduction inputs; nothing here needs a live node.

```sh
cargo xtask check-module /home/aldur/mainnet-8716986/state \
  SPXWGJQ101N1C1FYHK64TGTHN4793CHVKTJAT7VQ.amm-swap003
```

## Tasks

- [x] Name the word `Not implemented` refuses, for each of the three contracts.
      **Done: `GeneratorError::NotImplemented` carries what it refused now, at all
      eight raise sites. All three answer the same thing — `equality over
      CallableType(Trait(..))`.**
- [x] Reduce the `Not implemented` family. **Done: `(is-eq <trait> <trait>)`.
      Fixed — `wasm_equal`'s principal arm covered `CallableSubtype::Principal`
      and not `Trait`, though a trait reference *is* a principal at run time
      ("a public function receives a trait argument as a bare principal",
      `wasm_generator.rs`). All three contracts compile and load.**
- [x] Reduce the duck-typing family from the real contract. **Done:
      `(map f a-string)`, from `gated-pages` line 390.**
- [x] Reduce the tuple family from the real contract. **Done, and it is not this
      task's to fix: it is [[068]]'s asymmetric least-supertype problem. See
      below.**
- [x] Fix the duck-typing rule. **Done, and it was not a rule to relax: the
      element type was wrong. `map` now takes it from clarity's own
      `SequenceSubtype::unit_type` rather than from a read strategy that cannot
      tell a buffer from an ASCII string. All four `gated-pages` compile and
      load.**
- [ ] `trajan-endorsement-alpha` is blocked on [[068]] rather than on a fix here.
      Moved there.
- [ ] Crosscheck each minimized source against the reference interpreter for
      result, receipt, cost, events and writes — not merely that it compiles.
- [ ] Add every reduction to the mandatory conformance suite, and re-run the
      full mainnet sweep to confirm 137,340/137,340.

## Acceptance Criteria

- All eight contracts compile and load through `compile_under`, with no
  per-contract exception, interpreter fallback or healing path.
- Each of the three defects has a minimized regression in the mandatory suite
  that fails on the current revision.
- The mainnet-state sweep reports every imported contract compiling, so the
  denominator is the whole state rather than the part that worked.
- Interpreter and clarity-wasm agree on the minimized sources, not just on
  whether they compile.

## Evidence that opened this task

Task 073 measured the locals boundary and closed it — no source-level shape
reaches wasmtime's limit any more — but its sweep over all 137,340 imported
mainnet contracts left 8 failing and unclassified, with a note that "some may be
harness artifacts". They are not. Splitting them out of 073 keeps that task's
locals result, which is finished and green, from being held open by three
unrelated code-generation defects.

## Three of eight fixed, 2026-08-08

`GeneratorError::NotImplemented` carried nothing, so all eight of its raise sites
produced the same three words. It names what it refused now, and all three
`Not implemented` contracts answered identically: **`equality over
CallableType(Trait(..))`** — `(is-eq <trait> <trait>)`.

`wasm_equal` handled `CallableSubtype::Principal` and not `CallableSubtype::Trait`
in the arm that compares bytes, though the two are one thing at run time.
`wasm_generator` states it where it lowers a parameter — *"a public function
receives a trait argument as a bare principal"* — and `contract-of` is the read of
it. One arm, and `amm-swap003` and both `.pool`s compile and load.

Pinned by `trait_equality`, which crosschecks against the reference interpreter
rather than asserting that it compiles. That distinction is the point: a trait
reference is a principal in linear memory, so this bug had two possible endings —
refuse to compile, or compare the wrong bytes and answer confidently. Task 086 is
what the second looks like on mainnet. The tests cover equal pairs, unequal
pairs, and equality tied to `contract-of`, and they fail on the previous
revision with the original refusal.

**Five contracts remain**: four `gated-pages*` on duck typing
(`buff 1` against `string-ascii 256`) and `trajan-endorsement-alpha` on
`Tuples fields should be typed`.

## The duck-typing family is located, 2026-08-08

`gated-pages` line 390:

```clarity
(map airdrop-single-page recipients titles descriptions metadata-uri)
```

`airdrop-single-page` takes `(recipient principal) (title (string-ascii 64))
(description (string-ascii 256)) (metadata-uri (string-ascii 256))`. The first
three arguments to `map` are lists. **The fourth is not** — `metadata-uri` is a
bare `(string-ascii 256)`, so `map` iterates over its *characters*, and each
element is fed to a parameter declared `(string-ascii 256)`.

The network accepted this, so the coercion is legal: an element of a
`string-ascii` sequence is a one-character `string-ascii`, widened to the
parameter's length. clar2wasm's `duck_type_stack` widens `StringType` to
`StringType` in its pass-through arm and would handle exactly that.

What it is actually asked to widen is `SequenceType(BufferType(BufferLength(1)))`
to `SequenceType(StringType(ASCII(BufferLength(256))))`. So the element type
clar2wasm derives for a `string-ascii` sequence under `map` is **`buff 1`**,
where Clarity's is `string-ascii 1`. The duck-type refusal is the symptom; the
wrong element type is the defect, and it is upstream of `duck_type.rs`.

Next step is therefore *not* to add a `buff → string` arm to `duck_type_stack` —
that would paper over a wrong type with a wrong coercion, and buffers and ASCII
strings are not interchangeable. It is to find where `map`'s element type is
derived and make a `string-ascii` sequence yield `string-ascii 1`.

Worth checking at the same time: whether `map` over a `string-utf8` has the same
problem, and whether anything else consumes that element type (`fold`, `filter`).

### Pinned to the line, and `fold` already solved it

`WasmGenerator::get_sequence_element_type` (`wasm_generator.rs:3144`) returns
`SequenceElementType::Byte` for **both** a buffer and a `string-ascii` — the
comment there says so: *"For buffer and string-ascii return none, which indicates
that elements should be read byte-by-byte."* That is fine for reading bytes and
wrong as a *type*: `Byte.into()` is `(buff 1)`, so `map`'s duck-typing step
(`sequences.rs:968`, `need_ducktyping(&element_type.into(), fn_arg)`) asks to
widen `(buff 1)` to the parameter's `(string-ascii 256)` and `duck_type_stack`
refuses.

`fold` hit this and worked around it. Its `FoldFuncTy` carries the comment
verbatim:

> Only for a list: `get_sequence_element_type` reports `(buff 1)` for a buffer
> *and* for a string, so ducking a string's element to the parameter's
> `(string-ascii n)` is refused as incompatible.

and takes the element type from the folded **function's declared parameter**
instead. `map` has the same problem and no such workaround, which is the whole of
this defect.

Two ways out, and they are not equivalent:

- **Local**, mirroring `fold`: at `map`'s site, take each sequence's element type
  from the corresponding parameter, or from the sequence's own `TypeSignature`
  rather than from `SequenceElementType`. Smallest diff; leaves the ambiguity in
  place for the next caller to trip over.
- **At the source**: split `SequenceElementType::Byte` into a buffer byte and an
  ASCII byte so the conversion to `TypeSignature` cannot lose which it is. Bigger
  diff, and it retires the trap — `fold`'s workaround could then go too.

The second is the one to do, and it wants a session with room to check every
`SequenceElementType` consumer rather than the tail of one. Whichever is taken,
the assertion is a crosscheck of `(map f a-string)` against the reference
interpreter, not that the contract compiles: this is a *type* being wrong, and a
wrong type that happens to lay out compatibly computes a wrong answer quietly —
which is what task 086 was.

## Seven of eight fixed, 2026-08-08

The duck-typing family is closed, and not by adding a `buff → string` coercion —
that would have papered over a wrong type with a wrong widening. `map` reads its
element type from `SequenceElementType`, which is a *read strategy*: `Byte` means
"a byte at a time" and is right for a buffer and an ASCII string alike, so its
conversion to a `TypeSignature` has to pick one and picked `(buff 1)`. Fine for
the load; wrong for the widening.

`map` carries the element as a type as well as a strategy now, and that type
comes from **clarity's own `SequenceSubtype::unit_type`** — the element type here
is the reference implementation's rather than a second opinion about it. Two
sites needed it: the workspace sizing and the `duck_type` call itself.

All four `gated-pages` compile and load. `map_over_a_string` pins it against the
reference interpreter for `string-ascii`, `buff`, `string-utf8`, a list, and the
mainnet shape of a list beside a string — because the defect was a *type* being
wrong, and a wrong type that lays out compatibly computes a wrong answer rather
than refusing. It fails on the previous revision with the mainnet error verbatim.

**One contract remains**: `trajan-endorsement-alpha`, on `Tuples fields should be
typed`.

## The last one belongs to 068, 2026-08-08

`Tuples fields should be typed` carried no detail either, so it says which key and
what the analysed type holds now:

```
the literal names `profile-sender`, and the analysed type holds
  ["date-event", "date-sent", "endorsement", "endorsementURI", "title"]
```

`trajan-endorsement-alpha` line 263 appends a **seven**-field tuple to
`submission-drafts`, whose declared element type (line 49) has **five**:

```clarity
drafts: (list 25 {date-sent: uint, date-event: uint, title: (string-ascii 256),
                  endorsement: (string-ascii 2048), endorsementURI: (string-ascii 128)})
...
(append submission-drafts { profile-sender: true, profile-receiver: false,
                            date-sent: .., date-event: .., title: ..,
                            endorsement: .., endorsementURI: .. })
```

So the analyser narrowed the literal to the list's element type and code
generation is asked to build the narrow shape out of a wider literal. That is
exactly [[068-resolve-asymmetric-tuple-least-supertype-semantics]]: its own
finding is that `least_supertype_v2_1`'s tuple arm "silently drops the ones the
second has and the first does not", and that **no static layout can be
conformant** — what is needed is a value representation carrying its shape at run
time, which 068 records as a clar2wasm architecture change rather than a choice
inside one word.

So this contract is not a ninth defect and must not be fixed here with a
per-shape workaround. It is the first *mainnet* instance of 068, which until now
had only minimized cases, and that is worth knowing: 068 is no longer only a
differential nobody has met.

**Seven of eight load. The eighth is 068's.**
