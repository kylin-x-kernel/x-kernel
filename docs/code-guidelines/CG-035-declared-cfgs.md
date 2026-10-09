---
id: CG-035-declared-cfgs
level: SHOULD
paths: ["**/*.rs"]
checks:
  - backend: rustc
    lint: [unexpected_cfgs]
---

# CG-035-declared-cfgs: Declare configuration names and values

- **Rule**: Rust cfg names and feature values SHOULD be known to the configured compilation.
- **Scope**: Conditional compilation; workspace check-cfg explicitly admits cfg(unittest).
- **Why**: A misspelled cfg silently excludes intended code.
- **How to check**:
  1. Run the backend with the configured platform and review each diagnostic.
  2. Use Kconfig-generated features and declared cfgs. Declare a custom name at its owning configuration boundary, rather than suppressing a typo.
- **Exceptions**: None. Use the syntax in [README.md](README.md#suppressing-a-check).
- **Counter-example**:

```rust,ignore
#[cfg(unitest)]
mod tests;
```

This violates the requirement or needs a specific permitted exception.

- **Example**:

```rust,ignore
#[cfg(unittest)]
mod tests;
```
