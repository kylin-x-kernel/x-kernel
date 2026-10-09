---
id: CG-026-unsafe-api-safety-docs
level: MUST
paths: ["**/*.rs"]
checks:
  - backend: clippy
    lint: [missing_safety_doc]
---

# CG-026-unsafe-api-safety-docs: Document unsafe API obligations

- **Rule**: Every unsafe function and unsafe trait MUST document a `# Safety` contract.
- **Scope**: Public and private items; clippy.toml sets check-private-items = true.
- **Why**: Callers and implementors need concrete obligations they can discharge.
- **How to check**:
  1. Run the backend with the configured platform and review each diagnostic.
  2. Check the section exists and describes caller or implementor obligations. Comment substance remains a review responsibility, not something presence checking proves.
- **Exceptions**: Only regenerated bindgen binding modules may allow this check; handwritten APIs need a contract. Use the syntax in [README.md](README.md#suppressing-a-check).
- **Counter-example**:

```rust,ignore
unsafe fn read_value(pointer: *const u32) -> u32 { /* ... */ }
```

This violates the requirement or needs a specific permitted exception.

- **Example**:

```rust,ignore
/// # Safety
/// pointer must be live, aligned and initialized for a u32 read.
unsafe fn read_value(pointer: *const u32) -> u32 {
    // SAFETY: the caller establishes the documented pointer requirements.
    unsafe { pointer.read() }
}
```
