# osvm security and reliability

## Scope, assets, and boundaries

All of `src/` is covered. This crate is itself a trust boundary: nearly
every input it receives is a user-controlled address or length. Protected
assets are kernel memory integrity (no user pointer may become a kernel
dereference) and availability (bounded work per call). Trusted inputs are
lengths already validated by syscall layers where noted.

## External boundaries

| Boundary | Direction | Content |
|---|---|---|
| User virtual memory | read (inbound) | syscall argument structs, path/env arrays, `siginfo`, futex words, I/O buffers |
| User virtual memory | write (outbound) | syscall results, signal frames, I/O data |
| Provider (`kuaccess::Vm` via `VirtMemIo`) | kernel-internal | exception-table-guarded byte copy |

## Unsafe inventory

| Location | Operation | Invariant |
|---|---|---|
| `lib.rs` `write_vm_mem` | `slice::from_raw_parts` over `src` | `src` is a live kernel slice; byte view is layout-preserving |
| `ptrs.rs` `VirtPtr::read_vm` | `assume_init` | `AnyBitPattern` guarantees every bit pattern is a valid `Target` |
| `heap.rs` `load_vec_unsafe` | whole function | caller must guarantee `count` initialized valid `T`s in user memory (documented `# Safety`); safe wrapper `load_vec` recovers the guarantee via `AnyBitPattern` |
| `heap.rs` `load_vec_until_null` | `assume_init_ref`, `set_len` | only on ranges `read_mem` initialized in the immediately preceding call |
| `vm_io.rs` `VmBytes::read` | `from_raw_parts_mut` byte↔`MaybeUninit<u8>` view rebuild | same allocation, length, alignment; preserves a live mutable borrow |

The `unsafe impl ... VirtMemIo` provider implementations live outside this
crate (`kuaccess`) and carry their own SAFETY notes.

Core soundness rule: no operation in this crate forms a typed reference
into user memory; user addresses are used only as `usize` ranges handed to
the fault-checked byte copy.

## Threat analysis

| ID | Threat | Severity | Trigger | Response |
|---|---|---|---|---|
| T-01 | Kernel dereference of a forged user pointer | High | caller bypasses osvm and derefs directly | design rule enforced by review; crate offers no deref API at all |
| T-02 | Page-fault deadlock in provider | Medium | copy fault while caller holds a spinlock | provider contract requires fault-checked copy that fails instead of sleeping indefinitely; callers must not hold non-preemptible locks across osvm calls (documented in ksignal/kfutex designs) |
| T-03 | Unbounded kernel memory via huge `count` | Medium | `load_vec(p, huge_count)` | `load_vec` allocates `count` elements up front — syscall layers must cap counts (residual risk, same class as Linux `memdup_user`); `load_vec_until_null` hard-caps at 128 KiB and returns `ENAMETOOLONG` |
| T-04 | Unbounded scan time / DoS via unterminated array | Medium | missing NUL in user array | fixed batch cap bounds both work and memory; alignment precheck rejects misaligned heads |
| T-05 | `rewind_bytes` pointer underflow | Low | rewinding more than consumed | length is `checked_add`-guarded; cursor unchanged on failure. Residual: `wrapping_sub` on the pointer is intentional because the pair (ptr,len) stays the same range |
| T-06 | Info leak through uninitialized bytes | Medium | writing `MaybeUninit` tail to user | writes always originate from initialized `&[u8]`; `read` paths only ever initialize `out` |
| T-07 | TOCTOU between validation and copy | Medium | user mutates memory between check and use | osvm copies once per call and returns the copied value; semantic revalidation (e.g. path rechecks) belongs to callers that need it |

## FMEA

| ID | Failure mode | Cause | Local effect | System effect | Sev | Response |
|---|---|---|---|---|---|---|
| F-01 | `read_mem` fault mid-buffer | page unmapped under cursor | `NoAccess`, `out` unspecified | syscall returns `EFAULT` | 3 | documented; no partial-copy success |
| F-02 | `write_mem` fault mid-buffer | write-protected page | `NoAccess` | syscall returns `EFAULT`; earlier bytes may be written (same as Linux `copy_to_user`) | 4 | caller-visible; not silently retried |
| F-03 | `Vec` allocation failure | heap exhaustion | `alloc` error propagates as `MemError` via KResult conversion at caller | syscall fails cleanly | 3 | no retry loops inside crate |

## Thread safety

Stateless except for the per-call provider instance; all functions are
reentrant. `VmBytes`/`VmBytesMut` are single-owner cursors (no interior
mutability); they hold a raw user pointer plus a length and are therefore
`!Send`/`!Sync` by default.

## Failure handling

Every fallible API returns `MemResult`/`kio::Result`; there are no panics
in this crate. Error mapping into `KError` (`EFAULT`, `ENOMEM`, `ENAMETOOLONG`) is
centralized in `From<MemError>`.

## Privacy analysis

Payload bytes crossing this layer may include user file data and paths; the
crate copies without inspection or logging.

## Known limitations

- `load_vec` trusts caller-supplied `count` for allocation sizing.
- `VmBytes`/`VmBytesMut` do not defend against callers advancing beyond the
  original range through repeated `rewind_bytes` on a moved cursor; range
  discipline is the caller's contract.

## Audit checklist

- Never replace an osvm call with a raw dereference of a user pointer.
- Cap user-supplied counts before `load_vec`.
- Do not hold non-preemptible locks across calls that can fault.
- New providers must preserve the whole-or-fail `write_mem` contract.


## Locked destination copies

`VirtMemIo::write_mem_nofault` is a distinct provider contract: it must not resolve
faults, must report the copied prefix, and must not transfer bytes on error.
`VmBytesMut` advances only by that count. `fault_in_writeable` may sleep, leaves
user contents unchanged, and must run outside folio or backing-provider locks.
The following copy still uses exception protection because another thread may
unmap or reprotect the range after preparation.
