# krlimit security and reliability

## Scope and trust boundaries

This analysis covers the whole crate (`src/lib.rs`). The protected policy data
is each `Rlimit` pair. Trusted kernel callers provide resource indices and
values. User ABI copies, index validation, authorization, and enforcement belong
to the caller; `kresources::ProcessResources` provides checked limit access.
There is no direct user memory, file/network input, device, MMIO, DMA, firmware,
FFI, or assembly boundary here.

## Unsafe inventory and invariants

There is no unsafe code. Fixed arrays and safe indexing prevent out-of-bounds
memory access. They do not prevent a panic for an invalid index. Public fields
and `IndexMut` deliberately allow any `u64` pair; soft <= hard is a policy
invariant for callers, not a guarantee of `Rlimit::new`.

## Thread safety

The values contain only integers and an array, with automatic `Send`/`Sync`.
There is no interior mutability; shared mutation requires external exclusive
access, such as the `kresources` limits lock.

## Threat analysis

| ID | Threat and asset | Severity | Trigger | Response and residual risk |
|---|---|---|---|---|
| T-01 | Kernel denial of service via resource index | Medium | An unchecked external index reaches `Index`/`IndexMut` | Array bounds checking panics instead of corrupting memory; callers must reject indices >= `RLIM_NLIMITS` before indexing. |
| T-02 | Invalid resource policy | Medium | A trusted caller writes soft > hard or treats storage as enforcement | `kresources::update_rlimit_and_get_old` validates checked updates (soft > hard is `InvalidInput` regardless of the privilege flag); direct construction remains intentionally unchecked. |
| T-03 | Unexpected resource consumption | Medium | An unlimited entry is assumed to provide admission control | Consuming subsystems must enforce relevant limits; this crate only supplies defaults. |

## Failure modes and effects (FMEA)

| ID | Failure mode | Cause | Local effect | System effect | Severity (1-4) | Handling |
|---|---|---|---|---|---|---|
| F-01 | Bounds panic | Invalid resource number | Index operation aborts | Kernel service may terminate | 2 | Validate at the syscall/resource owner boundary. |
| F-02 | Wrong initial stack cap | Incorrect byte count from creator | Wrong reported pair | Program limit behavior differs | 3 | Pass the actual configured user stack size. |
| F-03 | Invalid pair stored | Bypass of update policy | Inconsistent limits | Policy may be bypassed | 2 | Use checked `kresources` access for untrusted requests. |

## Failure handling, privacy, and limitations

Constructors return values, not `Result`; there is no allocation, retry, logging,
or recovery. The crate holds numeric policy metadata, not file contents,
identities, or user buffers. Numeric metadata may still be exposed by caller
syscalls. Fixed descriptor/stack defaults and absent enforcement must be
revisited together if the owning subsystems change capacity.

## Audit checklist

- Validate Linux indices before using the indexing traits.
- Keep defaults consistent with actual kernel capacities.
- Do not equate `Rlimit::new` with validation or authorization.
- Preserve external serialization of shared updates.
