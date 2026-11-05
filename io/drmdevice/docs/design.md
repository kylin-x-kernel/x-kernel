# drmdevice — Design

## Purpose

`drmdevice` implements a minimal Linux DRM/KMS card over one display backend,
exposed as two minors: the modesetting node (`/dev/dri/cardN`) and the render
node (`/dev/dri/renderD{128+N}`). Both nodes resolve to the same `DrmDevice`;
they differ only in the per-open file state and in the ioctl permission bits.

## Layering

```text
model/            the object model: no VFS, no display driver
├── device.rs     DrmDevice + DrmMinor (the only DeviceFileOps adapter)
├── file.rs       DrmFile: per-open caps, handles, fb list, auth
├── ioctl.rs      IOCTLS table + permission check + argument marshalling
├── backend.rs    trait Backend — the only hardware boundary
├── ids.rs        Id<T> / IdRegistry<T, Tag>
├── event.rs      bounded vblank queue + poll
├── error.rs      DrmError -> errno
└── protocol.rs   VERSION / SET_VERSION / GET_UNIQUE / GET_CAP / SET_CLIENT_CAP

gem.rs            GemObject: backing pages, scanout resource, destructor
framebuffer.rs    Framebuffer objects and the fb ioctls
scanout.rs        ScanoutSlot: what the host is scanning
kms.rs            KmsState, the property table and every KMS ioctl
blob.rs           property blobs
auth.rs           master ownership and magic tokens
abi/              the Linux UAPI: structures, numbers, constants
```

`model` depends on `abi` and on the `Backend` trait, never on `kvfs`, `devfs`
or `kclass` internals. `device.rs` is the single adapter that converts
`DrmError` to `VfsError` and implements `DeviceFileOps`; a subsystem handler
cannot touch a VFS type. Its external caller is devfs: `dynamic.rs` publishes
`DrmDevice::minor(Primary)` on `/dev/dri/cardN` and
`DrmDevice::minor(Render)` on `/dev/dri/renderD{128+N}`, and calls
`DrmDevice::notify_removed` when the device goes away.

## The four single sources of truth

1. **`model/ioctl.rs::IOCTLS`** — every command once: number, name, permission
   bits and handler. Dispatch is a lookup, not a 37-arm match, and a command
   without `RENDER_ALLOW` is unreachable from a render node by default.
2. **`kms.rs::PROPS`** — every property once: id, wire metadata, reader and
   optional writer. `OBJ_GETPROPERTIES`, `GETPROPERTY` and the write path all
   consume this table, so metadata cannot disagree with a writer and a property
   cannot be readable but unwritable.
3. **`ids::IdRegistry`** — every object id comes from the one allocator of its
   class, and the id carries its class (`FbId`, `BlobId`, `DumbHandle`, ...), so
   ids of different tables cannot be mixed up.
4. **`Arc<GemObject>`** — resource lifetime. A handle, a framebuffer and the
   scanout slot each hold a reference; the last one to drop destroys the
   resource. There is no reference counting bookkeeping in the driver.

## Device, minor, file

- `DrmDevice` owns the backend, the object registries, the committed
  `KmsState`, the scanout slot, master ownership and the event queue.
- `DrmMinor` owns an `Arc<DrmDevice>` (the cdev is the last thing to go) and
  remembers which node it is.
- `DrmFile` is created on `open` and owns what Linux puts in `struct drm_file`:
  the node kind, negotiated client capabilities, GEM handles, the framebuffers
  and property blobs it created, and the authentication binding.

A render file is never master and gets a private master object, so `GET_UNIQUE`
answers "no identity" and `SET_VERSION`/`AUTH_MAGIC` cannot touch the card's.

## Framebuffer resource queries

