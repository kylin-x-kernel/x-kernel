---
id: CG-030-explicit-boolean-parameters
level: SHOULD
paths: ["**/*.rs"]
checks:
  - backend: clippy
    lint: [fn_params_excessive_bools]
---

# CG-030-explicit-boolean-parameters: Make boolean call sites clear

- **Rule**: APIs SHOULD avoid boolean arguments when call sites become unclear.
- **Scope**: Function parameters; max-fn-params-bools is 3.
- **Why**: Adjacent true/false arguments do not identify their meaning at the call site.
- **How to check**:
  1. Run the backend with the configured platform and review each diagnostic.
  2. The lint counts boolean parameters; review single booleans and smaller combinations below the threshold. Use distinct entry points or meaningful enums/flags.
- **Exceptions**: None. Use the syntax in [README.md](README.md#suppressing-a-check).
- **Counter-example**:

```rust,ignore
configure(true, false, true, false);
```

This violates the requirement or needs a specific permitted exception.

- **Example**:

```rust,ignore
configure(Mode::Enabled, Policy::Strict);
```
