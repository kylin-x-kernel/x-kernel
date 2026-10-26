---
id: CG-019-user-pointers-stop-at-syscall-boundary
level: MUST
paths: ["posix/**", "core/ksyscall/**", "process/**", "core/kuaccess/**"]
checks:
  - backend: ai
triggers:
  - "UserPtr"
  - "UserConstPtr"
  - "from_raw_parts"
  - "copy_from_user"
  - "copy_to_user"
---

# CG-019-user-pointers-stop-at-syscall-boundary: Keep user access at the boundary

- **Rule**: Syscall frontends MUST validate user input through checked user-memory
  abstractions, keep nullable-pointer semantics explicit, and pass copied
  kernel-owned values to deeper subsystem logic instead of raw user pointers.
- **Scope**: User input and copy-in/copy-out at syscall boundaries in the listed
  paths. Use `UserPtr<T>` / `UserConstPtr<T>` for typed pointers, `UserRead` /
  `UserWrite` for by-value ABI objects, `core/kuaccess` / `process/osvm` helpers
  for checked copies, and `IoVectorBuf`, `VmBytes`, `VmBytesMut`, or equivalent
  existing wrappers for bulk buffers. Direct dereference and ad hoc
  `from_raw_parts` over user addresses in syscall logic violate this rule;
  the checked access implementations themselves own the raw access boundary.
  Not this rule: the pending Dylint lint `unsound_user_abi_impl` ([pending](pending.md)) owns validity,
  layout and initialized-byte obligations of `UserRead`/`UserWrite` types,
  and the pending Dylint lint `user_copy_without_abi_contract` ([pending](pending.md)) owns whether a typed copy
  uses the required ABI contract; this rule owns where user access and
  validation occur and whether deeper logic receives kernel values.
  Report a non-ABI initialization-state defect under
  [CG-018](CG-018-uninitialized-storage-model.md), and a capacity defect in
  a kernel raw output buffer under [CG-020](CG-020-raw-output-buffer-capacity.md).
- **Why**: A user address is not a validated kernel value; keeping access and
  validation at the boundary prevents deeper logic from trusting user memory.
- **How to check**:
  1. Trace each user pointer from the syscall argument to its reads and writes;
     check null handling against the syscall's semantics.
  2. Confirm accesses use the existing checked wrappers and validate input at
     the boundary before subsystem logic consumes it.
  3. After copy-in, follow calls into deeper helpers and verify they receive
     kernel-owned values rather than raw user addresses.
- **Exceptions**:
  1. None.
- **Counter-example**:

```rust,ignore
fn sys_set_limit(limit: UserConstPtr<u64>) -> Result<()> {
    process::set_limit_from_user(limit)
}
```

The syscall pushes a user pointer into process logic instead of copying and validating its value at the boundary.

- **Example**:

```rust,ignore
fn sys_set_limit(limit: UserConstPtr<u64>) -> Result<()> {
    let limit = limit.check_non_null().ok_or(Error::EFAULT)?.read_vm()?;
    let limit = ValidatedLimit::try_from(limit)?;
    process::set_limit(limit)
}
```