GETRESOURCES enumerates the calling open file description's framebuffer set,
not the device-wide registry. A count-only request allocates no ID buffer.
For a positive capacity, the handler snapshots at most that many IDs while
holding the file-list lock, drops the lock, and copies the prefix through the
checked user-array helper. It always reports the snapshot's full count, even
when the caller's buffer is shorter. A concurrent add/remove can make that
snapshot stale after the lock is released, so clients retain the usual
count/fetch/retry protocol. Ordering is not part of the interface contract.

Independent opens have separate lists; duplicated descriptors share the same
file state. Querying does not change framebuffer ownership or lifetime.

## Commits

`KmsState` is plain data. `DrmDevice::commit_with` validates the requested
state, presents the resulting scanout and only then publishes the state, all
while holding the commit lock (a failed present leaves the state describing what
is actually on screen); legacy `SETCRTC`, `MODE_ATOMIC`, `MODE_SETPROPERTY` and `PAGE_FLIP` all
go through it. `RMFB`/`CLOSEFB` first establish that the id belongs to the
calling file, then unbind the plane under the same lock; a foreign or unknown
id changes nothing. A legacy modeset is therefore visible to atomic clients,
and the state cannot disagree with the screen.

`DIRTYFB` is not a commit: it looks the framebuffer up and goes to
`DrmDevice::flush`, which asks the backend to transfer and flush that
framebuffer's resource without touching the binding. There is no `fb_id == 0`
case: zero and every other unknown id return `ENOENT`, as in Linux.

The scanout slot keeps three separate facts: `bound` (the plane's logical
framebuffer, zero when disabled), `retained` (the resource the host may still be
scanning) and `uncertain` (resources whose present failed, so the host may have
switched to them even though the call reported an error). Unbinding the plane clears `bound` and keeps `retained`, because
this driver has no call that stops the host from scanning; the retained
reference is dropped only after a *new* resource has been successfully
presented. On a failed present the previous contents are restored, so a failure
never frees pixels the host is still scanning.

## Object lifecycle

- `CREATE_DUMB` allocates contiguous zeroed backing as a PageRef, creates the
  scanout resource, and records the object in the calling file's handle table.
  GemObject retains that holder through Arc<PageRef> and stores length_bytes for
  its requested page-aligned pixel range. Exact allocation returns unused backing
  before publication. mmap and host resource lengths keep using length_bytes.
- Handles are per open file (`file_priv->object_idr`): `DESTROY_DUMB`,
  `GEM_CLOSE`, `MAP_DUMB` and `mmap` only accept the file's own handles, and
  `release()` drops whatever is left.
- `ADDFB2` records the framebuffer in the calling file's set and adds a
  reference to its GEM object. `RMFB`/`CLOSEFB` only accept an id in that set
  (a foreign id is `ENOENT`, as in Linux); removal unbinds the plane under the
  commit lock, while `CLOSEFB` refuses a framebuffer that is bound or on screen.
  Closing the file destroys the framebuffers it created (`drm_fb_release()`)
  instead of leaving them in the registry.
- `mmap` resolves the offset through the file's handle table and retains the
  backing pages, because the mapping interface has no unmap notification.
- PRIME is a handle round-trip stub; `DRM_CAP_PRIME` stays `0` until a real
  dma-buf implementation exists.
- Client property blobs are owned by the creating file and the id registry.
  `DESTROY_BLOB` accepts only an id in the calling file's set: an existing id
  owned by another file, or a driver-owned system blob, is `EPERM`, while an
  unknown id is `ENOENT`. `release()` removes the file's whole set under one
  commit section; the `MODE_ID` blob a legacy `SETCRTC` creates is additionally
  recorded on the device so the next modeset (or a CRTC disable) retires it
  instead of leaking one blob per modeset. `GETPROPBLOB` always reports the
  required length; it copies only when the caller's length is exactly the blob
  length, and treats a short or oversized buffer as a successful size probe.
  Writing an attached immutable property (`type`, `IN_FORMATS`) is `EINVAL`.
  The atomic path keeps `ENOENT` for an unknown object, an unknown property or a
  property attached to another object class; the single-property ioctls keep
  Linux's `EINVAL` initializer for a property that is not attached to the
  addressed object, while an unknown object is still `ENOENT`.

