---
id: CG-016-safety-comment-substance
level: MUST
paths: ["**/*.rs"]
checks:
  - backend: clippy
    lint: [undocumented_unsafe_blocks]
---

# CG-016-safety-comment-substance: State the concrete safety argument

- **Rule**: Every unsafe block MUST have a preceding `// SAFETY:` justification; every unsafe impl MUST have a safety argument.
- **Scope**: Unsafe blocks and unsafe impls, including generated code compiled by the selected configuration.
- **Why**: An explicit proof obligation makes unsafe operations auditable.
- **How to check**:
  1. Run the backend with the configured platform and review each diagnostic.
  2. Clippy checks comment presence. Review must identify the operation, concrete validity, lifetime, alignment, bounds, initialization, aliasing, synchronization, execution-context, pinning or FFI requirements, and who or which check or guard establishes each one. Placeholder claims are insufficient.
- **Exceptions**: No handwritten exemption. Regenerated bindgen modules may carry a focused generated-code allowance; this does not establish soundness of their consumers. Use the syntax in [README.md](README.md#suppressing-a-check).
- **Counter-example**:

```rust,ignore
let value = unsafe { pointer.read() };
```

This violates the requirement or needs a specific permitted exception.

- **Example**:

```rust,ignore
let value = 42_u32;
let pointer = &value as *const u32;
// SAFETY: pointer comes from the live, aligned, initialized u32 local;
// reading copies the value without changing the shared pointee.
let copied = unsafe { pointer.read() };
```
