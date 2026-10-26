---
id: CG-041-reuse-safe-boundary-abstractions
level: SHOULD
paths: ["**/*.rs"]
checks:
  - backend: ai
triggers:
  - '\b(?:read_volatile|write_volatile|from_raw_parts(?:_mut)?|copy_nonoverlapping)\s*\('
  - '\bUnsafeCell\b'
  - '\bunsafe\s+extern\s*"'
---

# CG-041-reuse-safe-boundary-abstractions: Use the existing safe boundary abstraction

- **Rule**: Code SHOULD use an existing matching audited safe boundary
  abstraction instead of open-coding raw operations or foreign ownership.
  Safe-only layers should keep unsafe operations behind existing safe layers;
  where an invariant can be centralized, expose a safe wrapper rather than
  export raw obligations to unrelated callers.
- **Scope**: Raw-pointer, user-memory, MMIO/PIO, refcounting, pinning,
  synchronization and FFI boundaries, plus reused raw output formats.
  Not this rule: [CG-025](CG-025-minimal-unsafe-scope.md) owns minimal unsafe
  block scope; [CG-019](CG-019-user-pointers-stop-at-syscall-boundary.md) owns
  user-access placement at syscall frontends and the pending Dylint lint `unsound_user_abi_impl` ([pending](pending.md))/
  the pending Dylint lint `user_copy_without_abi_contract` ([pending](pending.md)) own user-copy ABI contracts.
  This rule owns matching-helper reuse, including user-memory code outside
  syscall frontends. Capacity proof remains [CG-020](CG-020-raw-output-buffer-capacity.md),
  and foreign declaration visibility remains the pending Dylint lint `exported_foreign_item` ([pending](pending.md)).
- **Why**: Centralizing validation, ownership and unsafe invariants makes
  the boundary auditable and prevents scattered raw foreign calls. The
  [Nomicon FFI guidance](https://doc.rust-lang.org/nomicon/ffi.html) recommends
  safe interfaces when wrappers can validate inputs and centralize obligations.
- **How to check**:
  1. Identify the raw operation and read the immediately relevant subsystem API
     for an existing helper that carries the same invariants.
  2. For user memory, prefer UserPtr<T>/UserConstPtr<T> for typed pointers,
     UserRead/UserWrite for by-value ABI objects, core/kuaccess or process/osvm
     checked copies, and IoVectorBuf, VmBytes, VmBytesMut or equivalent bulk
     wrappers. This applies outside syscall frontend paths as well.
  3. For MMIO/PIO, use existing device-resource helpers, typed register wrappers
     or central driver glue rather than open-coded fixed-address volatile
     access. Reused output formats should use typed buffer wrappers.
  4. For raw/FFI ownership, check that a safe wrapper translates raw buffers and
     ownership into Rust types and centralizes the invariant. Prefer matching
     local helpers for refcounting, pinning and synchronization too. Identify
     any actual mismatch before requesting a replacement.
- **Exceptions**: Foundational implementations, absent matching helpers, distinct
  invariants and intentional low-level tests may retain raw operations. State
  why the matching abstraction is unavailable or unsuitable; a safe-looking
  wrapper that merely hides unchecked obligations does not satisfy this rule.
- **Counter-example**:

```rust,ignore
// High-level device code repeats fixed-address register access.
// SAFETY: address is the live device's aligned 32-bit status register;
// its resource contract permits this volatile read and no Rust reference exists.
let status = unsafe { core::ptr::read_volatile(address as *const u32) };
```

The higher layer repeats a raw register boundary even though its device-resource helper can carry it.

- **Example**:

```rust,ignore
let status = device.registers.read_status();
```
