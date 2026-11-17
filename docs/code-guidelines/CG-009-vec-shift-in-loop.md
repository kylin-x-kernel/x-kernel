---
id: CG-009-vec-shift-in-loop
level: SHOULD
paths: ["**/*.rs"]
checks:
  - backend: dylint
    lint: [vec_shift_in_loop]
---

# CG-009-vec-shift-in-loop: Avoid shifting vectors inside loops

- **Rule**: Do not call `Vec::remove` or `Vec::insert` inside a loop body.
- **Scope**: Resolved `Vec::remove` and `Vec::insert` calls in syntactic `loop`, `while`, and `for` bodies, including method syntax through references and UFCS. The check covers compiled code, including macro expansions when source spans are available. It stops at closure boundaries because a closure may be stored or returned instead of running during each iteration. Iterator callbacks such as `.for_each` are outside this syntactic-loop check.
- **Why**: These operations shift later elements and can make repeated loop work quadratic.
- **How to check**:
  1. Find calls to `Vec::remove` or `Vec::insert` whose receiver is a `Vec`.
  2. Check whether the call is nested in a loop body.
  3. If it is intentional, verify the vector is small or bounded so the O(n) shift does not matter.
- **Exceptions**: Use the paired Dylint exception attributes from
  [README.md](README.md#suppressing-a-check). An allow reason must identify why the vector is small or bounded. Use `#[allow(unknown_lints, vec_shift_in_loop, reason = "CG-009-vec-shift-in-loop: <why the vector is small or bounded>")]`. Review still covers `Vec` growth on hot paths.
- **Counter-example**:

```rust,ignore
for item in items { items.remove(0); }
```

Repeated shifting can make this loop quadratic.
- **Example**:

```rust,ignore
// The queue contains at most four entries by construction.
#[expect(clippy::allow_attributes, reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy")]
#[allow(unknown_lints, vec_shift_in_loop, reason = "CG-009-vec-shift-in-loop: queue is bounded to four entries")]
for _ in 0..count { queue.remove(0); }
```
- **Run**: `make dylint` scans the configured kernel; `make dylint_test` runs UI tests.
