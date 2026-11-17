---
id: CG-004-static-mut-option
level: MUST
paths: ["**/*.rs"]
checks:
  - backend: dylint
    lint: [static_mut_option]
---

# CG-004-static-mut-option: Avoid open-coded `static mut Option<T>` initialization

- **Rule**: Do not use open-coded `static mut Option<T>` initialization.
- **Scope**: Rust `static mut` items whose resolved type is `Option<T>`.
- **Why**: A mutable static option exposes unsynchronized global mutable state and makes initialization and access invariants difficult to enforce.
- **How to check**:
  1. Find `static mut` items and resolve their declared type.
  2. Report items whose type is `Option<T>`, including type aliases.
- **Exceptions**: None.
- **Counter-example**:

```rust
static mut INSTANCE: Option<MyType> = None;
```

This exposes global mutable state whose initialization and access require manual synchronization.

- **Example**:

```rust
static INSTANCE: klazy::Once<MyType> = klazy::Once::new();
```

- **Run**: `make dylint` scans the configured kernel; `make dylint_test` runs the lint's UI tests. Details: `tools/lints/README.md`.
