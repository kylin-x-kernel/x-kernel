---
id: CG-025-minimal-unsafe-scope
level: MUST
paths: ["**/*.rs"]
checks:
  - backend: clippy
    lint: [multiple_unsafe_ops_per_block]
  - backend: rustc
    lint: [unsafe_op_in_unsafe_fn]
---

# CG-025-minimal-unsafe-scope: Keep unsafe blocks small

- **Rule**: Each unsafe block MUST be as small as practical; unsafe functions also use explicit minimal unsafe blocks.
- **Scope**: Unsafe Rust operations in functions, methods and macros.
- **Why**: A small unsafe scope makes each proof obligation and the safe surrounding work visible.
- **How to check**:
  1. Run the backend with the configured platform and review each diagnostic.
  2. Clippy checks multiple unsafe operations and rustc requires explicit blocks in unsafe fn. Review safe work embedded in an unsafe block: unrelated allocation, control flow or state changes belong outside it. Preserve required lock and cleanup ordering.
- **Exceptions**: Macro-internal unsafe sequences may use a narrow expectation, or a macro allowance if the expectation is unreliable. No crate opt-outs are registered. Use the syntax in [README.md](README.md#suppressing-a-check).
- **Counter-example**:

```rust,ignore
unsafe { left.read(); right.read(); }
```

This violates the requirement or needs a specific permitted exception.

- **Example**:

```rust,ignore
// SAFETY: left points to a live, aligned, initialized u32.
let a = unsafe { left.read() };
// SAFETY: right points to a separate live, aligned, initialized u32.
let b = unsafe { right.read() };
```
