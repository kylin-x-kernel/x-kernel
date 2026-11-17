---
id: CG-012-private-foreign-items
level: SHOULD
paths: ["**/*.rs"]
checks:
  - backend: dylint
    lint: [exported_foreign_item]
---

# CG-012-private-foreign-items: Keep foreign declarations crate-private

- **Rule**: Items declared in an `extern` block must not be reachable outside their crate.
- **Scope**: Foreign functions and statics whose visibility, enclosing module visibility, or re-exports make them externally reachable.
- **Why**: Foreign declarations encode ABI and safety obligations and should remain behind the crate's narrow wrapper API.
- **How to check**:
  1. Find foreign items declared in `extern` blocks.
  2. Check effective visibility, including the visibility of containing modules and public re-exports.
  3. Make declarations private and expose a safe or appropriately constrained wrapper where needed.
- **Exceptions**: No general exception. A required crate-level ABI surface needs review and an allow reason naming its external contract.
- **Counter-example**:

```rust,ignore
pub mod ffi { unsafe extern "C" { pub fn device_reset(); } }
```

This exposes the raw foreign obligation as a public crate API.
- **Example**:

```rust,ignore
mod ffi { unsafe extern "C" { fn device_reset(); } }
```
- **Run**: `make dylint` scans the configured kernel; `make dylint_test` runs UI tests.
