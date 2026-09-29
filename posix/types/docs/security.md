# posix-types — Security and reliability

## Scope, assets and trust

The analysis covers every source file listed in `design.md`. The protected
assets are kernel memory integrity (no user pointer becomes a kernel
dereference; no unvalidated bit pattern becomes a typed value) and availability
(bounded work and allocation per call). Trusted callers are kernel syscall
adapters; the untrusted input is user memory behind the wrapped addresses and
the ABI integers carried in the structures. The crate performs no
authorization: permission, identity, and policy checks belong to the adapters
after copying.

## Boundaries and inputs

| Entry | Input / direction | Check and outcome |
|---|---|---|
| `UserPtr`/`UserConstPtr` construction, `cast`, `is_null` | integer address into wrapper | None; no memory is accessed until a `*_vm`/`load_*` helper runs. |
| `read_vm`/`read_uninit`/`load_vm_vec` | user memory into kernel | `osvm` checked copy; alignment and accessibility enforced by the provider. Representation validity comes from `T: UserRead`. |
| `write_vm`/`write_vm_slice` | kernel value into user memory | `osvm` checked copy; `T: UserWrite` guarantees initialized bytes including padding; partial writes possible on fault. |
| `load_string_with_max_len`/`load_bytes_with_max_len` | NUL-terminated user buffer | Bound enforced (`max_len + 1` probed bytes); `OutOfRange`/`InvalidInput` on overrun, UTF-8 checked only by the string variants. |
| `IoVectorBuf::from_iovecs` | copied descriptors | ≤ 1024 segments, non-negative lengths, checked total sum; rejects before any user I/O. |
| `FdSet::read_from_user` | bitmap + `nfds` | Bits ≥ `nfds` cleared after copy; `nfds ≥ FD_SETSIZE` accepted unchanged (Linux behavior); null pointer means absent. |
| `check_sigset_size` | `pselect6` size word | Must equal 8; otherwise error. |
| `TimeSpanLike`/`SystemTimeLike` conversions | ABI time fields | Subsecond range validated; unrepresentable results are `InvalidInput`; pre-epoch realtime deadlines rejected. |
| IPC/fs/process/task POD carriers | ABI structs by value | Copy traits only; field semantics (e.g. permissions in `IpcPerm`, `rusage` counters) are validated by their owners. |

## Unsafe inventory and invariants

All unsafe sites carry inline `SAFETY:` justifications.

| Site | Operation | Invariant |
|---|---|---|
| `ptr.rs` `UserRead`/`UserWrite` trait defs and impls | unsafe traits | implementers must guarantee any user bit pattern is a valid initialized `T` (read) / fully initialized byte representation including padding (write); audited for scalars, arrays, raw pointers, `UserPtr`, and the listed POD `linux_raw_sys` structs |
| `ptr.rs` `as_uninit_bytes`/`spare_as_uninit_bytes` | byte-view of `MaybeUninit` | `MaybeUninit<T>` may always be viewed as an equally sized uninit byte slice |
| `ptr.rs` `write_vm_slice` | byte-view of a live slice | same allocation and length; layout-preserving reinterpretation |
| `ptr.rs` `read_vm`/`load_vm_vec`/`load_bytes_with_max_len` | `assume_init` | exactly the copied bytes were initialized by the preceding `osvm` call; `T: UserRead`/element-by-element init makes them valid |
| `ptr.rs` `bytemuck::Zeroable`/`AnyBitPattern` for `UserPtr` | auto-trait override | `repr(transparent)` over a raw pointer; all bit patterns are plain addresses |
| domain modules `unsafe impl User{Read,Write}` | trait attachment | each listed `linux_raw_sys` struct is plain-old-data with explicit ABI fields (no implicit padding for write side) |

The crate never dereferences a user pointer itself; every access goes through
`osvm`, whose provider owns the fault-checked copy.

## Threats and responses

