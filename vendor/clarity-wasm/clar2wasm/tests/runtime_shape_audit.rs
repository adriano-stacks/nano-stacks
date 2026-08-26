//! Every path a widened value can take, measured rather than reasoned about.
//!
//! A value read out of storage has a *declared* capacity wider than the length
//! it happens to carry, and the reference charges for the declared one:
//! `Value::size()` is `TypeSignature::type_of(self).size()`, and a list's size
//! is `type_signature_size + max_len × entry.size()`. The compiler keeps that
//! capacity in a side arena, reached through a handle the value carries, and
//! every word that hands such a value onward has to hand the handle on with it.
//!
//! Tasks 149 and 150 found eight words that did not. They were found one at a
//! time, by a mainnet block stopping — and the audit that was supposed to be
//! exhaustive missed `concat`, which its own list named, because it reasoned
//! about the word instead of measuring it. So this file measures: one test per
//! path, each one a `crosscheck_cost` against the interpreter, which is the
//! only oracle that settles the question.
//!
//! The sink is almost always `print`, whose `cost_print` is the measured value's
//! size and therefore reports a lost capacity immediately. Where a word charges
//! by something else — a write length, a hash input — that is the sink instead.

use clar2wasm::tools::crosscheck_cost;
use clarity::vm::Value;

/// A stored `(list 12000 uint)` holding nothing: the mainnet shape, and the
/// widest possible gap between declared capacity and run-time length.
const EMPTY_STORED_LIST: &str = r#"
(define-map holder uint {items: (list 12000 uint)})
(map-set holder u1 {items: (list )})
(define-private (items) (get items (unwrap-panic (map-get? holder u1))))
"#;

/// The same with three elements in it, so a word that walks the sequence still
/// has something to walk.
const SHORT_STORED_LIST: &str = r#"
(define-map holder uint {items: (list 12000 uint)})
(map-set holder u1 {items: (list u1 u2 u3)})
(define-private (items) (get items (unwrap-panic (map-get? holder u1))))
"#;

/// Run one body against both stored lists, empty and short.
fn both(body: &str) {
    for prelude in [EMPTY_STORED_LIST, SHORT_STORED_LIST] {
        crosscheck_cost(&format!("{prelude}{body}"), "run", &[]);
    }
}

// ---------------------------------------------------------------- pass-through

#[test]
fn an_if_branch_hands_back_a_widened_list() {
    both(
        r#"
(define-public (run)
  (begin (print (if (is-eq (len (items)) u0) (items) (items))) (ok u0)))
"#,
    );
}

#[test]
fn a_let_rebinding_hands_back_a_widened_list() {
    both(
        r#"
(define-public (run)
  (let ((a (items))) (let ((b a)) (begin (print b) (ok u0)))))
"#,
    );
}

#[test]
fn a_begin_hands_back_a_widened_list() {
    both(
        r#"
(define-public (run)
  (begin (print (begin (items))) (ok u0)))
"#,
    );
}

#[test]
fn a_match_binding_hands_back_a_widened_list() {
    both(
        r#"
(define-public (run)
  (match (map-get? holder u1)
    d (begin (print (get items d)) (ok u0))
    (ok u1)))
"#,
    );
}

#[test]
fn an_unwrapped_optional_hands_back_a_widened_list() {
    both(
        r#"
(define-public (run)
  (begin (print (unwrap-panic (some (items)))) (ok u0)))
"#,
    );
}

#[test]
fn a_defaulted_optional_hands_back_a_widened_list() {
    both(
        r#"
(define-public (run)
  (begin (print (default-to (list u9) (some (items)))) (ok u0)))
"#,
    );
}

#[test]
fn an_unwrapped_response_hands_back_a_widened_list() {
    both(
        r#"
(define-private (wrap) (ok (items)))
(define-public (run)
  (begin (print (unwrap-panic (wrap))) (ok u0)))
"#,
    );
}

#[test]
fn a_try_hands_back_a_widened_list() {
    both(
        r#"
(define-private (wrap) (if false (err u1) (ok (items))))
(define-public (run)
  (begin (print (try! (wrap))) (ok u0)))
"#,
    );
}

