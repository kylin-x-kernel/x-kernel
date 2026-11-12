# drmdevice — Security and reliability

## Analysis scope

This analysis covers the entire crate: `src/lib.rs`, `src/abi/`, `src/model/`,
`src/ioctl.rs`, `src/protocol.rs`, `src/sync/`, `src/syncobj/`, `src/kms/`,
`src/gem.rs`, `src/framebuffer.rs`, `src/blob.rs`, and `src/auth.rs`. No
source module is excluded. KVFS typed
user-copy and mmap primitives, `kclass` device-handle validity, and the display
backend contract remain the responsibility of their provider crates:
`model::backend::ClassBackend` is the only code here that calls the display
contract, and `model::device::minor::DrmMinor` is the only code that implements
`DeviceFileOps` for devfs.

## Trust model and external boundaries

Every DRM ioctl payload and mmap offset is untrusted user input. User structs
are copied with typed `UserPtr` helpers, ids are resolved in kernel-owned
maps, and array copy-out is bounded by caller-provided capacity. The bound
display handle is trusted kernel state supplied by devfs.

## Unsafe code

The crate has three unsafe sites, none of which touches memory the resource
model does not already own. Hardware access is delegated through the
`DisplayDevice` contract and user memory is accessed through `UserRead`,
`UserWrite` and `UserPtr`.

| site | invariant | enforced by |
|---|---|---|
| `model/device.rs::DrmDevice::finish_flip` → `take_reservation` | The lifetime-erased `EventReservation<'static>` only outlives its borrow of the card's event queue for as long as the card lives. | The card owns the event queue and the flip queue; every reservation is published or dropped by the card, and the card cannot be released while an open file exists. |
| `sync/wait.rs::Expiry::arm` → `Timer::start` | The timer runs on an initialized CPU from sleepable syscall context, and its handler only takes a waker. | Every fence wait is entered from an ioctl or a descriptor read; the handler does one `SpinNoIrq` take and one `Waker::wake`. |
| `ut-tests` (test builds only) | A `PageRef`-owned page stays mapped while the test reads it. | The test keeps the owner alive for the read; excluded from production builds. |

A fence's callback list is an IRQ-safe spinlock: a device or a timer may
complete a fence from interrupt context, so every callback is written to be
short, non-blocking and free of sleepable locks.

## Invariants

- One `DrmDevice` serves both minors of one card; a `DrmMinor` owns the device
  (the character device is the last reference) and never selects a
  process-global display.
- Display-dependent I/O checks handle availability before use.
- User ids are lookup keys and never kernel addresses.
- Copy-out does not exceed the declared array capacity.
- Array-copy counts are bounded before the corresponding array is loaded: an
  atomic request naming more than `MAX_ATOMIC_OBJECTS` objects, or whose
  per-object property counts overflow or exceed `MAX_ATOMIC_PROPERTIES` in
  total, is refused with `ENOMEM` before the property and value arrays are
  fetched. The count arithmetic uses `checked_add`, so a hostile count cannot
  wrap into a small allocation.
- Blob copy-out always writes the required length back. Bytes are copied only
  when the supplied length exactly matches the blob; a short or oversized buffer
  is a successful size probe that copies nothing, while an unknown id is
  `ENOENT`, matching Linux's `MODE_GETPROPBLOB`.
- `mmap` resolves the file offset through the per-file handle table. An offset
  that no live handle owns is `EINVAL`, so it cannot select another file's
  object or an unrelated physical range. The resolved GEM object supplies the
  physical base and page-aligned backing length directly; the file offset is
  never added to or subtracted from that address, so there is no offset
  arithmetic to underflow or overflow. A request longer than the backing is
  refused with `EINVAL` rather than shortened. The PageRef supplies the
  physical base and retained lifetime; `alloc_zeroed_pages_exact_ref`
  initializes all backing before any host/user publication.
- Kernel allocators assign framebuffer ids, blob ids, resource ids and mmap
  offsets through one `IdRegistry` per class; dumb-buffer handles are allocated
  per file and belong to the file that created them (`file_priv->object_idr`),
  so a handle is only usable through the descriptor that created it and is
  dropped when that file closes.
- A GEM object is kept alive by `Arc` references from its handles, its
  framebuffer and the scanout slot; the resource is destroyed exactly when the
  last reference goes away. Unbinding a framebuffer (RMFB) clears the plane
  binding but retains the scanned resource until the next successful present.
- A framebuffer id is usable for `RMFB`/`CLOSEFB` only by the file that created
  it; a foreign or unknown id is `ENOENT` and changes neither the registry nor
  the committed scanout. `DIRTYFB` remains a device-wide lookup, as in Linux.
