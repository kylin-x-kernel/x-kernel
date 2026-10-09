---
id: CG-018-uninitialized-storage-model
level: MUST
paths: ["**/*.rs"]
checks:
  - backend: clippy
    lint: [disallowed_methods]
---

# CG-018-uninitialized-storage-model: Model initialization state explicitly

- **Rule**: Code MUST NOT use `mem::zeroed()` or `assume_init()` without proof that the concrete target type admits the resulting value. Storage that may be uninitialized needs `MaybeUninit<T>` or an equivalent explicit model.
- **Scope**: Uninitialized slots, field-by-field initialization and arrays in Rust code.
- **Why**: Invalid values or references can cause undefined behavior before later writes repair the bytes.
- **How to check**:
  1. Run the backend with the configured platform and review each diagnostic.
  2. Prefer safe construction. `disallowed_methods` flags `core::mem::zeroed`; default `invalid_value` and `uninit_assumed_init` checks remain auxiliary in lints.md. For every other `assume_init`, the SAFETY argument must establish initialization, including every array element and cleanup of partial initialization.
- **Exceptions**: A zeroed-value exception must prove that all-zero is valid by the concrete type contract. An allowance cannot excuse an invalid value. Current site: `process/kfd/src/stat.rs`, whose linux_raw_sys stat fields are integers and integer arrays. Use the syntax in [README.md](README.md#suppressing-a-check).
- **Counter-example**:

```rust,ignore
let value = unsafe { core::mem::zeroed::<core::num::NonZeroUsize>() };
```

This violates the requirement or needs a specific permitted exception.

- **Example**:

```rust,ignore
let mut slot = core::mem::MaybeUninit::<u32>::uninit();
slot.write(42);
// SAFETY: write initialized the entire slot with a valid u32.
let value = unsafe { slot.assume_init() };
```
