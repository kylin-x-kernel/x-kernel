# drmdevice — Design

## Purpose

`drmdevice` implements a minimal Linux DRM/KMS card over one display backend,
exposed as two minors: the modesetting node (`/dev/dri/cardN`) and the render
node (`/dev/dri/renderD{128+N}`). Both nodes resolve to the same `DrmDevice`;
they differ only in the per-open file state and in the ioctl permission bits.

The card also carries the kernel-side synchronization objects a modern client
needs (G2 of the graphics alignment plan): dma-fences, dma-resv
implicit ordering, `sync_file`, the `SYNCOBJ_*` family and a real dma-buf
export/import path. They are described in *Synchronization* below.

## Layering

```text
model/            the card object: no VFS, no display driver
├── device/       DrmDevice: struct + card-core methods
│   ├── device.rs struct, construction, device-wide accessors
│   ├── commit.rs committed KMS state: commit / validate / flush / present
│   ├── events.rs event-queue front: flip events, read, poll
│   └── minor.rs  DrmMinor (the only DeviceFileOps adapter)
├── file.rs       DrmFile: per-open caps, handles, fb list, auth
├── scanout.rs    ScanoutSlot: what the host is scanning
├── backend.rs    trait Backend — the only hardware boundary
├── ids.rs        Id<T> / IdRegistry<T, Tag>
├── event.rs      bounded vblank queue + poll
└── error.rs      DrmError -> errno

ioctl.rs          IOCTLS table + permission check + argument marshalling
protocol.rs       VERSION / SET_VERSION / GET_UNIQUE / GET_CAP / SET_CLIENT_CAP
gem.rs            GemObject: backing pages, scanout resource, destructor, and
                  the dumb-buffer / GEM_CLOSE / PRIME ioctls
framebuffer.rs    Framebuffer registry and the fb ioctls
kms/              the committed KMS state and every KMS ioctl
├── state.rs        KmsState, property application, mode-blob codec
├── props.rs        PROPS + the property ioctls
├── modeset.rs      queries / SETCRTC / PAGE_FLIP / ATOMIC
├── cursor.rs       CURSOR / CURSOR2
└── vblank.rs       WAIT_VBLANK
blob.rs           property blobs: registry, legacy-mode slot, blob ioctls
auth.rs           master ownership, magic tokens and the auth ioctls
abi/              the Linux UAPI: structures, numbers, constants
```

`model` depends on `abi` and on the `Backend` trait, never on `kvfs`, `devfs`
or `kclass` internals. `device/minor.rs` is the single adapter that converts
`DrmError` to `VfsError` and implements `DeviceFileOps`; a subsystem handler
cannot touch a VFS type. Its external caller is devfs: `dynamic.rs` publishes
`DrmDevice::minor(Primary)` on `/dev/dri/cardN` and
`DrmDevice::minor(Render)` on `/dev/dri/renderD{128+N}`, and calls
`DrmDevice::notify_removed` when the device goes away.

## The four single sources of truth

1. **`ioctl.rs::IOCTLS`** — every command once: number, name, permission
   bits and handler. The name itself is declared on the wire struct's `Ioctl`
   impl, next to its handler, with the full `DRM_IOCTL_*` name in its doc; the
   table lists commands in number order. Dispatch is a lookup, not a 37-arm
   match, and a command without `RENDER_ALLOW` is unreachable from a render
   node by default.
2. **`kms/props.rs::PROPS`** — every property once: id, wire metadata, reader and
   optional writer. `OBJ_GETPROPERTIES`, `GETPROPERTY` and the write path all
   consume this table, so metadata cannot disagree with a writer and a property
   cannot be readable but unwritable.
3. **`ids::IdRegistry`** — every object id comes from the one allocator of its
   class, and the id carries its class (`FbId`, `BlobId`, `DumbHandle`, ...), so
   ids of different tables cannot be mixed up.
4. **`Arc<GemObject>`** — resource lifetime. A handle, a framebuffer and the
   scanout slot each hold a reference; the last one to drop destroys the
   resource. There is no reference counting bookkeeping in the driver.
