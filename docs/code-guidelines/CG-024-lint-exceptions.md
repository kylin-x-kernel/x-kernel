---
id: CG-024-lint-exceptions
level: MUST
paths: ["**/*.rs"]
checks:
  - backend: clippy
    lint: [allow_attributes, allow_attributes_without_reason]
---

# CG-024-lint-exceptions: Justify lint exceptions at the narrowest scope

- **Rule**: Lint exceptions MUST use the narrowest practical scope and a concrete, truthful rule-id reason.
- **Scope**: Rust lint attributes, including generated attributes and inner crate-root opt-outs.
- **Why**: Focused reasons preserve the review obligation instead of hiding unrelated violations.
- **How to check**:
  1. Run the backend with the configured platform and review each diagnostic.
  2. Use the unified syntax in README.md. These Clippy lints check outer allow attributes and presence of a reason; review must check scope and substance. For `expect(dead_code)`, permit only simple intentional code likely to be used, and explain when it will be used.
- **Exceptions**: Only the README unreliability cases permit outer allow. Registered case (b) library roots for both allowance lints: `kext4` and `tee_kernel`; the stricter threshold is twenty semantically diverse legacy sites. Review new code in those crates against scope and reason requirements. Use the syntax in [README.md](README.md#suppressing-a-check).
- **Counter-example**:

```rust,ignore
#[allow(clippy::unwrap_used)]
mod production;
```

This violates the requirement or needs a specific permitted exception.

- **Example**:

```rust,ignore
#[expect(clippy::unwrap_used, reason = "CG-021-no-panic-on-reachable-failure: validated nonempty input guarantees a first element")]
let first = input.first().unwrap();
```
