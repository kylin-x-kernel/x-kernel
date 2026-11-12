# osvm design

## Purpose

`osvm` (Other-Space Virtual Memory) is the checked user-memory access layer
for X-Kernel syscall and signal paths. It offers typed reads/writes of user
pointers, byte-streaming adapters for I/O buffers, and heap-loading helpers,
all routed through a single fault-checked provider contract. Kernel
subsystems (`ksyscall`, `ksignal`, `kexec`, futex, ipc, net) use it instead
of dereferencing user pointers directly; the concrete copy primitive is
implemented by `kuaccess` (`Vm`), supplied via the `extern-trait`
`MemImpl` mechanism so `osvm` itself stays architecture- and kernel-free.

## Scope

```text
process/osvm/src/
├── lib.rs     MemError, VirtMemIo provider contract, typed/byte read/write helpers
├── ptrs.rs    VirtPtr / VirtMutPtr pointer traits
├── vm_io.rs   VmBytes / VmBytesMut streaming adapters
└── heap.rs    load_vec, load_vec_unsafe, load_vec_until_null (alloc feature)
```

Features: `alloc` (default) enables the heap-loading helpers and the
`NameTooLong` error variant; without it the crate is a pure no_std access
layer.

## Architecture

```text
        callers (ksyscall, ksignal, ...)
            | VirtPtr::read_vm / write_vm / VmBytes / load_vec*
            v
        alignment check  ->  MemImpl::new()  ->  VirtMemIo::read_mem/write_mem
            (osvm)                                  (kuaccess Vm impl:
                                                    exception-table-guarded
                                                    byte copy)
```

`read_vm_mem`/`write_vm_mem` enforce natural alignment for typed access and
view the payload as bytes (`MaybeUninit::as_bytes_mut`) so no typed reference
into user memory is ever formed. `read_vm_bytes`/`write_vm_bytes` skip
alignment for byte-granular ABI fields.

`VirtPtr`/`VirtMutPtr` wrap the address-only conventions (`check_non_null`
for the Linux "NULL means absent" rule). `VmBytes`/`VmBytesMut` implement
`kio` `Read`/`Write`/`IoBuf` over a `(ptr, len)` cursor, advancing with
`wrapping_add` and supporting `rewind_bytes` for paths that must hand back
optimistically consumed bytes; `cast_mut`/`cast_const` switch views without
copying.

`load_vec` allocates capacity and fills it via `spare_capacity_mut`, taking
`AnyBitPattern` types; `load_vec_unsafe` is the ABI-validity-agnostic
escape hatch with an explicit `# Safety` contract;
`load_vec_until_null` scans fixed 32-element batches up to a hard 128 KiB
limit for an all-zero terminator.

## Execution context

- All helpers are synchronous, non-sleeping, and usable from any context
  that permits page-fault handling through the provider (kernel syscall and
  trap paths). They take no locks of their own.
- `MemImpl::new()` is expected to be a cheap stack guard (fault-entry
  bookkeeping in `kuaccess`), not a resource acquisition.
- The `alloc` helpers require a working kernel heap; batch scanning in
  `load_vec_until_null` can transiently hold up to one batch of spare
  capacity.

## Error model

| Failure | Mapped result |
|---|---|
| Misaligned typed pointer | `MemError::InvalidAddr` (`EFAULT` via `From` into `KError`) |
| Provider fault/rejection | `MemError::NoAccess` (`EFAULT`) |
| Provider out of memory | `MemError::OutOfMemory` (`ENOMEM`) |
| Terminator beyond 128 KiB (`alloc`) | `MemError::NameTooLong` (`ENAMETOOLONG`) |
| `rewind_bytes` overflow | `kio` `EINVAL` (cursor unchanged) |

Partial writes are impossible by contract: `VirtMemIo::write_mem` either
copies the whole slice or fails. `Read`/`Write` impls propagate provider
errors after advancing the cursor only on success.

## Design decisions

- A provider trait (`extern-trait`) instead of direct kuaccess dependency
  keeps `osvm` reusable by user-mode test harnesses and lets the fault
  strategy live entirely in one place.
- `MaybeUninit` throughout the read path makes the initialization state
  explicit; `AnyBitPattern` bounds the safe `assume_init`.
- Typed alignment is enforced only for `*_vm_mem`/`VirtPtr` helpers;
  byte-level ABI accessors exist because several Linux ABIs pack unaligned
  fields.
- The 128 KiB null-scan cap bounds kernel memory and time per call without
  per-architecture configuration.
- `load_vec_unsafe` exists because some ABI payloads (e.g. bindgen unions)
  cannot satisfy `AnyBitPattern`; the safety burden stays at the single
  audited call sites.

## Resource lifecycle

No global state, no locks, no Drop behavior. `Vec` results are ordinary
kernel-heap allocations owned by the caller.


## Destination iterators

`VmBytesMut` implements `iov_iter::IovSink` using its existing pointer and length.
Ordinary copies retain the whole-or-fail provider contract. Its nofault operation
uses `VirtMemIo::write_mem_nofault` and advances by the returned prefix. Fault
preparation calls `VirtMemIo::fault_in_writeable` for the next byte without
advancing the cursor or modifying user contents. Preparing only the next byte
lets a later invalid page preserve an earlier transferable prefix. Neither
operation pins mappings or retains an MM reference beyond the call.
