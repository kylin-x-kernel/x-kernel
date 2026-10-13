# drmdevice — Security and reliability

## Analysis scope

This analysis covers the entire crate: `src/lib.rs`, `src/card0.rs`,
`src/drm.rs`, and `src/consts.rs`. No source module is excluded. KVFS typed
user-copy and mmap primitives, `kclass` device-handle validity, and the display
backend contract remain the responsibility of their provider crates. The crate
has no separate test-only source modules.

## Trust model and external boundaries

Every DRM ioctl payload and mmap offset is untrusted user input. User structs
are copied with typed `UserPtr` helpers, ids are resolved in kernel-owned
maps, and array copy-out is bounded by caller-provided capacity. The bound
display handle is trusted kernel state supplied by devfs.

## Unsafe code

The crate contains no unsafe blocks, FFI, or inline assembly. Hardware access
is delegated through the `DisplayDevice` contract. User memory is accessed
through `UserRead`, `UserWrite`, and `UserPtr` abstractions.

## Invariants

- A `DrmCard` operates only on the display handle supplied to its constructor.
- Display-dependent I/O checks handle availability before use.
- User ids are lookup keys and never kernel addresses.
- Copy-out does not exceed the declared array capacity.
- Kernel allocators assign handles, framebuffer ids, blob ids, and mmap
  offsets.
- Retained backing pages outlive user mappings that reference them.
- The vblank event queue remains bounded by `MAX_EVENTS`.

## Thread safety

Mutable object groups have separate mutexes. Atomics allocate monotonic ids and
publish the build-once system blob. Removal is observed through the class
handle's availability state and does not mutate another card's local tables.

## Threat analysis

| ID | Threat | Impact | Trigger | Existing control |
|----|--------|--------|---------|------------------|
| T-01 | Malformed ioctl pointer or count | High — kernel memory corruption or disclosure | Hostile DRM UAPI payload | Typed user copies, bounds checks, and object-table lookup. |
| T-02 | Arithmetic overflow in buffer geometry | Medium — undersized allocation or denial of service | Crafted dimensions or pitches | Creation handlers validate dimensions, formats, and sizes before allocation. |
| T-03 | Mapped buffer freed too early | High — use-after-free and kernel memory corruption | Remove framebuffer while mmap survives | `retained_pages` keeps backing pages alive. |
| T-04 | Open fd targets a newly registered display | High — operations affect the wrong display and corrupt visible device state | Hot-unplug followed by card-index reuse | Each `DrmCard` owns one concrete class handle and returns `ENODEV` after removal. |
| T-05 | Event queue exhausts memory | Medium — unbounded kernel-memory growth and service denial | Repeated event-producing ioctls | Queue length is capped at `MAX_EVENTS`. |

## Failure modes and effects

| ID | Failure mode | Local effect | System effect | Severity | Handling |
|----|--------------|--------------|---------------|----------|----------|
| F-01 | Unknown object id | Ioctl error | Client can recover or exit | 4 | Checked map lookup. |
| F-02 | Removed display | I/O returns `ENODEV`; poll reports error/hangup | Compositor closes the node and rescans | 3 | Availability check on the bound handle. |
| F-03 | Buffer allocation failure | Object creation fails | Client may choose a smaller mode | 3 | Propagate allocation error without partial table insertion. |
| F-04 | Small result array | Required count reported and output truncated | Client reissues with sufficient storage | 4 | Linux two-phase array protocol. |

## Known limitations

- KMS-only: no render node, command submission, or complete DMA-BUF sharing.
- Each card currently exposes one CRTC, connector, and plane.
- Connector hotplug within one display backend is not modeled separately from
  driver-core device removal.

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

Master ownership here is an identity lifecycle, not a complete access-control
implementation: existing authentication and KMS privilege limitations remain.
The synthetic identity is not a validated hardware bus ID and does not provide
real PCI bus-id matching.