#[test]
fn an_unwrapped_err_hands_back_a_widened_list() {
    both(
        r#"
(define-private (wrap) (if true (err (items)) (ok u0)))
(define-public (run)
  (begin (print (unwrap-err-panic (wrap))) (ok u0)))
"#,
    );
}

#[test]
fn an_asserted_widened_list_survives_the_assertion() {
    both(
        r#"
(define-public (run)
  (let ((l (items)))
    (begin (asserts! (is-eq (len l) (len l)) (err u1)) (print l) (ok u0))))
"#,
    );
}

// ------------------------------------------------------------ function borders

/// An argument is charged at the width it arrives with; the binding the body
/// then reads carries its own.
///
/// The reference charges `cost_inner_type_check_cost` over `arg.size()` — the
/// caller's width, 192,006 for a `(list 12000 uint)` — and then binds the cast
/// value, so `cost_lookup_variable_size` and everything after it measure what
/// the data holds, 54 for three elements. Keeping the caller's arena entry for
/// the binding charged 192,006 *twice*, and no word was wrong to make it
/// happen: the parameter itself was.
#[test]
fn a_private_function_argument_arrives_widened() {
    both(
        r#"
(define-private (measure (l (list 12000 uint))) (print l))
(define-public (run)
  (begin (measure (items)) (ok u0)))
"#,
    );
}

/// The same where the handle is not in the parameter's own first slot.
///
/// A tuple keeps one of its own and one per field, an optional keeps its arm's
/// one slot along, and a response keeps one per arm. Clearing only the first
/// slot left every one of these charging the caller's width, and a tuple
/// reached through `map` charged it twice.
#[test]
fn a_nested_parameter_handle_arrives_widened_too() {
    both(
        r#"
(define-private (from-tuple (d {items: (list 12000 uint)})) (print (get items d)))
(define-private (from-optional (o (optional (list 12000 uint)))) (print (unwrap-panic o)))
(define-private (from-response (r (response (list 12000 uint) uint))) (print (unwrap-panic r)))
(define-public (run)
  (begin
    (from-tuple (unwrap-panic (map-get? holder u1)))
    (from-optional (some (items)))
    (from-response (ok (items)))
    (ok u0)))
"#,
    );
}

/// A `fold` accumulator declared wider than what it carries is one of these on
/// every iteration.
///
/// This is the shape that made it worth measuring rather than reasoning: the
/// step function hands the accumulator straight back, so nothing in the body
/// looks like a measurement, and the over-charge was 5 × 383,904 on a
/// three-element list.
#[test]
fn a_fold_accumulator_is_charged_once_per_iteration() {
    both(
        r#"
(define-private (step (v uint) (acc (list 12000 uint))) acc)
(define-public (run)
  (begin (print (fold step (items) (items))) (ok u0)))
"#,
    );
}

/// An element of a constructed list is sanitized, and sanitizing reaches into a
/// tuple field.
///
/// `Value::cons_list` rebuilds each element against the entry type it derived,
/// so a `{items: (list 12000 uint)}` element keeps only the width its field is
/// using. Narrowing only *list* elements left `map` and `fold` over a list of
/// stored tuples charging the declared 192,006 an iteration, and reading a
/// field back out of such an element over-charged by 375.
#[test]
fn a_tuple_element_of_a_constructed_list_is_narrowed_through_its_fields() {
    both(
        r#"
(define-private (measure (d {items: (list 12000 uint)})) (len (get items d)))
(define-public (run)
  (let ((one (list (unwrap-panic (map-get? holder u1)))))
    (begin
      (print (map measure one))
      (print (fold + (map measure one) u0))
      (print (get items (unwrap-panic (element-at? one u0))))
      (ok u0))))
"#,
    );
}

#[test]
fn a_private_function_return_is_still_widened() {
    both(
        r#"
(define-private (relay) (items))
(define-public (run)
  (begin (print (relay)) (ok u0)))
"#,
    );
}

