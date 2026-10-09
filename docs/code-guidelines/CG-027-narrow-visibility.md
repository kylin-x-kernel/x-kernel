---
id: CG-027-narrow-visibility
level: SHOULD
paths: ["**/*.rs"]
checks:
  - backend: rustc
    lint: [unreachable_pub]
---

# CG-027-narrow-visibility: Use the narrowest useful visibility

- **Rule**: Items SHOULD default to the narrowest visibility that works.
- **Scope**: Rust items and public API reachability in the selected feature graph.
- **Why**: Accidental public implementation details enlarge the external contract.
- **How to check**:
  1. Run the backend with the configured platform and review each diagnostic.
  2. Rustc checks unreachable pub. Review whether pub(crate) can narrow further and whether public items represent the crate semantic boundary. Scan optional features before shrinking configuration-dependent APIs.
- **Exceptions**: Configuration-dependent public APIs and exported-macro helper items may retain reachability. Generated suite items such as unittest::tests_name! and bindgen modules may use an allowance; ordinary items should use the narrowest visibility. Use the syntax in [README.md](README.md#suppressing-a-check).
- **Counter-example**:

```rust,ignore
mod internal { pub fn helper() {} }
```

This violates the requirement or needs a specific permitted exception.

- **Example**:

```rust,ignore
mod internal { pub(super) fn helper() {} }
```
