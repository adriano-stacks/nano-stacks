use clar2wasm::tools::crosscheck;
use clarity::vm::Value;

fn check(snippet: &str, expected: u128) {
    crosscheck(snippet, Ok(Some(Value::UInt(expected))));
}

/// The smallest shape found so far.
#[test]
fn c_one_division() {
    check(
        r#"
(define-read-only (f (x uint))
  (let (
    (shares u33619060)
    (price u14073473)
    (value (* price x))
    (bin-value (* price u419249642))
    (dlp (if (or (is-eq shares u0) (is-eq bin-value u0))
             (sqrti value)
             (/ (* value shares) bin-value)))
  )
    dlp))
(f u2204130835)
"#,
        176_746_261,
    );
}

/// Same, with the `or` replaced by a single `is-eq`.
#[test]
fn d_single_condition() {
    check(
        r#"
(define-read-only (f (x uint))
  (let (
    (shares u33619060)
    (price u14073473)
    (value (* price x))
    (bin-value (* price u419249642))
    (dlp (if (is-eq bin-value u0)
             (sqrti value)
             (/ (* value shares) bin-value)))
  )
    dlp))
(f u2204130835)
"#,
        176_746_261,
    );
}

/// Same, without `sqrti` in the untaken branch.
#[test]
fn e_no_sqrti() {
    check(
        r#"
(define-read-only (f (x uint))
  (let (
    (shares u33619060)
    (price u14073473)
    (value (* price x))
    (bin-value (* price u419249642))
    (dlp (if (is-eq bin-value u0)
             u0
             (/ (* value shares) bin-value)))
  )
    dlp))
(f u2204130835)
"#,
        176_746_261,
    );
}

/// Small numbers, same shape.
#[test]
fn f_small_numbers() {
    check(
        r#"
(define-read-only (f (x uint))
  (let (
    (shares u3)
    (price u5)
    (value (* price x))
    (bin-value (* price u7))
    (dlp (if (is-eq bin-value u0)
             u0
             (/ (* value shares) bin-value)))
  )
    dlp))
(f u11)
"#,
        4,
    );
}

/// No `let` at all: the same expression inline.
#[test]
fn g_no_let() {
    check(
        r#"
(define-read-only (f (x uint))
  (if (is-eq (* u5 u7) u0) u0 (/ (* (* u5 x) u3) (* u5 u7))))
(f u11)
"#,
        4,
    );
}

/// A `let` binding read once in a condition and once in the branch.
#[test]
fn h_two_reads() {
    check(
        r#"
(define-read-only (f (x uint))
  (let ((d (* u5 u7)))
    (if (is-eq d u0) u0 (/ x d))))
(f u70)
"#,
        2,
    );
}
