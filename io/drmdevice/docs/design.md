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

## Identity ioctl buffer contracts

`DRM_IOCTL_VERSION` supports length-only queries with null pointers or zero
capacities, and reports complete lengths even when only a prefix fits.
`DRM_IOCTL_GET_UNIQUE` reports the calling file's master identity and copies
only when the complete identity fits. An unset identity reports length zero
without touching its destination, preserving libdrm's open-by-driver-name
discovery. Neither query establishes an identity.

Each open file description has typed `DrmFile` private data. The first open
creates and owns a master; subsequent opens share that master. Duplicated file
descriptors share the same binding. `SET_MASTER` rejects another active owner
with `EBUSY`; a former owner reuses its master, whereas a client becoming owner
creates a new master with an unset identity. `DROP_MASTER` and final owner close
release active ownership without clearing identities held by older files.
Private-state destruction also rolls back ownership for an unpublished open.
The ioctl dispatcher resolves `DrmFile` once and passes its shared reference to
the typed handlers.
All raw helpers assert their command direction at
compile time, so selecting a helper inconsistent with the ioctl encoding fails
the build. Input-only commands, including SET_CLIENT_CAP and GEM_CLOSE, never
write their request structures back. Bidirectional commands retain the
read-and-write helper.

`SET_VERSION` requires the calling file to be the current active owner, including
for query-only requests. Other files receive `EACCES` without changing the shared
master identity. A former owner must reacquire ownership before calling it.
`SET_VERSION` validates the requested interface against 1.4 and driver versions
against the advertised constants, treating major `-1` as a query. A valid
interface request for 1.1 through 1.4 establishes the calling master's identity;
query-only and 1.0 requests do not. As in Linux, interface processing precedes
driver-version validation. Responses report the highest supported interface and
driver versions, including for lower-version requests and validation errors.
The established string remains the synthetic driver label; real PCI bus-id
matching, leases, and complete KMS master authorization are not implemented by
this change.

## Legacy DRM authentication

DRM_IOCTL_GET_MAGIC allocates one nonzero token per open file description in
its bound master's registry and returns it through an output-only user buffer.
Repeated requests return the same token while it remains registered. The
active master may consume a client's token once with input-only
DRM_IOCTL_AUTH_MAGIC; a later GET_MAGIC allocates a fresh token. Unknown,
expired, and reused tokens fail. Tokens are removed when their file closes,
moves to a new master, or its master stops being active. GET_MAGIC rejects
files whose bound master is no longer active. Authentication is master-scoped
and is serialized with master ownership changes. This implements the legacy
token handshake; it does not add per-ioctl DRM_AUTH permission checks or
complete KMS authorization.
