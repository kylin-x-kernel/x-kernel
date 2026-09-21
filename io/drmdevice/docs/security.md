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