5. **`sync::dma_fence::DmaFence`** — completion. Every observable completion
   userspace can wait for (a flip reaching the display, a commit that failed,
   a buffer that is still written) is a fence, and every wait goes through
   `sync::wait`; no module polls a completion by hand.

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
framebuffer, zero when disabled), `scanning` (the framebuffer of the resource
the host was last asked to display, which outlives `bound`) and `uncertain`
(resources whose present failed, so the host may have switched to them even
though the call reported an error). Unbinding the plane clears `bound` and keeps
`scanning`, because this driver has no call that stops the host from scanning;
the scanned resource is dropped only after a *new* resource has been
successfully presented. On a failed present the previous contents are restored,
so a failure never frees pixels the host is still scanning. Keeping the binding
separate from the scanned resource is what lets a removal, a file close and a
failed rollback each describe the same screen truthfully.

### Fences on the commit path

A commit userspace can wait for allocates its completion point **before** it
touches the scanout (`model/flip.rs::OutFence`): the page-flip event slot and the
out-fence. The whole `Flip` is then handed to `DrmDevice::commit_flip`, which is
the only place a flip is completed:

- success — a card with a blank timeline queues the flip and completes event
  and fence together on the blank that scans it out; a card without one
  completes both with the commit (`drm_crtc_send_vblank_event` on a device
  without vblank);
- failure — `Flip::fail` completes the out-fence with the ioctl's errno and the
  event slot is released unused;
- a flip dropped without being completed (a caller that gave up) also fails the
  out-fence, so no descriptor this driver handed out is ever left pending.

`MODE_ATOMIC` and `MODE_PAGE_FLIP` differ in how the descriptor reaches the
client, not in what it means: the atomic path names a `u64` address in
`OUT_FENCE_PTR`, the legacy request may carry `out_fence_ptr` in its larger wire
structure. Both write the number only after the commit accepted the flip; a
failed commit leaves `-1` in the caller's word, `TEST_ONLY` always writes `-1`,
and an unwritable address is `EFAULT` exactly as `put_user` reports it in
`drm_atomic_uapi.c`.

`DRM_MODE_ATOMIC_NONBLOCK` selects the completion discipline. Without it the
ioctl waits for the flip's completion point (the same fence `OUT_FENCE_PTR`
names) before returning, as `drm_atomic_helper_commit_tail` ends in a vblank
wait; with it the ioctl returns once the flip is queued and the client observes
completion through `PAGE_FLIP_EVENT` / `OUT_FENCE_PTR`. Both paths keep the
in-fence and reservation waits and the present synchronous (Linux keeps those
waits synchronous under the flag too). A signal during the blocking wait does
not undo a commit that already programmed the scanout.

Before the commit locks anything it waits for two fence sets:

- the explicit `IN_FENCE_FD` of the plane, resolved to a fence when the property
  is applied and validated there (`EINVAL` for a descriptor that is not a
  sync_file), so a client may close the descriptor immediately;
- the framebuffer's reservation with `DMA_RESV_USAGE_READ`, which is
  `drm_gem_plane_helper_prepare_fb`'s rule: scanning the buffer out reads the
  memory, so it must wait for every writer. Linux gives explicit fencing
  precedence over implicit fencing, and so does this driver: an `IN_FENCE_FD`
  replaces, rather than adds to, the reservation's writers.

The wait is a task-context wait (`sync/wait.rs`), never a spin, and happens
before the commit lock is taken, so no sleepable operation runs under it.

