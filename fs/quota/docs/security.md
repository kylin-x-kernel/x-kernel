# kquota security and reliability

## Trust model and boundaries

Quota file bytes, sizes, counts, IDs and links are untrusted filesystem input.
`QuotaIo` is a trusted filesystem provider for direct reads/writes and reported
file sizes. Loaded state owns the canonical file reference. The provider must keep its
filesystem context stable, bypass charging for
those files and serialize all core operations. Namespace mapping, permission
checks, resource-bypass authorization and user-memory copying belong to the
VFS/syscall integration; this crate accepts native IDs and values only.

There is no user-pointer, MMIO, PIO, DMA, firmware or FFI boundary in this
crate. No unsafe code or custom synchronization primitive is introduced.

## Invariants

- One `QuotaInfo` is owned by one filesystem. Cache keys uniquely identify
  quota class and filesystem ID; the all-ones ID cannot be constructed.
- Only a fully validated header can become loaded file information. That
  state retains its file through suspension and failed disable; successful
  disable returns it for release outside filesystem locks. Owner destruction
  drops any still-loaded file references. Failed enable borrows the
  supplied file without retaining or publishing it.
- Tree traversal is bounded to four levels. Nonzero child references must
  remain within the file's declared blocks and outside the ancestor path.
- Data counts match occupied slots. A referenced ID must occur exactly once
  in its data block. ID zero cannot alias an unused slot.
- Free-list mutation validates ranges, reciprocal links and head/previous-link
  equivalence before writes.
  Empty tree/data blocks cannot return the root to a free list.
- No per-class usage/deadline update is installed until all active IDs accept
  a charge. Signed arithmetic is checked before installation.
- A reservation is never encoded as allocated disk space. Failed claims
  or reclaims cannot move only a subset of the participating quota classes.
  Reclaim cannot transfer more than an active record's allocated usage.
- Active-record dirty state is cleared only after its write succeeds.
  An initial acquisition remains inactive until its record and file header
  have both been written successfully. Writeback rejects inactive dirty
  records once with `EIO` without doing I/O and clears their dirty indication,
  so later sync/disable can finish. Only acquisition can resume initialization.

## Concurrency and execution context

Safe mutation requires exclusive access through Rust references. KVFS places
the state behind its existing sleepable-mutex abstraction. The core may
allocate and block in I/O and therefore requires task execution with allocator
and scheduler initialization complete. It must not execute under spinlocks,
IRQ-disable or preemption-disable guards, or from IRQ/early-boot contexts.

The provider receives the retained backing file and may mutably borrow the
current filesystem transaction. It must not reenter the same quota state. The filesystem must use
one lock order for quota administration, inode operations and direct quota I/O.
No second quota lock or atomic counter mirrors a field in the protected state.

## Threat analysis

| ID | Threat and trigger | Impact | Implemented control and residual risk |
| --- | --- | --- | --- |
| T1 | Forged block count or child/free pointer | Out-of-file I/O or metadata overwrite | Header size/range validation, checked child ranges and root exclusion; the provider must honor requested offsets |
| T2 | Cyclic radix references | Unbounded traversal or wrong record access | Fixed depth and ancestor-path checks; this is local traversal validation, not a full offline graph audit |
| T3 | Forged data count or referenced-but-missing ID | Charging another record or overwriting occupied space | Occupied-slot counts, unique matching ID and bounded free-slot search |
| T4 | A late group/project refusal after earlier admission | Under/over-accounted resources or premature grace expiry | Fixed-size candidate states published only after all admission checks succeed |
| T5 | Invalid limit, negative charge or counter overflow | Limit bypass through arithmetic wrap | Boundary checks, format limit checks and checked arithmetic returning errors |
| T6 | Short or failed metadata writes | Lost accounting or damaged quota tree | Exact-size writes, dirty-state retention, explicit error propagation; filesystem transactions and abort/recovery must handle structural partial writes |
| T7 | Untrusted caller requests resource bypass or quota changes | Quota policy bypass | No implicit current credentials; trusted VFS/syscall integration must authorize these arguments |
| T8 | Backing-file I/O recursively charges quota | Deadlock or recursive accounting | Direct-I/O integration contract and documented nonreentrancy; internal ext4 quota inodes are exempt by on-disk identity |