#[test]
fn a_read_only_function_return_is_still_widened() {
    both(
        r#"
(define-read-only (relay) (items))
(define-public (run)
  (begin (print (relay)) (ok u0)))
"#,
    );
}

// --------------------------------------------------------------- constructors

#[test]
fn a_tuple_built_from_a_widened_field_is_measured_whole() {
    both(
        r#"
(define-public (run)
  (begin (print {n: u0, items: (items)}) (ok u0)))
"#,
    );
}

#[test]
fn a_merged_tuple_keeps_a_widened_field() {
    both(
        r#"
(define-public (run)
  (let ((d (unwrap-panic (map-get? holder u1))))
    (begin (print (get items (merge d {items: (items)}))) (ok u0))))
"#,
    );
}

#[test]
fn a_nested_tuple_keeps_a_widened_field() {
    both(
        r#"
(define-public (run)
  (begin (print {outer: {inner: (items)}}) (ok u0)))
"#,
    );
}

#[test]
fn a_list_of_a_widened_list_is_measured_whole() {
    both(
        r#"
(define-public (run)
  (begin (print (list (items))) (ok u0)))
"#,
    );
}

#[test]
fn an_optional_of_a_widened_list_is_measured_whole() {
    both(
        r#"
(define-public (run)
  (begin (print (some (items))) (ok u0)))
"#,
    );
}

#[test]
fn a_response_of_a_widened_list_is_measured_whole() {
    both(
        r#"
(define-public (run)
  (let (
    (committed (print (ok (items))))
    (failed (print (err (items))))
  ) (ok u0)))
"#,
    );
}

// ------------------------------------------------------------- sequence words

#[test]
fn a_filtered_widened_list_keeps_its_capacity() {
    both(
        r#"
(define-private (keep (v uint)) (< v u2))
(define-public (run)
  (begin (print (filter keep (items))) (ok u0)))
"#,
    );
}

#[test]
fn an_appended_widened_list_keeps_its_capacity() {
    both(
        r#"
(define-public (run)
  (begin (print (append (items) u7)) (ok u0)))
"#,
    );
}

#[test]
fn a_concatenated_widened_list_keeps_its_capacity() {
    both(
        r#"
(define-public (run)
  (begin
    (print (concat (items) (list u7)))
    (print (concat (list u7) (items)))
    (print (concat (items) (items)))
    (ok u0)))
"#,
    );
}

#[test]
fn a_reduced_widened_list_keeps_its_reduced_capacity() {
    both(
        r#"
(define-public (run)
  (begin (print (unwrap-panic (as-max-len? (items) u5000))) (ok u0)))
"#,
    );
}

#[test]
fn a_replaced_widened_list_keeps_its_capacity() {
    crosscheck_cost(
        &format!(
            r#"{SHORT_STORED_LIST}
(define-public (run)
  (begin (print (unwrap-panic (replace-at? (items) u0 u9))) (ok u0)))
"#
        ),
        "run",
        &[],
    );
}

#[test]
fn a_sliced_widened_list_is_measured_correctly() {
    crosscheck_cost(
        &format!(
            r#"{SHORT_STORED_LIST}
(define-public (run)
  (begin (print (unwrap-panic (slice? (items) u0 u2))) (ok u0)))
"#
        ),
        "run",
        &[],
    );
}

#[test]
fn an_element_of_a_widened_list_is_measured_correctly() {
    crosscheck_cost(
        &format!(
            r#"{SHORT_STORED_LIST}
(define-public (run)
  (begin (print (element-at? (items) u0)) (ok u0)))
"#
        ),
        "run",
        &[],
    );
}

#[test]
fn a_mapped_widened_list_is_measured_correctly() {
    both(
        r#"
(define-private (inc (v uint)) (+ v u1))
(define-public (run)
  (begin (print (map inc (items))) (ok u0)))
"#,
    );
}

#[test]
fn a_folded_widened_list_hands_its_accumulator_back() {
    both(
        r#"
(define-private (step (v uint) (acc (list 12000 uint))) acc)
(define-public (run)
  (begin (print (fold step (items) (items))) (ok u0)))
"#,
    );
}