| ID | Threat | Severity | Trigger | Response |
|---|---|---|---|---|
| T-01 | Kernel dereference of a forged user address | High (if violated) | bypassing the wrappers and dereferencing directly | no deref API exists; review rule: adapters must use `osvm`-routed helpers only |
| T-02 | Invalid bit pattern typed as `T` (e.g. padding or union read as bool) | High (if violated) | `read_uninit` + `assume_init` on a non-`UserRead` type | `read_vm` requires `T: UserRead`; `read_uninit` returns `MaybeUninit` precisely to keep this decision explicit |
| T-03 | Padding-byte information disclosure to user space | High | `UserWrite` on a struct with implicit padding, or a raw `osvm` write of such a struct | trait contract forbids implicit padding; padded `linux_raw_sys` structs use explicit-padding `k_*` carriers with layout assertions instead of carrying `UserWrite`; newly attached impls and raw `osvm` writes, which have no `UserWrite` bound, remain review-time checks |
| T-04 | Unbounded allocation via user length | Medium | `load_vm_vec(ptr, huge_len)` or unbounded `load_string` | bounded variants exist and are preferred; adapters must cap counts (documented caller contract, same class as Linux `memdup_user`) |
| T-05 | TOCTOU on copied descriptors or strings | Low/Medium | user mutates memory between copy and use | copies are single-pass snapshots at copy time; semantic revalidation is the adapter's documented responsibility |
| T-06 | iovec count/length abuse | Medium | > 1024 segments, negative `iov_len`, total overflow | rejected in `from_iovecs` before any user-memory I/O |
| T-07 | Time-field abuse (negative nsec, pre-epoch deadlines) | Low | malformed `timespec`/`timeval` | subsecond range checked in every conversion; `try_into_realtime_deadline` rejects pre-epoch |
| T-08 | `nfds` beyond `FD_SETSIZE` accepted | Low | `select` with huge `nfds` | matches Linux semantics (bits above `nfds` cleared, oversized `nfds` leaves carrier unchanged); adapters document their own fd-limit policy |

## Failure handling and FMEA

All fallible APIs return `KResult`/`osvm::MemResult`; errors map to `EFAULT`
via the `osvm`/`kerrno` conversions, `EINVAL`/`OutOfRange` for bound and range
violations, `IllegalBytes` for UTF-8 failures. Failed writes may have partial
effects; earlier iovec segments are not rolled back. No panics are reachable
from user-controlled data on these paths except documented capacity-overflow
cases in allocating helpers (allocator policy).

| ID | Failure mode | Cause | Local effect | System effect | Sev | Response |
|---|---|---|---|---|---|---|
| F-01 | Copy fault mid-buffer | page unmapped under cursor | error, buffer unspecified | syscall returns `EFAULT` | 3 | documented; no partial-copy success |
| F-02 | Allocation failure | heap exhaustion or capacity overflow | error/panic per allocator policy | syscall fails cleanly | 3 | callers treat as resource error |
| F-03 | Descriptor vector rejected | iovec bound violation | `InvalidInput` before I/O | syscall fails, no user writes | 4 | pre-I/O validation |
| F-04 | Stale snapshot used as authority | concurrent user mutation after copy | adapter acts on old values | semantic, not memory-safety | 4 | documented revalidation contract |

## Privacy analysis

Copied payloads may include paths, strings, and IPC messages; the crate copies
and converts without inspection beyond UTF-8 checks and logs nothing.

## Known limitations

- `load_string` (unbounded) probes up to the `osvm` null-scan cap; hostile
  inputs should use the bounded variants.
- `FdSet` operations are whole-carrier copies; no partial-fd optimization.
- The `Pid` scalar reuses `u32` for several identity domains; negative ABI
  selectors are interpreted only by the process-domain owners.

## Audit checklist

- Never attach `UserRead`/`UserWrite` to a struct with implicit padding on the
  write side or non-POD fields on the read side. For a padded Linux structure,
  add a `k_*` carrier with explicit padding fields and layout assertions.
- Adapters that write through `osvm` directly (`write_vm_mem` or a raw-pointer
  `write_vm`), which has no `UserWrite` bound, must copy only padding-free
  values, e.g. individual output fields; prefer `UserPtr::write_vm`.
- Adapters must cap `len` before `load_vm_vec` and prefer bounded string loads.
- New ABI carriers belong in the domain modules with a `SAFETY` comment, not as
  ad-hoc reinterprets in adapters.
