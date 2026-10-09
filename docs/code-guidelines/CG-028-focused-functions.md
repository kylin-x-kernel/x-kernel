---
id: CG-028-focused-functions
level: SHOULD
paths: ["**/*.rs"]
checks:
  - backend: clippy
    lint: [too_many_lines]
---

# CG-028-focused-functions: Keep functions small and focused

- **Rule**: Functions SHOULD stay small and focused.
- **Scope**: Function bodies; the Clippy threshold is 100 nonblank, noncomment lines.
- **Why**: A focused function exposes control flow, invariants and cleanup responsibilities.
- **How to check**:
  1. Run the backend with the configured platform and review each diagnostic.
  2. The lint counts lines; review must judge single responsibility. Split legacy functions in separate changes and review lock, transaction and cleanup ordering.
- **Exceptions**: Per function only: linear dispatch or registration tables, with one match or independent registrations, each branch roughly ten lines or fewer and no shared mutable state; explain `linear ... table` after the rule id. Also test scenarios and `TODO(cg): legacy` reasons stating how to split. Registered case (b) library root: `kext4`. Use the syntax in [README.md](README.md#suppressing-a-check).
- **Counter-example**:

```rust,ignore
fn process() { /* parsing, allocation, registration and cleanup in one long body */ }
```

This violates the requirement or needs a specific permitted exception.

- **Example**:

```rust,ignore
fn process() -> Result<(), Error> {
    let request = parse_request()?;
    register_request(request)
}
```
