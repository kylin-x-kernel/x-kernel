---
id: CG-036-ordinary-error-propagation
level: SHOULD
paths: ["**/*.rs"]
checks:
  - backend: clippy
    lint: [question_mark]
---

# CG-036-ordinary-error-propagation: Use question mark for ordinary error propagation

- **Rule**: Code SHOULD use `?` for ordinary error propagation.
- **Scope**: Fallible Rust control flow. This default Clippy lint is the sole mechanical enforcement of this rule.
- **Why**: Direct propagation keeps the success path clear without changing the error contract.
- **How to check**:
  1. Run the backend with the configured platform and review each diagnostic.
  2. The lint recognizes directly rewritable early returns; review other ordinary propagation paths and preserve necessary cleanup or logging.
- **Exceptions**: Extra handling required by the layer contract must remain explicit; an exception names that handling. Use the syntax in [README.md](README.md#suppressing-a-check).
- **Counter-example**:

```rust,ignore
if let Err(error) = operation() { return Err(error); }
```

This violates the requirement or needs a specific permitted exception.

- **Example**:

```rust,ignore
operation()?;
```