- Mastership is asked of the device (`DrmDevice::is_master`); a per-file flag
  only records which master object the file may reacquire and never grants
  permissions.
- A dirty flush (`DIRTYFB`) transfers pixels and never changes the scanout
  binding; only a commit (`SETCRTC`, `ATOMIC`, property writes, page flips)
  rebinds it.
- Blobs created by the driver for a legacy modeset are owned by the driver and
  retired by the next modeset or by disabling the CRTC.
- Retained backing pages outlive user mappings that reference them.
- The vblank event queue remains bounded by `MAX_EVENTS`.  A page flip
  reserves its completion slot before presenting, so a full queue fails with
  `ENOMEM` instead of reporting success after dropping the event.
- Every fence is created pending with a unique `(context, seqno)` pair and
  reaches a terminal state at most once; a completed fence never changes state
  or error again, so a waiter that reads it twice reads the same answer.
- A fence's completion capability (`FenceSignal`) cannot be reconstructed or
  copied from a fence. The party that owns a completion reports its loss
  explicitly (a failed commit fails its out-fence), so an out-fence descriptor
  handed to userspace always becomes readable; a fence whose *callers* are all
  gone is released with them.
- A merge of one fence is that fence (no wrapper), so a client cannot smuggle a
  second identity into a single-fence dependency.
- `SYNCOBJ_*` requests validate every flag before touching an object and copy
  every handle array (bounded by `DRM_SYNCOBJ_MAX_HANDLES`) before the first
  wait, so a malformed request cannot leave a partially applied transfer or a
  task parked on an unknown handle.
- `IN_FENCE_FD` and `SYNC_IOC_*` resolve descriptors through the calling
  process's table and reject anything that is not one of this driver's
  sync_files or dma-bufs with `EINVAL`; no fence fd from another subsystem is
  accepted, and a foreign fd is never dereferenced.
- `OUT_FENCE_PTR` and `out_fence_ptr` are userspace addresses written exactly
  once, with a single `s32`, after the commit succeeded; a dry run, a failed
  commit and an unmapped address (`EFAULT`) never leak a descriptor number.
- A fence records an error only when it is completed; a pending fence that was
  released while carrying an error cannot make a waiter report success.
- `DMA_BUF_IOCTL_SYNC` validates its direction bits before touching the
  reservation, and the wait it performs is interruptible.
- A dma-buf has one import per card and one export per object; a repeated
  import resolves to the existing object or the file's existing handle, so a
  client cannot make one backing hold two mapping offsets or leave an import
  attached that no object owns.
- DRM magic tokens are scoped to one master, consumed once, and removed when
  their file closes or changes master.
- AUTH_MAGIC requires the active master and reads its argument without copying
  data back to user memory.
- User-copy helpers enforce the matching ioctl direction bits
  at compile time, preventing a mismatched dispatcher arm from being built.

## Display hotplug

A refresh that observes a host size change calls the sink installed by the node
owner. The call happens after the display driver's internal lock is released
and outside the device locks, so the sink may publish to userspace. It runs in
syscall context and inherits that constraint: no blocking, no unbounded work.
The sink cannot change what is scanned — only a later `SETCRTC`/page flip does.
A card without an installed sink simply reports the new size to the next
querying client.

## Connector size metadata

The caller's incoming physical dimensions are overwritten on a successful
`GETCONNECTOR`. Pixel dimensions from the display backend are never exposed
as millimetres: without a physical-size source, both fields report unknown
(zero). This avoids publishing fabricated DPI metadata and requires no
geometry conversion or arithmetic on caller-supplied dimensions. Existing
connector-id validation and typed user-copy failure paths are unchanged.

## Thread safety

Mutable object groups have separate mutexes. Atomics allocate monotonic ids and
publish the build-once system blob. Removal is observed through the class
handle's availability state and does not mutate another card's local tables.

The fence layer adds IRQ-safe spinlocks: a fence's callback list (completion
may come from an interrupt handler) and a syncobj's value and waiter list.
Callbacks run after the fence's lock is released and never take a sleepable
lock; a syncobj registers the chain callback for a replaced value only after
releasing its own locks, so the fence lock is always taken before the syncobj
lock and never after it. A task wait parks outside all of them.

## Threat analysis