A committed flip also records itself as the framebuffer's exclusive work
(`DrmDevice::reserve_display` writes the same fence into the framebuffer's
reservation), which is what makes a client that fills the buffer wait for the
display before it overwrites the pixels.

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
- `PRIME_HANDLE_TO_FD` exports the object's backing as a real dma-buf
  descriptor (`model/dma_buf.rs`): it carries the object's pages and its
  reservation, so the descriptor keeps the memory alive after the handle is
  closed. `PRIME_FD_TO_HANDLE` creates this card's object for an imported
  dma-buf over the *same* pages and the *same* reservation (the memory is guest
  memory and both names describe one bus master's view), records an attachment
  that the object owns, and returns a new handle. A descriptor that is not one
  of this driver's dma-bufs is `EINVAL`.

  One card has one import per dma-buf (`drm_gem_prime_import`'s attachment
  lookup): the second import of the same buffer — through another descriptor
  or through a re-export of it — resolves to the object the file already
  holds, so a backing is never addressable under two handles in one file, and
  `DESTROY_DUMB` refuses an imported handle (forgetting its mapping offset
  would break a later import). An attachment is released with the object that
  owns it, which is the detach. `DMA_BUF_IOCTL_SYNC` waits for the
  reservation before a CPU start, `DMA_BUF_IOCTL_INFO` reports the size, and
  `DMA_BUF_IOCTL_EXPORT/IMPORT_SYNC_FILE` move the reservation's fence through a
  sync_file. `DRM_CAP_PRIME` reports import and export only because both are
  implemented.
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

## Synchronization

The fence layer (`src/sync`) is written against Linux's dma-fence sources, one
module per source file. It is the only way this card expresses completion, and
every ioctl that userspace can wait on builds on it.

### Fences

A `DmaFence` starts pending, reaches a terminal state exactly once, keeps the
error it finished with, and records its completion instant. Waiting is done
through Linux's callback list: a callback either tells a collection that it is
one step closer to complete (`MemberDone` for a fence array, `ChainPremise` and
`ChainWork` for a chain node), signals an eventfd relation, or wakes a waiter
(`Wake`, `Wakeable` for a poll source). Callbacks run after the fence's lock is
released, so a chain may complete a fence that refers back to its premise.

Completion capabilities are lock-free: `DmaFence::new_owned` returns the fence
and the single `FenceSignal` that completes it. Dropping that capability does
**not** complete the fence, because a drop cannot distinguish "the work was
lost" from "the capability was released together with the fence itself", and a
fence that completes during its own last release would run callbacks against an
object whose owners are being torn down. Who owns the capability is therefore
explicit: a commit's out-fence is owned by its `Flip`, which fails it on any
failure path; a fence array keeps its head's capability for the array's lifetime.

The card creates fences in four namespaces: anonymous (out-fences and implicit
work, `new_anonymous_fence`), a private one for already signaled fences,
`MERGE_CONTEXT` for fence arrays built by `sync_file` merging, and the driver
context reported in `sync_file_info`. Every fence has a unique
`(context, seqno)` pair.

### Reservations and arrays

`DmaResv` holds the exclusive and shared fences of one backing. `set_write_fence`
replaces the exclusive set, `add_read_fence` extends the shared one, and
`snapshot(usage)` returns the set a consumer with that usage must wait for. The
lists are immutable (`FenceList`) and published by pointer replacement, so a
reader always gets a complete set and a writer never mutates a set a consumer may
still be waiting on.

`FenceArray` merges a set into one fence. The head completes when every member
did and reports the first member failure; it never completes early, because this
driver's arrays only merge client fences and display dependencies, where an early
completion would expose memory another member still uses. An empty or
single-fence set is not wrapped (`dma_fence_unwrap`).

### sync_file and syncobj

A `SyncFile` owns one fence and is the only way a fence crosses the user/kernel
boundary. It is readable once the fence completed, and `read`, `poll`,
`SYNC_IOC_FILE_INFO` and `SYNC_IOC_MERGE` behave as in `sync_file.c` (a merge
returns a fence array of both files).

A `Syncobj` (`model/syncobj.rs`) holds either one fence or a timeline of
points. Setting a value chains it onto the value it replaces: the new value is
signaled only after the old one completed. That one rule gives `FD_TO_HANDLE`,
`TIMELINE_SIGNAL` and `TRANSFER` their Linux meaning (a replacement cannot lose
the dependency it replaced). A replacement also pokes the waiters parked on the
old value, because only a re-read can observe the new one; waiting is therefore
a loop of "observe, register, park, re-observe", never a single park on a fence
that may have been replaced.

`SYNCOBJ_WAIT` and `SYNCOBJ_TIMELINE_WAIT` follow `drm_syncobj_wait_ioctl`: a
signal and an expired timeout are successful waits (no `first_signaled` index),
`WAIT_ALL` reports no index, and an object that can never become available is
`EINVAL` for the infinite wait. `timeout_nsec` accepts all three forms the ABI
defines: `-1` waits forever, `0` polls, a positive value waits that long, and
`DRM_TIME_ABSTIME` treats the value as an absolute CLOCK_MONOTONIC instant. The
deadline is enforced by one sleeping timer per wait (`sync/wait.rs`), the same
mechanism that honours a `dma_fence_set_deadline` hint a producer published.
`HANDLE_TO_FD` names the descriptor after the object, `FD_TO_HANDLE` keeps its
own reference so the descriptor can be closed at once, and `QUERY` reports the
newest available timeline point. A binary replacement is
`drm_syncobj_replace_fence` (the new fence is the whole value, verified against
`drm_syncobj.c`); only a timeline point becomes a chain node.

`SYNCOBJ_EVENTFD` ties an eventfd to a timeline point of one `EVENT` unit (the
node the command arrived through). The relation is armed on the object's
current value and re-armed on every value installed afterwards, so the eventfd
counts points instead of firing once; a value that makes the point available
signals the eventfd once, on the same completion that made it available. The
only defined flag is `WAIT_AVAILABLE`, which signals at registration; any
other bit and a non-zero `pad` are `EINVAL`, as in `drm_syncobj_eventfd_ioctl`.

`sync_file` follows `sync_file.c` after a line-by-line check against the source:
a count-only `SYNC_IOC_FILE_INFO` (`num_fences == 0`) reports the count and the
whole file's status without a buffer, a record request requires
`num_fences >= count` (`EINVAL` otherwise), an odd `flags` or `pad` is
`EINVAL`, `status` is `dma_fence_get_status` (0 active, 1 signaled, negative
error), the file's name is `sync_file_get_name` (the merge's user name, or
`<driver>-<timeline>-<context>-<seqno>`), and `SYNC_IOC_SET_DEADLINE` publishes
a CLOCK_MONOTONIC deadline (`pad` guarded, `EINVAL`). `SYNC_IOC_MERGE` merges
the unwrapped leaves of both files, drops duplicates, and reports a closed or
foreign `fd2` as `ENOENT`. The file has no `read` operation, as in Linux, and
`poll` arms its wake callback on the first poll.

