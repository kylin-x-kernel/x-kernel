---
id: CG-029-shallow-control-flow
level: SHOULD
paths: ["**/*.rs"]
checks:
  - backend: clippy
    lint: [excessive_nesting]
---

# CG-029-shallow-control-flow: Minimize unnecessary nesting

- **Rule**: Code SHOULD minimize unnecessary nesting.
- **Scope**: Blocks, including mod, impl and function bodies. Threshold 6 leaves four control-flow levels inside a method.
- **Why**: Shallow control flow makes failure and cleanup paths easier to follow.
- **How to check**:
  1. Run the backend with the configured platform and review each diagnostic.
  2. Prefer early returns, let ... else and extracted functions. The metric does not prove a nesting level unnecessary. Flattening must preserve lock scope and cleanup order.
- **Exceptions**: Only test scenarios or `TODO(cg): legacy` with a concrete flattening plan. Use the syntax in [README.md](README.md#suppressing-a-check).
- **Counter-example**:

```rust,ignore
if let Some(value) = value { if is_ready { process(value); } }
```

This violates the requirement or needs a specific permitted exception.

- **Example**:

```rust,ignore
let Some(value) = value else { return };
if !is_ready { return; }
process(value);
```
