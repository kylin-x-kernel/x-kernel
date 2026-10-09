---
id: CG-034-used-binding-names
level: SHOULD
paths: ["**/*.rs"]
checks:
  - backend: clippy
    lint: [used_underscore_binding]
---

# CG-034-used-binding-names: Name used bindings without an underscore prefix

- **Rule**: Bindings that are used SHOULD NOT have an underscore prefix.
- **Scope**: Used Rust variable bindings.
- **Why**: An underscore prefix communicates intentional non-use.
- **How to check**:
  1. Run the backend with the configured platform and review each diagnostic.
  2. Rename used bindings to reflect their role; keep underscore prefixes for intentionally unused values.
- **Exceptions**: None. Use the syntax in [README.md](README.md#suppressing-a-check).
- **Counter-example**:

```rust,ignore
let _value = read();
consume(_value);
```

This violates the requirement or needs a specific permitted exception.

- **Example**:

```rust,ignore
let value = read();
consume(value);
```
