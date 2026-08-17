(define-read-only (answer (value (optional {extra: uint, kept: uint})))
  (default-to {kept: u0} value))