### Descriptor commands

`SYNC_IOC_*` and `DMA_BUF_IOCTL_*` are issued on a descriptor, not on the card,
so they never reach the DRM ioctl table: `sync_file.rs` and `dma_buf_file.rs`
dispatch them to `model/sync_ioctl.rs`, which validates the arguments and copies
them with the same user-memory helpers the DRM ioctls use.

### Capabilities

`DRM_CAP_PRIME`, `DRM_CAP_SYNCOBJ` and `DRM_CAP_SYNCOBJ_TIMELINE` are reported
because their commands are implemented end to end. `DRM_CAP_CRTC_IN_VBLANK_EVENT`
stays tied to a display that reports scanouts (G1), unchanged by this layer.

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

The fence layer adds one lock family with its own rules:

- A `DmaFence`'s callback list is an IRQ-safe spinlock, because a device or a
  timer may complete a fence from interrupt context. A callback runs after the
  lock is released and must stay short.
- Callbacks never take a sleepable lock: a fence array's check, a chain's
  completion check, an eventfd signal and a waiter's `wake` only touch their own
  atomics and spinlocks or enqueue a task.
- A `Syncobj`'s value, waiter list and event list are IRQ-safe spinlocks taken
  together; a chain node or event is registered after all of them are released,
  so the fence lock is ordered before the syncobj lock and never after it.
- A wait (`sync/wait.rs`) parks the task only in sleepable context, outside all
  of the locks above. The parked future re-evaluates on every poll: it checks
  the fence outcome first and then installs its waker, so a completion that
  races the check is seen by the same poll instead of being missed.

## Execution context

All entry points run in process syscall context through `DeviceFileOps`. They
may allocate, copy user memory, and acquire sleepable mutexes. A fence wait
(`SYNCOBJ_WAIT`, `read()` of a sync_file, `DMA_BUF_IOCTL_SYNC`) may sleep the
calling task; fence completion callbacks may run in interrupt context. No task
or workqueue of its own is created by this crate.

