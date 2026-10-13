# display — Design

## Purpose

`display` defines the contract for graphics display device drivers: the
`DisplayDevice` trait with basic mode information and framebuffer
flushing, plus an optional host-resource scanout path
(`create_scanout_resource` / `destroy_scanout_resource` /
`present_scanout_resource`) modeled on virtio-gpu 2D resources for
drivers that can scan out guest memory directly.

## Responsibilities

- Define `DisplayDevice: Device`: `info` (visible `width`/`height` as
  `DisplayInfo`), `need_flush`, and `flush` to push the framebuffer to
  the screen.
- Define the optional scanout-resource operations: creating a
  host-visible 2D resource backed by guest memory
  (`ScanoutResource` descriptor plus physical address and length),
  destroying it by id, and presenting a `ScanoutRect` region of a
  resource as the active scanout.
- Define the shared scanout vocabulary: `ScanoutRect` (x, y, width,
  height in pixels), `ScanoutFormat` (currently `Bgra8888`, the 32-bit
  BGRA/XRGB layout consumed by virtio-gpu 2D resources), and
  `ScanoutResource` (id, width, height, pitch, format).
- Re-export the `driver_base` vocabulary (`Device`, `DeviceKind`,
  `DriverError`, `DriverResult`) so display drivers import one contract
  root.

## Non-Responsibilities

- No framebuffer memory management: who allocates the scanout surface and
  where it lives is the driver's and framebuffer layer's business; the
  trait only describes flushing.
- No mode setting, multi-monitor topology, or EDID handling: one visible
  geometry, no mode enumeration.
- No rendering, compositing, or damage tracking: `need_flush`/`flush` is
  the whole frame-level contract.
- No user-space exposure: `/dev/dri`-style interfaces belong to the
  drmdevice/fbdevice layers, which consume implementers of this trait.

## Scope

```text
drivers/contracts/display/
├── src/
│   └── lib.rs        # DisplayDevice trait, display/scanout types
├── ut-tests/         # unit tests (unittest harness)
└── Cargo.toml
```

## Architecture

```text
driver_base (Device base, DriverError/Result, discovery pipeline)
      ^
      |  impl DisplayDevice for <driver>   e.g. virtio-gpu
      |
display (this crate: trait + scanout types)
      ^
      |  info/need_flush/flush, scanout resource ops
      |
framebuffer/display consumers (fbdevice, drmdevice layers)
```

The crate is one trait plus value types; there is no registry and no
state.

## Scanout Resource Lifecycle

`create_scanout_resource` / `destroy_scanout_resource` form a paired
ownership protocol over host-visible 2D resources:

- **Create**: the consumer (for example `fbdevice` at `fb_init`) passes a
  `ScanoutResource` descriptor plus the physical address and length of
  guest memory. The driver forwards the region to the host, which gains
  read/write access to it for the resource lifetime.
- **Use**: the caller must keep the backing memory alive and resident for
  as long as the resource exists — the host reads it as scanout backing
  at any time. `fbdevice` satisfies this by holding its `GlobalPage`
  shadow for the kernel lifetime. The buffer should be zeroed before
  `create_scanout_resource` so no stale kernel data is disclosed through
  the display.
- **Present**: `present_scanout_resource` only makes a region of the
  resource the active scanout; it does not create or free anything.
- **Destroy**: `destroy_scanout_resource(resource_id)` releases the host
  resource; after it returns, the backing memory may be reused. Drivers
  must reject unknown ids without affecting other resources.

## Execution Context

- All methods take `&self` and must not sleep; implementations synchronize
  internally if the hardware is shared.
- `flush` is intended for driver/ktask-driven refresh paths, including
  from deferred work; implementations must tolerate being called
  repeatedly.
- Scanout resource calls are optional capabilities: drivers without host
  resource support keep the default implementations.

## Error Model

All fallible operations return `driver_base::DriverResult`:

- `flush` — hardware-level failure to push the frame.
- `create_scanout_resource`, `destroy_scanout_resource`,
  `present_scanout_resource` — default
  `DriverError::Unsupported` for drivers without the capability;
  implementers return their own errors for invalid descriptors, resource
  exhaustion, or an unknown resource id.

## Design Decisions

- Two-tier trait: mandatory simple flush plus optional guest-memory
  scanout resources. Simple framebuffer drivers (and early boot consoles)
  need only `flush`; virtio-gpu-class devices can offer zero-copy
  scanout without burdening everyone else.
- Guest memory scanout addressed by physical address (`paddr`, `length`)
  with an explicit `pitch` and format: mirrors how virtio-gpu 2D
  resources attach guest memory, avoiding a copy at present time.
- Single `Bgra8888` format initially: it is the layout virtio-gpu 2D
  resources and simple framebuffers agree on; widening `ScanoutFormat`
  is additive and does not change the trait.
- Re-export `driver_base` types: consistent with the sibling contract
  crates (`char`, `input`, `net`, `block`), one error vocabulary and one
  import root for implementers.
