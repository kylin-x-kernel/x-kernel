# fbdevice — Design

## Purpose

`fbdevice` emulates a classic framebuffer device (`/dev/fb0`) on top of
the display scanout contract, mirroring the Linux `drm_fbdev` model: the
primary display device's resolution is queried once, a contiguous
guest-memory shadow buffer is allocated and bound to the host as a 2D
scanout resource, and userspace byte access lands in that shadow. The
shadow reaches the screen only when explicitly presented.

## Responsibilities

- `fb_init`: pick the primary display device from the `kclass` display
  registry, validate its resolution, allocate a packed BGRA8888 shadow
  (`alloc_zeroed_pages_exact_ref`, page-aligned), and bind it via
  `DisplayDevice::create_scanout_resource` (`FB_RESOURCE_ID`, physical
  address, length). Any failure leaves emulation unavailable with a
  warning — boot continues without `/dev/fb0`.
- State holder `FbEmulation`: display info, cached primary device handle,
  and the shadow page allocation kept alive for the kernel lifetime so
  the host resource's backing memory stays valid.
- Queries: `fb_available`, `fb_info` (`# Panics`: requires gating on
  `fb_available`), `fb_shadow_vaddr`, `fb_shadow_paddr` (for `mmap`),
  `fb_shadow_size`.
- `fb_present`: push the full shadow surface to the visible scanout via
  `present_scanout_resource`; returns `false` when unavailable or on
  transient host failure. Devfs triggers it on write to `/dev/fb0` and on
  the `FBIOPAN_DISPLAY` ioctl.

## Non-Responsibilities

- No devfs node creation, no `file_operations`, no ioctl decoding: the
  `/dev/fb0` node and its syscall surface live in the devfs layer, which
  consumes these helpers.
- No background refresh: presentation is on demand only (see Design
  Decisions).
- No rendering, damage tracking, or double buffering: one shadow, one
  full-surface present.
- No mode changes after init: resolution is sampled once; resizing is
  unsupported by design.

## Scope

```text
io/fbdevice/
├── src/
│   └── lib.rs        # fb_init, shadow setup, queries, fb_present
└── Cargo.toml
```

## Architecture

```text
kclass display registry -> primary ClassDevice<DisplayDeviceImpl>
        | info()
        v
PageRef shadow + requested length --phys_addr--> create_scanout_resource
        |                                            (host-visible resource)
        v
FB: LazyInit<SpinNoIrq<Option<Arc<FbEmulation>>>>
        |
  fb_shadow_* queries (devfs mmap/read/write)   fb_present() -> scanout
```

The zeroed exact allocator clears every selected page before host publication.
FbEmulation keeps shadow_len_bytes for the requested page-aligned buffer length;
queries and host resource descriptors use that length. Unused raw backing is
returned before the page range escapes.
PA comes from PageRef; only the VA query performs a direct-map conversion.

## Execution Context

- `fb_init` runs once during device bring-up, after the `kclass` display
  registry is seeded; a missing display device is treated as "no display
  hardware", not an error.
- Queries and `fb_present` are callable from syscall context; the state
  lock (`SpinNoIrq`) is held only to clone the `Arc` or read fields, and
  the present call itself runs without the lock held on the registry.
- No teardown exists: the shadow and the host resource live for the
  kernel lifetime, which is what makes the cached pointers safe to
  publish.

## Concurrency Model

- `FB` is a `LazyInit<SpinNoIrq<Option<Arc<FbEmulation>>>>`; after init
  the inner state is effectively read-only, and `fb_present` clones the
  `Arc` so device calls happen lock-free.
- The primary device handle is cached at init to avoid re-snapshotting
  (and allocating from) the whole class registry on every present.

## Error Model

- Setup failures (no display, zero resolution, allocation failure,
  `create_scanout_resource` error) downgrade to "emulation unavailable"
  with a logged warning; nothing panics during init.
- `fb_info` is the one panicking accessor, explicitly documented, and
  must be gated on `fb_available`.
- `fb_present` returns `bool`; present failures are transient and never
  fatal.

## External Boundary

- The shadow buffer is guest memory described to the host (virtio-gpu
  style) by physical address: the host reads/writes it as scanout
  backing, and userspace maps it through `fb_shadow_paddr`. The backing
  must stay resident — guaranteed by holding the `PageRef` forever.
- Trust assumption: the display driver honors the scanout resource
  contract; a misbehaving driver can show stale pixels but cannot
  corrupt kernel memory through this crate.

## Design Decisions

- Compatibility shim over scanout instead of a separate fbdev hardware
  path: any `DisplayDevice` (scanout-only virtio-gpu or a future
  directly-mapped device) gets a working `/dev/fb0` without exposing a
  directly-mapped buffer.
- No background refresh task: a continuous present would race an active
  DRM compositor for the single physical scanout and cause flicker;
  fbdev emulation defers to a DRM master and presents only on explicit
  request (write or `FBIOPAN_DISPLAY`).
- `FBIOPAN_DISPLAY` presents and reports success, but never pans: the shadow
  is exactly one full-screen buffer with `xpanstep == ypanstep == ywrapstep
  == 0` in `FBIOGET_FSCREENINFO`, so a client that reads those steps (as
  Linux clients do) knows there is nothing to pan. Linux would answer
  `EINVAL` for a non-zero pan offset; this driver accepts the call as
  "present the shadow", which is the only effect a pan can have here. The
  difference is deliberate and is why the offsets are reported as zero
  rather than as a supported but ignored range.
- A DRM master that is actively presenting owns the scanout: an fbdev
  present between two compositor commits would be overwritten by the next
  commit, so a mixed session does not flicker — it shows the compositor's
  frame until the next fbdev request, which is the same ordering a
  compositor has with any other client of the plane.
- Shadow allocated contiguous and page-aligned: one region serves both
  userspace `mmap` and a single host resource description, with pitch
  equal to `width * 4` (packed rows).
- Failure-soft init: a display driver that fails scanout setup cannot
  take down boot; the absence of `/dev/fb0` is the visible symptom.
