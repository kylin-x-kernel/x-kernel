---
id: CG-032-camel-case-types
level: SHOULD
paths: ["**/*.rs"]
checks:
  - backend: clippy
    lint: [upper_case_acronyms]
---

# CG-032-camel-case-types: Treat acronyms as words in type names

- **Rule**: Type names SHOULD use CamelCase, including acronyms as words such as IoMemoryArea.
- **Scope**: Rust type and variant names; upper-case-acronyms-aggressive = true.
- **Why**: Consistent capitalization makes identifiers readable without competing acronym conventions.
- **How to check**:
  1. Run the backend with the configured platform and review each diagnostic.
  2. Clippy flags acronyms; default non_camel_case_types remains in the auxiliary/default table. Check mechanically verifiable ABI names before accepting exceptions.
- **Exceptions**: Types or variants that mirror Linux or hardware ABI spellings. Use the syntax in [README.md](README.md#suppressing-a-check).
- **Counter-example**:

```rust,ignore
struct IOMemoryArea;
```

This violates the requirement or needs a specific permitted exception.

- **Example**:

```rust,ignore
struct IoMemoryArea;
```