#[test]
fn index_of_searches_a_widened_list() {
    both(
        r#"
(define-public (run)
  (begin (print (index-of? (items) u2)) (ok u0)))
"#,
    );
}

#[test]
fn len_measures_a_widened_list() {
    both(
        r#"
(define-public (run)
  (begin (print (len (items))) (ok u0)))
"#,
    );
}

// -------------------------------------------------------------- other charges

#[test]
fn a_widened_list_written_back_charges_its_capacity() {
    both(
        r#"
(define-public (run)
  (begin (map-set holder u2 {items: (items)}) (ok u0)))
"#,
    );
}

#[test]
fn a_widened_list_set_into_a_data_var_charges_its_capacity() {
    both(
        r#"
(define-data-var kept (list 12000 uint) (list ))
(define-public (run)
  (begin (var-set kept (items)) (ok u0)))
"#,
    );
}

#[test]
fn a_widened_list_serialized_charges_its_capacity() {
    both(
        r#"
(define-public (run)
  (begin (print (to-consensus-buff? (items))) (ok u0)))
"#,
    );
}

#[test]
fn a_widened_list_compared_charges_its_capacity() {
    both(
        r#"
(define-public (run)
  (begin (print (is-eq (items) (items))) (ok u0)))
"#,
    );
}

// -------------------------------------------------------- other sequence types

/// A stored `(buff 1000)` and a stored `(string-ascii 1000)`: same rule, and
/// their element sizes differ from a `uint`'s, which is what makes an inherited
/// entry type visible rather than incidental.
#[test]
fn a_widened_buffer_keeps_its_capacity_through_the_family() {
    let prelude = r#"
(define-map holder uint {items: (buff 1000)})
(map-set holder u1 {items: 0x0102})
(define-private (items) (get items (unwrap-panic (map-get? holder u1))))
"#;
    for body in [
        "(define-public (run) (begin (print (items)) (ok u0)))",
        "(define-public (run) (begin (print (concat (items) 0x03)) (ok u0)))",
        "(define-public (run) (begin (print (unwrap-panic (as-max-len? (items) u500))) (ok u0)))",
        "(define-public (run) (begin (print (unwrap-panic (replace-at? (items) u0 0x09))) (ok u0)))",
        "(define-public (run) (begin (print (unwrap-panic (slice? (items) u0 u1))) (ok u0)))",
        "(define-public (run) (begin (print (element-at? (items) u0)) (ok u0)))",
        "(define-public (run) (begin (print (sha256 (items))) (ok u0)))",
        "(define-public (run) (begin (print {n: u0, items: (items)}) (ok u0)))",
        "(define-public (run) (begin (print (list (items))) (ok u0)))",
    ] {
        crosscheck_cost(&format!("{prelude}{body}"), "run", &[]);
    }
}

#[test]
fn a_widened_string_keeps_its_capacity_through_the_family() {
    let prelude = r#"
(define-map holder uint {items: (string-ascii 1000)})
(map-set holder u1 {items: "ab"})
(define-private (items) (get items (unwrap-panic (map-get? holder u1))))
"#;
    for body in [
        "(define-public (run) (begin (print (items)) (ok u0)))",
        "(define-public (run) (begin (print (concat (items) \"c\")) (ok u0)))",
        "(define-public (run) (begin (print (unwrap-panic (as-max-len? (items) u500))) (ok u0)))",
        "(define-public (run) (begin (print (unwrap-panic (replace-at? (items) u0 \"z\"))) (ok u0)))",
        "(define-public (run) (begin (print (unwrap-panic (slice? (items) u0 u1))) (ok u0)))",
        "(define-public (run) (begin (print (element-at? (items) u0)) (ok u0)))",
        "(define-public (run) (begin (print {n: u0, items: (items)}) (ok u0)))",
        "(define-public (run) (begin (print (list (items))) (ok u0)))",
    ] {
        crosscheck_cost(&format!("{prelude}{body}"), "run", &[]);
    }
}