| ID | Threat | Impact | Trigger | Existing control |
|----|--------|--------|---------|------------------|
| T-01 | Malformed ioctl pointer or count | High — kernel memory corruption or disclosure | Hostile DRM UAPI payload | Typed user copies, bounds checks, and object-table lookup. |
| T-02 | Arithmetic overflow in buffer geometry | Medium — undersized allocation or denial of service | Crafted dimensions or pitches | Creation handlers validate dimensions, formats, and sizes before allocation. |
| T-03 | Mapped buffer freed too early | High — use-after-free and kernel memory corruption | Remove framebuffer while mmap survives | `retained_pages` keeps backing pages alive. |
| T-04 | Open fd targets a newly registered display | High — operations affect the wrong display and corrupt visible device state | Hot-unplug followed by card-index reuse | Each `DrmDevice` owns one concrete class handle and returns `ENODEV` after removal; both minors of the card share it. |
| T-05 | Event queue exhausts memory | Medium — unbounded kernel-memory growth and service denial | Repeated event-producing ioctls | Queue length is capped at `MAX_EVENTS`. |
| T-06 | Reuse or cross-master use of a DRM magic token | Medium — unauthorized client authentication | Forged, replayed, or stale AUTH_MAGIC request | Tokens live in one master registry, require its active owner, are single-use, and are removed on file close or master change. |
| T-07 | A fence waiter never wakes (lost completion) | Medium — a client task hangs in `SYNCOBJ_WAIT` or a read | Work that stops reporting its completion | Only the owner of a completion can lose it, and every such owner reports the loss explicitly (a failed commit fails its out-fence, a dropped signal does not complete a fence); a fence replaced in a syncobj pokes its waiters. Residual risk: an unbounded wait on work that never reports is parked forever, as in Linux, which is why only the infinite `SYNCOBJ_WAIT` timeout is accepted. |
| T-08 | Hostile syncobj counts or handles | Medium — unbounded work or partial state | Crafted `count_handles` or unknown handles | The count is refused above `DRM_SYNCOBJ_MAX_HANDLES` before the array is read, and every handle is resolved before the first side effect. |
| T-09 | A fence descriptor from another subsystem is passed as `IN_FENCE_FD` or to `FD_TO_HANDLE` | Medium — a foreign file is dereferenced or a fence is forged | Crafted fd number | Fds are resolved through the calling task's table and must be one of this driver's sync_files (or dma-bufs); anything else is `EINVAL`. |
| T-10 | A failed commit leaves its out-fence pending | Low — a client blocks on a descriptor that never becomes readable | Write fault, event queue full, an invalid final state | `Flip` fails the fence with the ioctl's errno on every failure path, including a caller that drops it. |

## Failure modes and effects

| ID | Failure mode | Local effect | System effect | Severity | Handling |
|----|--------------|--------------|---------------|----------|----------|
| F-01 | Unknown object id | Ioctl error | Client can recover or exit | 4 | Checked map lookup. |
| F-02 | Removed display | I/O returns `ENODEV`; poll reports error/hangup | Compositor closes the node and rescans | 3 | Availability check on the bound handle. |
| F-03 | Buffer allocation failure | Object creation fails | Client may choose a smaller mode | 3 | Propagate allocation error without partial table insertion. |
| F-04 | Small result array | Required count reported and output truncated | Client reissues with sufficient storage | 4 | Linux two-phase array protocol. |

## Known limitations

- No command submission (execbuf/virgl). Fences express display and reservation
  completion only.
- `SYNCOBJ_EVENTFD` is `EOPNOTSUPP`; `DRM_TIME_ABSTIME` waits are rejected.
  Neither reports success for work it does not do.
- PRIME crosses this card only: an exported dma-buf is this driver's guest
  memory plus its reservation. A resource the guest cannot read (a host-only
  BAR) is not exported, and another subsystem's dma-buf is not imported.
- The render node (`/dev/dri/renderD*`) is restricted to the commands marked
  `DRM_RENDER_ALLOW` in the ioctl table; everything else is `EACCES`. It is
  never master and gets a private master, so identity and authentication ioctls
  cannot reach the card's.
- `MASTER` commands are enforced from the same table; `DRM_AUTH` is not
  enforced per command, so the magic handshake remains a compatibility
  protocol rather than an authorization gate.
- A failed present cannot prove the host stopped scanning the attempted
  resource, so the previous and attempted resources are retained until the next
  successful present. A master that keeps issuing failing modesets can therefore
  retain one additional GEM object per failure; the memory is reclaimed at the
  first successful present. This unbounded-until-success retention is accepted
  because freeing a resource the host may still scan would corrupt the display.
