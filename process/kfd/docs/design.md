# kfd design

## Purpose and background

`kfd` maps process-local integer descriptors to `Arc<kvfs::VfsFile>` objects and
converts kernel metadata to Linux `stat`/`statx`. `kresources` attaches the table
to a process and orchestrates close outside locks. POSIX adapters validate ABI
rules and authorization. File operations, offsets, private object state, and
I/O synchronization belong to `kvfs` and its `FileOperations` implementations.
There is no local `FileLike` trait or file-like downcast layer.

## Scope and architecture

- `src/lib.rs`: public re-exports and kernel tests.
- `src/fd_table.rs`: `FdTable`, fixed-capacity slot operations and final cleanup.
- `src/file_descriptor.rs`: `FileDescriptor` and `FdSnapshot` ownership.
- `src/stat.rs`: `Kstat` and metadata/ABI conversions.

```text
kresources -> Arc<RwLock<FdTable>>
                         |
       FlattenObjects<FileDescriptor, krlimit::FILE_LIMIT>
                         |
          FileDescriptor { Arc<VfsFile>, cloexec }
                         |
                    kvfs operations
```

`FdSnapshot` retains the same file plus captured descriptor number, close-on-exec
bit, and open flags. It is not a live view of later descriptor reuse or flag
changes. `Kstat` is an independent value object with public metadata fields.

## Execution context and concurrency

`FdTable` has no interior lock. Standalone exclusive ownership suffices; shared
lookup needs the external read lock and mutation the write lock. `new_shared`
and `clone_shared_from` allocate, and the latter takes the source read lock.
Snapshots allow callers to unlock before procfs path traversal or exec loading.
No current process, CPU-local state, or hardware mapping is required locally.
Use task context with heap and lock services available; close invokes VFS flush
and may block or reenter callers, so these flows are not interrupt-safe.

## Slot lifecycle and algorithms

A slot is absent or holds a descriptor; these are conceptual states, not a Rust
state enum. `add_file` and `duplicate_from` use the same private allocator to
insert in the lowest unassigned slot in
`[minimum, min(soft_limit, FILE_LIMIT))`. Ordinary insertion supplies zero;
`F_DUPFD` supplies its requested minimum. Physical capacity is
`krlimit::FILE_LIMIT` (1024), which also bounds the search for an infinite limit.
The allocator delegates range selection to `FlattenObjects::add_in_range`, which
reuses the container's single bitmap and skips fully occupied words. It does
not check every occupied descriptor while holding the table write lock. The
local dependency preserves upstream 0.2.4 ownership and slot operations; its
provenance and sole API extension are in `third_party/flatten_objects/XKERNEL-PATCH.md`.
`duplicate_from` checks the source first and rejects a negative minimum or one
at/above the soft limit before entering the shared allocator. These
command-specific errors remain separate from ordinary insertion. Existing high descriptors after a limit
reduction do not count against free low slots. Source lookup and insertion use
one exclusive table access; failures do not replace entries or change flags.
The duplicate shares the open file and receives its own requested `cloexec` bit.
Regression coverage includes lowering the limit below the occupied count while
low slots remain free, mixing ordinary insertion and minimum-based duplication,
and rejecting both operations once every permitted low slot is occupied.
`insert_file` skips the soft-cap check and returns the uninserted descriptor when
full. Internal raw slot helpers are `pub(crate)`.

Lookups return `BadFileDescriptor` for absent entries, including negative signed
FDs converted to out-of-range indices. A snapshot copies flags and clones the
file reference while the caller has stable table access.

`duplicate_to` validates target capacity, clones the source descriptor, sets the
requested close-on-exec flag, and returns any replaced descriptor for later
close. Equal source/target succeeds without changing flags after checking that
the source exists. The syscall layer must impose its own dup2/dup3 distinctions.
This operation does not apply the process soft limit.

Range removal clips its upper bound to the largest occupied index and ignores
holes. Callers validate nonnegative ordered ranges. Close-on-exec removal first
collects matching IDs, then removes them. These APIs return entries and do not
run their close callbacks under the table lock.

## Metadata conversion

`From<kvfs::Metadata>` populates `Kstat`; conversion to Linux `stat` starts
with a zero value then assigns fields; `statx` uses a safe struct literal with
explicit zeros for reserved and unsupported fields. Time values become
seconds/nanoseconds;
`statx` sets `stx_mask` to `STATX_BASIC_STATS` for the basic fields populated by
every conversion, splits device major/minor, and additionally advertises
4096-byte atomic-write units for regular-file modes. Numeric casts follow
target field widths; conversion is not a fallible range-checking API. ABI
copyout belongs to POSIX/user-access code.

## Decisions and resource release

Per-descriptor flags stay with slots; shared file state stays with `VfsFile`.
`clone_shared_from` copies slots and flags while sharing their file references.
This supports separate fork tables without duplicating open-file objects.

Removal transfers an obligation to call `FileDescriptor::close` after unlocking.
Merely dropping that value releases its `Arc` but is not the explicit descriptor
flush operation. `FdTable::drop` removes and closes all remaining descriptors,
ignoring individual close errors so one failure cannot prevent other cleanup.
A snapshot or cloned table owner can intentionally delay final file release.