/// A stored UTF-8 string, whose element size is four bytes rather than one.
#[test]
fn a_widened_utf8_string_keeps_its_capacity() {
    let prelude = r#"
(define-map holder uint {items: (string-utf8 1000)})
(map-set holder u1 {items: u"ab"})
(define-private (items) (get items (unwrap-panic (map-get? holder u1))))
"#;
    for body in [
        "(define-public (run) (begin (print (items)) (ok u0)))",
        "(define-public (run) (begin (print (concat (items) u\"c\")) (ok u0)))",
        "(define-public (run) (begin (print {n: u0, items: (items)}) (ok u0)))",
    ] {
        crosscheck_cost(&format!("{prelude}{body}"), "run", &[]);
    }
}

// ------------------------------------------------------------- other crossings

/// A value that crosses a contract-call boundary is re-read on the other side,
/// and the declared return type is what it arrives under.
#[test]
fn a_widened_list_returned_across_a_contract_call_is_measured_whole() {
    let provider = r#"
(define-map holder uint {items: (list 12000 uint)})
(map-set holder u1 {items: (list u1 u2 u3)})
(define-read-only (fetch) (get items (unwrap-panic (map-get? holder u1))))
"#;
    let caller = r#"
(define-public (run)
  (begin (print (contract-call? .provider fetch)) (ok u0)))
"#;
    clar2wasm::tools::crosscheck_cost_multi_contract(
        &[("provider", provider), ("caller", caller)],
        "run",
        &[],
    );
}

/// A value deserialized from a buffer arrives under the declared type of the
/// `from-consensus-buff?` annotation, which can be wider than the bytes.
#[test]
fn a_deserialized_widened_list_is_measured_whole() {
    let snippet = r#"
(define-public (run)
  (begin
    (print (from-consensus-buff? (list 12000 uint) 0x0b00000001010000000000000000000000000000002a))
    (ok u0)))
"#;
    crosscheck_cost(snippet, "run", &[]);
}

// ------------------------------------------------------- the no-handle fallback

/// The same words again, over a value that carries *no* arena entry.
///
/// A public function's argument arrives as itself, so nothing widened it and
/// there is no handle to inherit from: its capacity is its element count, and
/// every word that inherits a capacity has to fall back to that. This path
/// barely ran while parameters kept their caller's handle, and the first thing
/// it found when it started running was `filter` handing the fallback its own
/// loop counter — decremented to zero by then, so a filtered list measured as
/// empty and under-charged every later reading of it.
#[test]
fn a_word_that_inherits_a_capacity_falls_back_to_the_element_count() {
    let list = Value::cons_list_unsanitized(vec![Value::UInt(1), Value::UInt(2), Value::UInt(3)])
        .expect("three elements");
    for body in [
        "(define-data-var h uint u1)\n(define-private (le (v uint)) (<= v (var-get h)))\n(define-public (run (l (list 100 uint))) (let ((f (filter le l))) (ok (len f))))",
        "(define-public (run (l (list 100 uint))) (let ((f (append l u9))) (ok (len f))))",
        "(define-public (run (l (list 100 uint))) (let ((f (unwrap-panic (as-max-len? l u50)))) (ok (len f))))",
        "(define-public (run (l (list 100 uint))) (let ((f (unwrap-panic (as-max-len? l u3)))) (ok (len f))))",
        "(define-public (run (l (list 100 uint))) (let ((f (concat l (list u9)))) (ok (len f))))",
        "(define-public (run (l (list 100 uint))) (let ((f (concat l l))) (ok (len f))))",
        "(define-public (run (l (list 100 uint))) (let ((f (unwrap-panic (replace-at? l u0 u9)))) (ok (len f))))",
        "(define-public (run (l (list 100 uint))) (let ((f (element-at? l u0))) (ok f)))",
        "(define-public (run (l (list 100 uint))) (let ((f (unwrap-panic (slice? l u0 u2)))) (ok (len f))))",
        "(define-private (inc (v uint)) (+ v u1))\n(define-public (run (l (list 100 uint))) (let ((f (map inc l))) (ok (len f))))",
        "(define-private (s (v uint) (acc (list 100 uint))) acc)\n(define-public (run (l (list 100 uint))) (let ((f (fold s l l))) (ok (len f))))",
        "(define-public (run (l (list 100 uint))) (let ((t {items: l})) (ok (len (get items t)))))",
        "(define-public (run (l (list 100 uint))) (let ((f (list l))) (ok (len (unwrap-panic (element-at? f u0))))))",
        "(define-public (run (l (list 100 uint))) (begin (print l) (ok u0)))",
    ] {
        crosscheck_cost(body, "run", std::slice::from_ref(&list));
    }
}