- Each card currently exposes one CRTC, connector, plane and encoder.
- Connector hotplug is reported as a `change` uevent on the card, not through a
  connector object: the driver projects one connector and has no per-connector
  kobject. A hotplug only tells listeners to re-read; it never mutates the
  committed KMS state, so a listener that ignores it keeps scanning the frame
  it already set.

## Permission model

The ioctl table is the complete, auditable list of command-level admission
decisions:

- a render file may issue only entries carrying `RENDER_ALLOW` (`VERSION`,
  `GET_CAP`, `GEM_CLOSE`, the two PRIME translations);
- entries carrying `MASTER` require the calling file to own the card;
- everything else is available to any open file, as in Linux.

`DESTROY_BLOB` additionally enforces object-level ownership that cannot be
expressed as a table flag because it depends on the named id: the id must be
present in the calling file's blob set. An id owned by another file, or a
driver-owned system blob, is `EPERM`; an unknown id is `ENOENT`. The check runs
after table admission and before the legacy slot or device registry is touched,
so a refused destroy leaves the owner's registration intact. Release destroys
only the snapshot of the closing file's own set.

Command-level denials are `EACCES` and are logged at debug level with the
command name. The matrix is covered by `ut-tests/`: every entry is checked
against both node kinds and both ownership states, so adding a command without
deciding its permission fails the test. Blob ownership is covered by
`destroy_client_blob_requires_ownership_and_refuses_system_blobs`.

## GETRESOURCES framebuffer enumeration

Framebuffer IDs come only from the caller's file-owned set. A global registry
walk would expose other files' objects and must not replace this selection.
The temporary vector is fallibly reserved for min(capacity, owned count);
untrusted capacity cannot by itself cause an oversized allocation. Zero
capacity, or an empty set, performs no ID-buffer access.

User copies run after dropping the framebuffer-list lock, preventing a fault
from retaining that object lock. Copy errors propagate as EFAULT and allocation
failure as ENOMEM; neither path modifies the framebuffer set. Concurrent
removal can invalidate a returned ID after the snapshot, as with other DRM
resource discovery; the snapshot does not grant ownership of that object.

## Audit checklist

- Check display availability on every new hardware-facing path.
- Never select a process-global display from a per-card operation.
- Validate all user counts and geometry before allocation or copy-out.
- Keep mapped backing memory alive independently of object-table removal.

## Identity copy bounds

VERSION bounds each copy by the original user capacity before reporting the
full length, and skips null or zero-capacity destinations. GET_UNIQUE requires
room for the entire identity. Neither writes a terminator beyond the declared
buffer; nonempty invalid destinations retain `EFAULT` through the existing
user-copy helpers. An unset master identity reports zero length without
accessing the destination. VERSION and GET_UNIQUE cannot activate an identity.

## Master identity ownership

The card's shared master-state mutex serializes open, acquisition, relinquish,
and final release. Lock order is card master state, file binding, then master
identity. GET_UNIQUE snapshots the string and releases those locks before
copying to user memory. File-private state retains its master after relinquish;
a new master cannot inherit that old identity. The active owner is a weak file
reference, avoiding a card/file reference cycle. Final private-state destruction
clears matching ownership even when an open file was never published.

SET_VERSION rejects unsupported requests and reports the supported versions.
Under the card master-state lock, it verifies that the active owner's weak
reference matches the calling file before acquiring the binding lock or changing
the identity. Non-owner requests, including queries, receive `EACCES` and cannot
modify the shared identity; retaining a former master binding grants no access.
Interface negotiation may establish an identity before a later driver-version
error, following Linux's operation order. Identity presence is represented by
the master-owned optional string, not an independent activation flag.

The legacy GET_MAGIC/AUTH_MAGIC handshake is implemented with
master-scoped, single-use tokens. GET_MAGIC rejects a file bound to an inactive
or replaced master; tokens are cleared when a master stops being active. The
handshake does not update an authorization state consumed by the ioctl
dispatcher; tokens are compatibility identifiers, not cryptographic
credentials. The dispatcher does not yet enforce per-ioctl DRM_AUTH flags, and
complete KMS authorization remains outside this change.
The synthetic identity is not a validated hardware bus ID and does not provide
real PCI bus-id matching.


The `drm_mmap_retains_exact_backing_after_handle_and_object_destruction` unit test
calls the production minor's `mmap` callback with an isolated pseudo file and a
recording mapper. It holds only a weak reference to the backing while destroying
the GEM handle and object, then checks that the mapping retained the exact
three-page range. It also checks that destroying the remaining file/device
owners releases that backing. The mapper records the physical range without
installing hardware translations, so the test does not establish TLB retirement
or real userspace unmap behavior.
