---
id: CG-006-user-copy-abi-contract
level: MUST
paths: ["**/*.rs"]
checks:
  - backend: dylint
    lint: [user_copy_without_abi_contract]
---

# CG-006-user-copy-abi-contract: Bound typed user copies by ABI contracts

- **Rule**: Do not copy raw bytes into arbitrary `T` without a `UserRead`-style
  validity contract. Every typed user-memory read must have a proven
  `T: UserRead` or `T: bytemuck::AnyBitPattern` bound; every typed write must have a proven `T: UserWrite`
  bound. Prefer the bounded `UserConstPtr` / `UserPtr` APIs. The lint also
  accepts a direct `osvm` copy when the matching trait is already proven.
- **Scope**: Typed copies between kernel and current user memory in Rust code
  compiled by the selected configuration. Byte buffers (`u8` or
  `MaybeUninit<u8>`) do not assert a typed ABI contract. CI scans the
  aarch64 and x86_64 `qemu_defconfig` configurations; other code is not covered.
  The lint treats a `u8` element as a byte buffer; review must trace byte
  slices manufactured by casting a typed value to bytes. The lint checks
  `read_uninit` at the source when its target lacks a proven read contract,
  including reads retained as `MaybeUninit<T>` for later validation. This
  avoids relying on partial dataflow through `?`, saved results, or
  `assume_init_read`.
- **Why**: Raw copies bypass the proof that arbitrary user bytes make a valid
  Rust value and that outbound bytes are fully initialized. This can cause
  undefined behavior or disclose kernel data through padding.
- **How to check**:
  1. Resolve calls to typed `osvm` copy functions and virtual-pointer methods
     by definition, including UFCS, `NonNull`, generic pointer receivers,
     aliases, and external macros.
  2. Identify the copied element type, unwrapping `MemResult<T>` for reads.
     For inbound copies require `UserRead` or `AnyBitPattern`; for outbound
     copies require `UserWrite`. Follow generic
     callers or add a bound to the helper. A cast to `u8` does not justify
     copying a typed record as bytes.
  3. Replace a typed call with `UserConstPtr<T>::read_vm` or
     `UserPtr<T>::write_vm` / `write_vm_slice`, then make its ABI implementation
     satisfy CG-005. Those wrappers copy bytes and accept misaligned user
     addresses; callers that relied on an alignment error must validate it
     explicitly.
- **Exceptions**: Use the paired Dylint exception attributes from
  [README.md](README.md#suppressing-a-check). A low-level API that only forwards opaque bytes as
  `MaybeUninit<T>` may use a focused allow when its public contract requires
  callers to validate before constructing `T`. Test fixtures may use direct
  copy primitives when they deliberately exercise that low-level API.
  Suppress on the containing item with
  `#[allow(unknown_lints, user_copy_without_abi_contract,
  reason = "CG-006-user-copy-abi-contract: <specific test purpose>")]`.
  The reason must identify the forwarding API or test-only caller and explain
  how it prevents an invalid `T` from being constructed. Production typed
  copies that construct `T` have no exemption.
  If `UserRead` / `UserWrite` later move into `osvm` and bound its generic
  copy APIs, rustc will enforce the write side and that half of this lint can
  be removed.
- **Counter-example**:

```rust
osvm::write_vm_mem(user_addr, core::slice::from_ref(&reply))?;
```

- **Example**:

```rust
UserPtr::<Reply>::from(user_addr).write_vm(reply)?; // Reply: UserWrite
```

- **Run**: `make dylint` scans the configured kernel; `make dylint_test` runs
  UI tests. See `tools/lints/README.md`.
