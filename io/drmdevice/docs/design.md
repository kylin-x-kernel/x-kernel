# drmdevice — Design

## Purpose

`drmdevice` implements a minimal Linux DRM/KMS primary device over one
`ClassDevice<DisplayDeviceImpl>`. Each `DrmCard` owns its KMS state and remains
bound to the display handle supplied at construction.

## Responsibilities

- Dispatch the supported DRM ioctl ABI and validate user-provided object ids,
  geometry, formats, arrays, and property values.
- Own legacy and atomic KMS state for one primary card.
- Allocate dumb buffers, framebuffer objects, property blobs, and the backing
  display scanout resources associated with them.
- Queue bounded vblank/page-flip events and implement read and poll readiness.
- Translate KMS presentation requests into the bound `DisplayDevice` contract.

## Scope

```text
io/drmdevice/src/
├── lib.rs      # public DrmCard export
├── card0.rs    # device I/O, ioctl handlers, KMS and buffer state
├── drm.rs      # Linux UAPI wire structures
└── consts.rs   # ioctl, capability, property, and format constants
```

Device-node discovery, card index allocation, and hotplug projection belong to
`devfs`; this crate implements the operations behind one node.

## Architecture

```text
/dev/dri/cardN (created by devfs)
        │ ioctl/read/mmap/poll
        ▼
DrmCard
├── bound display ClassDevice
├── KMS + legacy CRTC state
├── dumb buffers and framebuffer objects
├── property blobs
└── bounded vblank event queue + PollSet
        │
        ▼
DisplayDevice scanout operations
```

The implemented protocol covers legacy libdrm and atomic KMS paths used by
compositors. It is KMS-only: there is no render node or command submission.

The `display` contract crate provides the `DisplayDevice` interface, concrete
display drivers publish implementations through `kclass`, and `kclass`
forwards the contract through `ClassDevice<DisplayDeviceImpl>`. `DrmCard` is
the consumer. The scanout call direction is therefore `DrmCard` -> bound
`ClassDevice<DisplayDeviceImpl>` -> `DisplayDevice` for display information,
resource creation and destruction, and presentation. devfs only selects the
exact published class handle and passes it to `DrmCard::new`.

The public `DeviceFileOps` entries interact with card state as follows:

- `ioctl` dispatches through `DrmIoctl::handle_raw`; typed handlers validate
  user data and read or update the modeset, legacy CRTC, dumb-buffer,
  framebuffer, and blob tables. Atomic property updates use `apply_prop`, and
  successful scanout changes flow through `present_fb` to the bound display.
- `read` removes complete `DrmEventVblank` records from the bounded `events`
  queue and returns `EAGAIN` when no event is ready.
- `mmap` resolves an offset in the `dumbs` table, retains the backing page in
  `retained_pages`, and passes its physical range to `MmapMapper`.
- `poll` reads display availability and event-queue state. `register_poll`
  registers the caller with `poll_rx`; `queue_flip_event` and
  `notify_removed` wake that `PollSet` after an event or removal transition.

## Execution context

All entry points run in process syscall context through `DeviceFileOps`. They
may allocate, copy user memory, and acquire sleepable mutexes. They are not
valid in interrupt context. No task or workqueue is created by this crate.

## Device lifecycle

`DrmCard::new` receives the exact display class handle represented by its
`cardN` node. Every operation that needs scanout calls `display()`, which checks
`ClassDevice::is_available` and returns `ENODEV` after driver-core removal. A
card never falls back to another registered display, so an open fd cannot be
silently redirected when card indices are reused.

`DrmCard::notify_removed` is the public wakeup hook for this lifecycle. devfs
installs it as the card's removal callback and invokes it after removing the
registry record and before releasing the character-device number. The method
wakes `poll_rx`, causing blocked poll waiters to reevaluate `DrmCard::poll`;
the failed display availability check then reports `POLLERR|POLLHUP`.

## Buffer and framebuffer lifecycle

`DRM_IOCTL_MODE_CREATE_DUMB` validates geometry, allocates zeroed contiguous
`GlobalPage` backing, creates the display scanout resource, then publishes a
handle and mmap offset in the dumb-buffer table. `DRM_IOCTL_MODE_DESTROY_DUMB`
removes that table entry and asks the bound display to destroy the associated
scanout resource. A missing handle is already destroyed and succeeds.

`DRM_IOCTL_MODE_ADDFB2` validates one dumb-buffer handle and creates a
framebuffer id that refers to its scanout resource. `DRM_IOCTL_MODE_RMFB`
removes the framebuffer record and clears matching legacy CRTC state. mmap
clones the dumb buffer's `GlobalPage` into `retained_pages` before installing a
physical mapping. Because the mapping interface has no unmap notification,
those clones remain until the `DrmCard` is dropped, allowing a mapping to
outlive later framebuffer or dumb-table removal.

## Concurrency model

Separate mutexes protect modeset state, legacy CRTC state, dumb buffers,
framebuffers, user and system blobs, retained pages, and the event queue. Ids,
offsets, and event sequence numbers use atomics. The IN-FORMATS blob uses an
atomic fast path plus a mutex-protected build-once section. The event queue is
bounded and integrated with `PollSet`.

## Error model

Invalid user pointers, object ids, array capacities, dimensions, formats, and
unsupported ioctls return VFS errors. A removed display returns `ENODEV`;
`poll` reports `POLLERR|POLLHUP`. Dumb-buffer pages remain retained while a
mapping can reference them.

## Design decisions

- Bind every instance to one class handle, allowing independent state for
  multiple `cardN` nodes.
- Keep node naming and numbering in devfs, the shared device-lifecycle owner.
- Use local Linux UAPI structures with `repr(C)` and typed user-copy helpers.
- Share one state model between legacy and atomic paths.
- Retain mapped pages independently of framebuffer table lifetime.
