# kfd security and reliability

## Scope, assets, and external boundaries

The descriptor, ownership, snapshot and metadata modules are covered. Assets include descriptor/file associations,
close-on-exec flags, stable snapshots, and metadata returned through Linux ABI
structures. Kernel callers supply descriptors, files, and metadata. POSIX owns
user-buffer access, descriptor-range ABI validation, and access authorization;
`kresources` owns process attachment and shared table locking. `kvfs::VfsFile`
and its operations own file-level synchronization and flush semantics.

There is no direct user pointer, device input, MMIO, DMA, firmware, FFI call, or
assembly here. `FileDescriptor::close` crosses the VFS callback boundary. ABI
conversion builds kernel values; it does not copy them to user memory.

## Unsafe inventory

| Location | Operation | Invariant and safe entry |
|---|---|---|
| `src/stat.rs`, `From<Kstat> for linux_raw_sys::general::stat` | `core::mem::zeroed` | The target binding is a plain C ABI struct whose integer fields and integer arrays admit zero; no reference, invalid enum discriminant, or drop-bearing object may be introduced. Conversion fills supported fields after initialization. |

`statx` uses a safe struct literal that initializes every field, including reserved
fields and unsupported metadata. The `stat` entry matches its local `SAFETY`
comment. There is no unsafe `Send`/`Sync` impl.
Zero-valued fields do not by themselves prove that Rust struct padding will be
preserved as zero through arbitrary moves; ABI copyout must obey its own byte
initialization contract rather than infer that guarantee from this conversion.

## Memory invariants and thread safety

Slots and snapshots hold strong `Arc<VfsFile>` references. Descriptor reuse
cannot change the file already held by a snapshot. Safe table borrows enforce
exclusive mutation; the external `RwLock` serializes shared access. File I/O is
not serialized by this table. Close callbacks must run after releasing table
locks; the final table destructor has exclusive ownership of its entries.

## Threat analysis

| ID | Threat and asset | Severity | Trigger | Response and residual risk |
|---|---|---|---|---|
| T-01 | Access through invalid descriptor | Medium | Negative, closed, or out-of-capacity FD | `FlattenObjects` lookup rejects absent indices and returns `BadFileDescriptor`; ABI-specific checks remain external. |
| T-02 | Quota bypass | Medium | Caller uses `insert_file`/fixed-slot duplication for an untrusted request | `add_file` and `duplicate_from` share an allocator that bounds the new descriptor number by the supplied soft limit and physical capacity. Other paths need explicit caller policy. |
| T-03 | File reference leaks across exec | High | Caller omits close-on-exec cleanup | `remove_cloexec_files` returns flagged entries; exec owner must invoke cleanup and close them outside the lock. |
| T-04 | Reentrant close deadlock | Medium | Flush callback needs a held table lock | Removal returns owned entries and `kresources` closes after unlocking; direct callers must do likewise. |
| T-05 | Stale snapshot used as current authorization | High | FD is closed/reused or flags change after snapshot | Snapshot pins the old file intentionally; consumers needing live state must re-query with appropriate authorization. |
| T-06 | Invalid ABI value or leaked fields | High | Binding changes invalidate zero initialization or copyout exposes padding | Re-audit the `stat` zero-validity assumption, both ABI conversions, and copyout on ABI updates; all fields are initialized, but copyout owns byte-level guarantees. |
| T-07 | Incorrect reported I/O capability | Low | Regular-file mode is used for unsupported atomic writes | Conversion advertises fixed atomic-write metadata for all regular files; consumers/providers must not treat it as independent device validation. |

## Failure modes and effects (FMEA)

| ID | Failure mode | Cause | Local effect | System effect | Severity (1-4) | Handling |
|---|---|---|---|---|---|---|
| F-01 | Table/soft-cap exhaustion | No free slot below both soft limit and capacity | `TooManyOpenFiles` | Open/dup fails | 3 | Caller reports failure; `insert_file` instead returns the descriptor. |
| F-02 | Missing source/invalid target | Invalid descriptor number | `BadFileDescriptor` | Descriptor operation fails | 3 | Validate before replacement. |
| F-03 | Flush fails | VFS callback error | Entry already removed | Writeback may fail | 2 | Explicit close returns error; final drop continues ignoring errors. |
| F-04 | Metadata truncation | Wider input than target ABI field | Cast truncates | Incorrect user metadata | 3 | Caller/ABI review must assess supported ranges. |
| F-05 | Destructor invariant broken | Enumerated ID disappears internally | `expect` panic | Cleanup fails | 2 | Exclusive destruction must preserve slot iteration/removal consistency. |

## Failure handling, privacy, and limitations

The crate uses `KResult` for descriptor failures and returns ownership on raw
insertion failure. Range helpers tolerate holes and need caller range checks.
No retry, logging, or persistence is implemented here. `Kstat` and snapshots
expose paths, IDs, inode/device identifiers, and timestamps to kernel consumers;
user-visible exposure requires external policy. Capacity is fixed, snapshots
are intentionally stale, and external `Arc`s may delay final close.

## Audit checklist

- Keep documentation and ABI zero-validity assumptions synchronized with bindings.
- Match insertion/duplication paths to the intended resource policy.
- Close removed or replaced entries exactly once outside shared table locks.
- Preserve per-descriptor flags when cloning tables.
- Do not authorize current-FD operations from stale snapshots.
- Validate actual byte initialization at metadata copyout boundaries.

## Binding teardown

`FdTableOwner` uses safe Arc ownership to distinguish bindings from temporary
storage references. The last binding drains entries even when observers still
pin the table; each entry is removed once under the write lock and closed after
unlocking. This releases POSIX locks promptly and prevents a later storage Drop
from repeating flushes. The retained storage also prevents reuse of the POSIX
owner address during outstanding operations. Writable table borrows require a
live owner. Temporary `FdTableRef` handles keep the storage Arc private and return
a mapped read guard without access to its underlying lock, so safe callers
cannot repopulate a retired table.