// -------------------------------------------------- other sources and nestings

/// A second sweep: other places a width comes from, and deeper places it hides.
///
/// The first sweep took its widened value out of a map field. A data var is the
/// other source; a map *key* is a place the width is charged rather than
/// measured; and an optional or a tuple two levels down is where a walk that
/// stops one level early would miss it. All measured, none of them differential
/// when this went in — which is the point of writing them down.
#[test]
fn a_second_sweep_of_sources_and_nestings() {
    const PRELUDE: &str = r#"
(define-data-var kept (list 12000 uint) (list ))
(define-map holder uint {items: (list 12000 uint)})
(define-map by-list (list 12000 uint) uint)
(define-map deep uint {a: (optional (list 12000 uint)), b: {inner: (list 12000 uint)}})
(map-set holder u1 {items: (list u1 u2 u3)})
(map-set deep u1 {a: (some (list u1 u2)), b: {inner: (list u1 u2)}})
(define-private (items) (get items (unwrap-panic (map-get? holder u1))))
"#;
    for body in [
        // var-get as the widening source
        "(define-public (run) (begin (var-set kept (items)) (print (var-get kept)) (ok u0)))",
        // a widened list used as a map key
        "(define-public (run) (begin (map-set by-list (items) u1) (print (map-get? by-list (items))) (ok u0)))",
        // a widened list inside an optional field
        "(define-public (run) (begin (print (get a (unwrap-panic (map-get? deep u1)))) (ok u0)))",
        // a widened list inside a nested tuple field
        "(define-public (run) (begin (print (get inner (get b (unwrap-panic (map-get? deep u1))))) (ok u0)))",
        // the whole deep tuple
        "(define-public (run) (begin (print (unwrap-panic (map-get? deep u1))) (ok u0)))",
        // a match on a response arm
        "(define-private (wrap) (if false (err u1) (ok (items))))\n(define-public (run) (match (wrap) l (begin (print l) (ok u0)) e (err e)))",
        // index-of? with a widened needle
        "(define-public (run) (begin (print (index-of? (list (items)) (items))) (ok u0)))",
        // a fold whose accumulator is a tuple with a widened field
        "(define-private (s (v uint) (acc {items: (list 12000 uint)})) acc)\n(define-public (run) (begin (print (fold s (items) {items: (items)})) (ok u0)))",
        // map over two sequences
        "(define-private (both (a uint) (b uint)) (+ a b))\n(define-public (run) (begin (print (map both (items) (items))) (ok u0)))",
        // let rebinding a widened binding
        "(define-public (run) (let ((l (items))) (let ((m (append l u9))) (begin (print m) (ok u0)))))",
        // to-consensus-buff? and back
        "(define-public (run) (begin (print (from-consensus-buff? (list 12000 uint) (unwrap-panic (to-consensus-buff? (items))))) (ok u0)))",
        // a widened list compared for equality with a narrow one
        "(define-public (run) (begin (print (is-eq (items) (list u1 u2 u3))) (ok u0)))",
        // a widened list written to a var then filtered
        "(define-private (keep (v uint)) (< v u2))\n(define-public (run) (begin (var-set kept (items)) (print (filter keep (var-get kept))) (ok u0)))",
    ] {
        crosscheck_cost(&format!("{PRELUDE}{body}"), "run", &[]);
    }
}
