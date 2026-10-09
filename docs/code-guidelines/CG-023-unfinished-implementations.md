---
id: CG-023-unfinished-implementations
level: MUST
paths: ["**/*.rs"]
checks:
  - backend: clippy
    lint: [todo, unimplemented]
---

# CG-023-unfinished-implementations: Make missing implementations explicit

- **Rule**: Code MUST NOT use `unimplemented!`. Missing behavior represented by `todo!` requires an authorized repair reason.
- **Scope**: Compiled Rust paths, including platform-specific implementation placeholders.
- **Why**: A missing implementation can terminate ordinary execution; silent no-ops hide the same missing contract.
- **How to check**:
  1. Run the backend with the configured platform and review each diagnostic.
  2. Implement the behavior, or for authorized todo sites identify the missing implementation and planned repair. Do not rename the panic or replace it with a silent empty operation.
- **Exceptions**: `unimplemented` has none. `todo` permits only a reason whose explanation after the rule id starts `TODO(cg): ` and identifies the missing implementation. Use the syntax in [README.md](README.md#suppressing-a-check).
- **Counter-example**:

```rust,ignore
fn configure_irq() { unimplemented!() }
```

This violates the requirement or needs a specific permitted exception.

- **Example**:

```rust,ignore
fn configure_irq() -> Result<(), Error> { controller.configure_irq() }
```
