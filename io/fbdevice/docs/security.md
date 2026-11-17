# fbdevice — Security And Reliability

## Scope

This analysis covers the entire crate — the single `src/lib.rs`
(`fb_init`, shadow setup, queries, `fb_present`). The `/dev/fb0` devfs
node and its syscall surface live in the devfs layer and are excluded;
this document covers the state, boundaries, and presentation path
behind that node.

The crate shares one guest-memory shadow buffer between three parties:
user space (via devfs mmap of `/dev/fb0`), the display driver (scanout
resource), and the host (which reads the shadow through the virtual GPU).
The kernel trusts the display driver to honor the scanout contract; user
space is untrusted but can only touch the shadow via its mapping — never
kernel memory through this crate.

## External Boundaries

- **User-space mapping**: `/dev/fb0` mmap maps the shadow's physical
  pages (`fb_shadow_paddr`). Access is confined to the shadow buffer;
  size clamping belongs to the devfs mmap layer, which receives the
  exact size from `fb_shadow_size`.
- **Host access**: the shadow is published to the host as a scanout
  resource (`create_scanout_resource` with physical address and length).
  The host can read/write the shadow for as long as the resource exists —
  which is forever, since teardown is unsupported and the backing
  `PageRef` is retained for the kernel lifetime.
- No ioctl payloads are parsed here; the devfs layer owns the syscall
  surface.

## Unsafe Code

None. The crate contains no `unsafe`, no FFI, and no inline assembly
(device interaction goes through the `DisplayDevice` trait object).

## Protected Resources

- `FB: LazyInit<SpinNoIrq<Option<Arc<FbEmulation>>>>` — the single
  emulation state; the `Arc` keeps the shadow and device handle alive
  independent of the registry lock.
- The shadow `PageRef` — kernel-lifetime contiguous allocation; kept
  resident so the host resource's backing cannot be reused after a
  failed present.

## Invariants

- The shadow is zeroed before being described to the host, so no stale
  kernel data is disclosed through the display.
- `fb_info` panics unless `fb_available` was checked — the panic is
  documented and caller-gated, never reachable from a raw user syscall.
- `fb_present` never blocks: the present call runs on a cloned `Arc` with
  no lock held on the registry.

## Threat Analysis

| ID | Threat | Impact | Trigger | Existing control |
|----|--------|--------|---------|------------------|
| T-01 | User writes into the shadow and forces a present | Low — screen shows attacker-chosen pixels | Writing to `/dev/fb0` then `FBIOPAN_DISPLAY` | By design: that is the framebuffer interface. No kernel memory is reachable through the mapping. |
| T-02 | Host reads stale kernel data from the shadow | Medium — kernel data disclosure via the display | Shadow not zeroed, or buffer reused | Shadow is allocated fresh and zeroed before `create_scanout_resource`; retained forever, never reused. |
| T-03 | Display driver programs the host with a wrong address/length | Medium — device DMA outside the shadow | Driver bug in `create_scanout_resource` handling | Address and length come from the validated page allocation (`PageRef::phys_addr`, separately retained requested page-aligned length); driver conformance is the residual trust. |
| T-04 | Present racing device removal | Low — error return or stale frame | Hot-removal of the display device | No teardown exists; the cached device handle outlives everything, so the race cannot produce UAF — worst case is a failed present (`false`). |
| T-05 | `FBIOGET_FSCREENINFO` leaks kernel memory through struct padding | High — uninitialized kernel bytes reach userspace | The ioctl copies implicit gaps in `FixScreenInfo` | The ABI gaps are explicit fields set to zero; framebuffer results copy through `UserPtr::write_vm` with `UserWrite`. |

## Failure Modes

| ID | Failure mode | Local effect | System effect | Severity | Handling |
|----|--------------|--------------|---------------|----------|----------|
| F-01 | No display device at init | Emulation unavailable, warning logged | No `/dev/fb0`; boot continues | 3 | Failure-soft `fb_init`. |
| F-02 | Zero resolution reported | Emulation skipped | No `/dev/fb0` | 3 | Explicit validation before allocation. |
| F-03 | Shadow allocation fails | Emulation unavailable | No `/dev/fb0` | 3 | Checked allocation with warning. |
| F-04 | `fb_info` without availability check | Panic (documented) | Caller bug surfaces loudly | 4 | `# Panics` section + gating convention. |
| F-05 | Present fails transiently | `fb_present` returns `false` | Stale frame until next present | 4 | Non-fatal by design. |

## Known Limitations

- No teardown: the shadow and host resource persist for the kernel
  lifetime; repeated init cycles are not supported.
- Full-screen present only — no partial damage tracking.

## Audit Checklist

- The shadow returned by alloc_zeroed_pages_exact_ref is fully cleared before
  create_scanout_resource publishes it. FbEmulation retains its PageRef for the
  kernel lifetime; no free path exists. Exact allocation returns unused backing
  before publication, and shadow_len_bytes keeps the requested mapping bounds.
- New accessors keep the lock-free-after-clone pattern (no lock held
  across device calls).
- Any new user-visible size/offset path clamps against
  `fb_shadow_size`.
