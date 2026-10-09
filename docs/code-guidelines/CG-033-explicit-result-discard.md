---
id: CG-033-explicit-result-discard
level: SHOULD
paths: ["**/*.rs"]
checks:
  - backend: clippy
    lint: [unused_result_ok]
---

# CG-033-explicit-result-discard: Discard results explicitly

- **Rule**: Code SHOULD NOT discard Result through an unused `.ok();` conversion.
- **Scope**: Unused Result-to-Option conversions.
- **Why**: Explicit discard exposes intentional best-effort handling.
- **How to check**:
  1. Run the backend with the configured platform and review each diagnostic.
  2. Handle or propagate legitimate errors. If intentionally discarding, write let _ = expr; and explain why when it is not obvious.
- **Exceptions**: None. Use the syntax in [README.md](README.md#suppressing-a-check).
- **Counter-example**:

```rust,ignore
save_state().ok();
```

This violates the requirement or needs a specific permitted exception.

- **Example**:

```rust,ignore
// Best-effort diagnostic snapshot; shutdown must continue after a failure.
let _ = save_state();
```