## Failure modes

| ID | Failure | Local effect | System effect and response |
| --- | --- | --- | --- |
| F1 | Incompatible magic or revision | Enable returns `EINVAL` | Class remains disabled |
| F2 | Corrupt metadata encountered during lookup/update | `EUCLEAN` | Operation fails; owner decides filesystem/quota recovery policy |
| F3 | Quota limit or expired grace refuses a charge | `EDQUOT` | No usage or deadline change for any participating class |
| F4 | Record overwrite or file-info write fails | First error returned with failed dirty state retained | Independent dirty records are still attempted; explicit synchronization can retry pending writes |
| F5 | Structural update fails after some writes | Error and possibly changed graph/free-list state | Caller must abort/recover its transaction domain before normal work continues |
| F6 | Disable/suspend/release has reservations | `EBUSY` | Backing state stays owned until the filesystem drains reservations |
| F7 | State is dropped without successful disable | In-memory dirty state is lost | No hidden destructor I/O; owning filesystem must perform explicit shutdown |

## Privacy and limitations

IDs and usage reveal filesystem resource ownership. The core does not log
those values; access control for exposing snapshots belongs to the caller.
Acquiring an absent record can allocate quota-file metadata before admission
or while obtaining a snapshot. Rejected charges preserve usage but can leave
zero-valued disk entries; the filesystem owns their explicit release, writable
context selection and the quota files' protection from ordinary user writes.

The cache is per filesystem and has explicit record release, but no background
shrinker or writeback worker. A filesystem integration must release records
and drive synchronization. Legacy v1 format, quota warnings/netlink delivery,
user-namespace translation, project-ID inheritance, concrete filesystem hooks
and syscall dispatch are not implemented here. Revision-zero disk inode
counts remain 32-bit; an unrepresentable native value fails encoding rather
than being truncated. Structural multi-write atomicity is deliberately owned
by the filesystem's transaction implementation.

## Audit checklist

- Keep filesystem/quota state singular and the backing-file context stable.
- Verify authorization and namespace mapping before invoking administrative APIs.
- Keep block units at the format boundary; account native space in bytes.
- Drain delayed reservations before suspension or disabling.
- Do not use ordinary overwrite retry as structural recovery.
- Keep `QuotaIo` outside recursive charging and caller-held lock reentry.
- Complete explicit synchronization/disable before final owner destruction.

Cached reservation operations never call a provider while the caller holds its
inode reservation lock. Missing active records fail atomically. Ownership
transfer validates both owner sets and all three classes before publishing
candidates, including outstanding reservations in new-owner admission.

A free-entry head replacement is published after its data-block write succeeds.
A failed first write leaves the head unchanged; failures after earlier neighbor
writes still require provider transaction recovery. The no-I/O suspension phase
retains class/file ownership and rejects outstanding reservations; the provider
must drain writes before publishing read-only state. Resume uses that same state
rather than reopening or duplicating quota-file ownership.

Terminal `invalidate` requires a quiesced provider whose writes were synchronized
or irreversibly aborted. It performs no I/O and returns the sole file reference
for unlocked release. On abort, the provider retains on-disk recovery evidence
and reports the writeback error; dropping dirty cache state is not durability.
Other classes retain their records and backing-file ownership.


Ownership transfer clamps an undersized source reservation to zero and emits a
warning. Target admission still includes the full transferred reservation, and
a refusal in any class leaves both owner sets unchanged.

`read_record` loads existing records into the canonical cache without writes.
An absent ID stays inactive until acquisition creates its backing record. This
lets providers distinguish existing payload overwrites from structural growth
before physical-allocation admission, using the existing active state.
