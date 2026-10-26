---
id: CG-044-unsafe-precondition-assertions
level: SHOULD
paths: ["**/*.rs"]
checks:
  - backend: ai
triggers:
  - '\bunsafe\s+fn\s+[A-Za-z0-9_]+'
---

# CG-044-unsafe-precondition-assertions: Assert cheap deterministic unsafe preconditions

- **Rule**: Unsafe functions SHOULD add debug assertions for cheap,
  deterministically checkable preconditions, or explain why checks are
  unavailable, instead of leaving all checking implicit.
- **Scope**: Unsafe-function preconditions such as arithmetic bounds, alignment
  or local state that can be checked without violating another precondition.
  Not this rule: [CG-026](CG-026-unsafe-api-safety-docs.md) owns the caller's
  documented Safety contract and [CG-016](CG-016-safety-comment-substance.md)
  owns safety-argument substance. [CG-021](CG-021-no-panic-on-reachable-failure.md)
  owns release validation of external input; debug assertions here supplement
  caller contracts and never replace that validation.
- **Why**: Cheap assertions diagnose internal caller-contract violations
  close to their source without pretending to prove arbitrary pointer validity.
- **How to check**:
  1. Read the unsafe function and its caller preconditions.
  2. Identify cheap deterministic checks that can run safely before the unsafe
     operation. Check whether debug assertions or an applicable explanation
     cover them.
  3. Do not dereference an arbitrary pointer to test its liveness. Confirm any
     required release input validation remains in place.
- **Exceptions**: Uncheckable preconditions, checks that would themselves require
  the unproven invariant, or expensive/nondeterministic tests may remain caller
  obligations. Explain the unavailable checks; arbitrary pointer liveness is
  not established by dereferencing it.
- **Counter-example**:

```rust,ignore
/// # Safety
/// index must be less than values.len().
unsafe fn read_at(values: &[u32], index: usize) -> u32 {
    // SAFETY: the caller promises index is in bounds.
    unsafe { *values.get_unchecked(index) }
}
```

The cheap deterministic index bound is left implicit instead of asserted.

- **Example**:

```rust,ignore
/// # Safety
/// index must be less than values.len().
unsafe fn read_at(values: &[u32], index: usize) -> u32 {
    debug_assert!(index < values.len());
    // SAFETY: the caller promises index is in bounds; the assertion diagnoses
    // violations in debug builds and does not replace that caller obligation.
    unsafe { *values.get_unchecked(index) }
}
```