## Error model

Handlers and the object model return `DrmError`; `device/minor.rs` maps it to the
errno the syscall layer expects (`EINVAL`, `EOPNOTSUPP`, `ENODEV`, `EBUSY`,
`EACCES`, `EPERM`, `ENOENT`, `ENOMEM`, `EIO`, `EFAULT`, `EAGAIN`). Unknown
command numbers are `EOPNOTSUPP`.

## Known limitations

- No command submission (execbuf/virgl); fences express display and reservation
  completion only.
- A `SYNCOBJ_EVENTFD` relation is cleared only when the eventfd or the object
  is released; Linux also clears it on `drm_syncobj_array` argument reuse,
  which this driver has no equivalent of.
- The `dma-buf` command table matches `dma-buf.c`: `DMA_BUF_IOCTL_SYNC`
  validates exactly `DMA_BUF_SYNC_VALID_FLAGS_MASK`, `DMA_BUF_SET_NAME` caps
  the name at `DMA_BUF_NAME_LEN`, `EXPORT/IMPORT_SYNC_FILE` accept only the
  direction bits, and `poll` reports `EPOLLIN` for the exclusive work and
  `EPOLLOUT` for every user. `DMA_BUF_IOCTL_INFO` is not a Linux ioctl (the
  size is `lseek(SEEK_END)`); it stays as an X-Kernel extension because the
  ABI test and existing clients use it.
- `dma_fence_chain` (replaceable timeline point objects) is not implemented:
  timeline points are chained by callback instead, and `QUERY` reports the
  newest available point by walking the timeline.
- PRIME crosses this card only. A dma-buf is the exporter's guest pages plus its
  reservation, which another card on the same bus could import; a host-private
  resource could not, and this driver never creates one.
- The fence layer keeps no per-fence `/proc` or debugfs view.
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
  mode-blob encoding, the FakeBackend invariants, fences, arrays, reservations
  and syncobjs; the rest is covered by the on-target `abi-tests/` (including
  `drm-sync` for this layer).

## The dma-buf object

A dma-buf descriptor carries its export in the open file's framework slot
(`kvfs::DMA_BUF_SLOT`), typed by `display::DmaBufSlot`. Every importer —
`PRIME_FD_TO_HANDLE` here, a second display engine, a media device — reads it
the same way, instead of asking this driver whether the descriptor happens to
have been created by it. That is the kernel-wide equivalent of Linux's
`dma_buf` plus its `f_op`.

- `PRIME_HANDLE_TO_FD` publishes the export when it creates the descriptor;
  the slot keeps the `Arc<DmaBuf>` for the descriptor's lifetime.
- `PRIME_FD_TO_HANDLE` resolves the descriptor first (`ENOENT` when it is not
  a dma-buf, as a failed `dma_buf_get` is), then imports. This card's own
  buffer takes the fast path over its own pages and reservation; any other
  export is attached, mapped and imported over the reported window, and an
  export that cannot serve an importer is `EOPNOTSUPP`.
- An imported window owns no pages (`PageRef` only exists for memory the
  allocator accounts); the object keeps the attachment, and dropping the
  object detaches.

## The mmap offset namespace

`MAP_DUMB` and `mmap` name an object by a *virtual offset*, not by its handle
(`drm_vma_offset_manager.c`). `model/vma.rs` owns that namespace: offsets are
allocated once per object, page aligned, never reused, and each counts as a
reservation of its range; the present `mmap` path resolves the client's offset
through it (`state.object_by_offset`) and then requires the calling file to
hold
a handle for the object, so a render node still cannot map a buffer it never
received. The offset is held by a weak entry: a stale offset after the object
is gone resolves to nothing instead of a later buffer, and an offset past the
object's last page is not the object's mapping either.

The offset a dma-buf import used to take from the exporter's own numbering is
now allocated per import from this namespace, so two imports of one buffer in
two files never collide on an offset.

The other half of the Linux design — unmapping a `vma` and revoking its
offset when a client unmaps — has no hook in this kernel's VFS, so mappings
keep the object alive until the card goes away. That boundary is unchanged
and is recorded here rather than papered over.
