---
id: CG-018-uninitialized-storage-model
level: MUST
paths: ["**/*.rs"]
checks:
  - backend: clippy
    lint: [disallowed_methods]
  - backend: ai
triggers:
  - "mem::zeroed"
  - "assume_init"
  - "MaybeUninit"
---

# CG-018-uninitialized-storage-model: Model initialization state explicitly

- **Rule**: Storage that may be uninitialized MUST use `MaybeUninit<T>` or an
  equivalent explicit model, and code MUST prove the target type's valid
  initialization before constructing a `T`, creating `&T` or `&mut T`, or
  dropping it as initialized.
- **Scope**: Uninitialized slots, arrays, and field-by-field initialization in
  Rust code. Partially initialized arrays need an initialized-count invariant;
  whole-array `assume_init` requires proof for every element. `mem::zeroed()`
  and `assume_init()` cannot supply arbitrary `T` without a type-validity proof.
  Not this rule: the pending Dylint lint `unsound_user_abi_impl` ([pending](pending.md)) owns
  `UserRead`/`UserWrite` layout and initialized-byte contracts and
  the pending Dylint lint `user_copy_without_abi_contract` ([pending](pending.md)) owns the matching contract at
  typed copy sites; this rule owns initialization-state transitions outside
  those specific ABI findings. A missing safety proof for a sound transition
  belongs to [CG-016](CG-016-safety-comment-substance.md), while writing past
  a raw output buffer belongs to [CG-020](CG-020-raw-output-buffer-capacity.md).
- **Why**: Invalid values or references can cause undefined behavior before
  later writes repair the bytes: references and `bool`, for example, have
  invalid representations. Interpreting uninitialized memory as a value is UB;
  [MaybeUninit](https://doc.rust-lang.org/std/mem/union.MaybeUninit.html) models
  storage that may not yet contain a valid `T`, as described by the
  [Rustonomicon](https://doc.rust-lang.org/nomicon/uninitialized.html).
- **How to check**:
  1. Run `make clippy` with the configured platform for the mechanical checks.
     `disallowed_methods` flags `core::mem::zeroed`; default `invalid_value`
     and `uninit_assumed_init` remain auxiliary checks in [lints.md](lints.md).
  2. Trace initialization state through allocation, field or element writes,
     failure paths, and cleanup; check that the model records initialized data.
  3. Check every conversion, reference creation, and `assume_init_drop` against
     that state and the target type's valid bit patterns.
  4. Prefer `MaybeUninit<T>` for slots and `[MaybeUninit<T>; N]` with an
     initialized length/count for arrays; call `write`, `assume_init`, or
     `assume_init_drop` only with the matching initialization-state proof.
     For in-place initialization, use raw field pointers and `write` without
     creating references to uninitialized pointees.
  5. Repository examples: [the syscall table](../../api/linux_sysno/src/map.rs)
     records occupied slots in a bitmap before reading or dropping them.
     The old `task/kpoll/src/lib.rs` example has changed;
     [klazy::Once](../../util/klazy/src/once.rs) demonstrates an initialized
     slot and conditional cleanup instead.
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