## Connector dimensions

`GETCONNECTOR` reports `mm_width` and `mm_height` as zero (unknown). The
display contract supplies only visible pixel dimensions, with no physical
size or EDID. Those pixels still determine modes and resource limits, but
cannot be reused as millimetres or converted using an assumed DPI. A future
physical-size provider must pass measured dimensions through that contract
before this query can report nonzero values.

The pixel size is a host property that can change while the card is open
(a resized QEMU window). The connector query therefore asks the backend to
refresh first and derives the mode and the reported geometry from that one
snapshot, so a resize cannot make `hdisplay`/`vdisplay` disagree with the
mode name. Refreshing is a read: it never touches `KmsState`, the scanout
slot or the legacy mode blob, so a running client keeps presenting its old
framebuffer until it sets a mode itself.

`GETRESOURCES` reports the size *envelope* (`MIN_SCANOUT_*`/`MAX_SCANOUT_*`),
not the current size. A KMS client fixes its screen range from these bounds
at startup and cannot grow past them later; reporting the boot size as the
maximum would make every larger host size unreachable. `GETCRTC` keeps
reporting the committed mode, which is what the card is actually scanning.

A refresh that observes a change also calls the hotplug sink installed with
[`DrmDevice::set_hotplug_sink`]. The card does not know what a sink does: the
filesystem that owns the device node installs one that publishes a Linux-style
`change` uevent with `HOTPLUG=1` on `drm/cardN`, which is the object udev and
the compositor watch. A client that is not listening can still re-read the
connector, for example through a RandR re-probe. The sink runs in the querying
syscall context and therefore must not block.

## Concurrency

Two locks with a fixed order: `commit` before the state/registry locks. The
presenter holds the commit lock across "present, then publish", so a failed
present never leaves the committed state describing something else.
Per-file locks (handles, fbs, blobs, auth) never nest with each other. The
per-file blob set is also taken under `commit` by `DESTROY_BLOB` and release
(`destroy_client_blob_locked` / `destroy_file_blobs`); creation takes it without
`commit`, after the device registry lock. The master lock is never taken while
holding a file lock except in the documented order in `open`/`release`.

## Execution context

All entry points run in process syscall context through `DeviceFileOps`. They
may allocate, copy user memory, and acquire sleepable mutexes. No task or
workqueue is created by this crate.

## Error model

Handlers and the object model return `DrmError`; `device.rs` maps it to the
errno the syscall layer expects (`EINVAL`, `EOPNOTSUPP`, `ENODEV`, `EBUSY`,
`EACCES`, `EPERM`, `ENOENT`, `ENOMEM`, `EIO`, `EFAULT`, `EAGAIN`). Unknown
command numbers are `EOPNOTSUPP`.

## Known limitations

- No command submission (execbuf/virgl) and no dma-buf: PRIME is a stub and
  `DRM_CAP_PRIME` is `0`.
- One CRTC, connector, plane and encoder.
- `DRM_AUTH` is not enforced per command; `MASTER` and the render allowlist are,
  and mastership is answered by the device, not by per-file state.
- Page-flip events live in one card-wide queue, as this driver has always done;
  Linux queues them per file. With one master per card this is not observable in
  practice, but a second descriptor could read events it did not request.
- A legacy `PAGE_FLIP` on a disabled CRTC is rejected with `EINVAL`; it only
  replaces the framebuffer of an active CRTC and never enables a disabled pipe.
- A failed present cannot prove whether the host switched scanout, so every
  attempted resource is retained until a later present succeeds. A master that
  keeps issuing failing modesets can therefore retain one object per failure
  (including its pages); this is the memory cost of never freeing pixels the
  host may still scan, and it ends at the first successful present.
- The portable unit tests cover the permission matrix, the capability table, the
  mode-blob encoding and the FakeBackend invariants; the rest is covered by the
  on-target `abi-tests/`.
