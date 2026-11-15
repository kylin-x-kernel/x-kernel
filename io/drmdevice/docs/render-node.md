# Render node, framebuffer dirty flush and reference-counted release

Status: A5 / A7 / A8 from `docs/MAINLINE-GAPS-BEST-IMPL.md`, implemented on top
of the object model described in `design.md`.

## A5 — render node

A card has two minors; the render minor is `128 + card index` and both resolve
to the same `DrmDevice`.

- **Topology**: `kdevice::DeviceNode::render_device_number()` /
  `render_devname()` are the only place the `128 + index` rule is written down.
  devfs registers a second cdev for the same card, `kdevice-uevent` publishes
  the render `DEVPATH`/`DEVNAME`/`uevent`, and sysfs projects
  `class/drm/renderD*`, `devices/<phys>/drm/renderD*` and `dev/char/226:128`.
- **Permission**: the ioctl table carries a `RENDER_ALLOW` bit per command; a
  render file is refused with `EACCES` for everything else. Among the commands
  this driver implements the bit is set exactly on `VERSION`, `GET_CAP`,
  `GEM_CLOSE`, `PRIME_HANDLE_TO_FD` and `PRIME_FD_TO_HANDLE` — the same set
  Linux grants. `MODE_CREATE_DUMB` is *not* render-allowed, as in Linux.
- **Master**: a render file is never master and gets a private master, so
  `GET_UNIQUE` reports no identity and `SET_VERSION`/`AUTH_MAGIC` cannot touch
  the card's.

There is no per-arm node-kind guard anywhere: the rule is one column of the
table plus `check_permission`.

## A7 — `DIRTYFB`

A dirty request pushes pixels; it must not change which framebuffer the plane is
bound to. The operation is `Backend::flush` (transfer + flush of the bound
resource), not `Backend::present` (flush + `SET_SCANOUT`). Keeping the two verbs
apart is what makes "dirty" expressible at all.

An unknown `fb_id` — including zero — fails the framebuffer lookup and returns
`ENOENT`, which is what upstream Linux does in every version from v4.9 through
master (`drm_mode_dirtyfb_ioctl()` has no `fb_id == 0` branch in any of them).
The earlier revision of this note incorrectly claimed Linux had such a branch;
the claim has been removed and the code now matches upstream.

Flags are masked with `DRM_MODE_FB_DIRTY_FLAGS` (unknown bits are ignored, as in
Linux), a clip count and clip pointer must both be present or both absent,
`ANNOTATE_COPY` requires an even count, the count is bounded by
`DRM_MODE_FB_DIRTY_MAX_CLIPS`, and the clip array must be readable (`EFAULT`).
Rectangles are not applied: this driver presents whole frames.

## A8 — release by reference

There is no parking table and no "unsettled" set. A handle, a framebuffer and
the scanout slot each hold an `Arc<GemObject>`; the object's destructor destroys
the scanout resource when the last reference goes away. Consequences:

- `DESTROY_DUMB`/`GEM_CLOSE` while a framebuffer still names the buffer simply
  drops that reference; the framebuffer keeps it alive.
- `RMFB` on a framebuffer owned by the calling file that is on screen unbinds
  the plane, clears the scanout slot and drops the object reference; the
  resource is destroyed only after the host stops scanning it (the next
  successful present). A foreign or unknown id — including zero — is `ENOENT`
  and leaves the committed plane state and scanout untouched, matching Linux's
  per-file `file_priv->fbs` lookup.
- Closing a file drops the handles it still owns (`drm_gem_release()`), so a
  client that exits without `GEM_CLOSE` no longer leaks its buffers.
- Closing a file also destroys the property blobs it created
  (`drm_property_destroy_user_blobs()`), removing both the device-registry and
  per-file registrations under one commit section.
- `DESTROY_BLOB` only accepts an id in the calling file's set. An existing id
  owned by another file, or a driver-owned system blob, is `EPERM`; an unknown
  id is `ENOENT`, matching Linux's per-file ownership check.

## Verification

- `make build`, `make clippy` (header/lint/deps) pass.
- Portable unit tests (`ut-tests/`, 37): permission matrix over the whole ioctl
  table for both node kinds and both ownership states, unique command numbers,
  the Linux render allowlist, the capability table, the mode-blob round trip,
  atomic flag validation, signed plane-property metadata, the wildcard object
  type accepted by `OBJ_GETPROPERTIES`, the short-blob size probe (success with
  the required length, no copy), immutable-property writes returning `EINVAL`
  while an unattached property remains `ENOENT`, a null array destination,
  legacy `SETCRTC` without a framebuffer returning `ENOENT`, and — over a
  `FakeBackend` — removed-framebuffer retention until the next present,
  `DROP_MASTER` revoking master-only commands, dirty flush not rebinding,
  cross-file `RMFB`/`CLOSEFB` refusal, blob ownership enforced for
  `DESTROY_BLOB`
  (including a non-owner and a system blob), blob ownership removed by both
  `DESTROY_BLOB` and file release, atomic object-count bounds, and page-flip
  event-slot reservation.
- On-target ABI tests, run in a QEMU virtio-gpu guest (`GRAPHIC=y ACCEL=n`):
  - `drm-render-node` (5): render node exists, permission matrix (`CREATE_DUMB`
    → `EACCES`, `PRIME` unknown handle → `ENOENT`), `DIRTYFB` with zero and
    unknown ids → `ENOENT`, `DESTROY_DUMB` before `RMFB`, per-file handle
    scoping;
  - existing suites, unmodified: `drm-identity` (1), `drm-mode` (7),
    `drm-set-property` (1), `drm-gem-close` (1).
- Guest topology: `/dev/dri/card0` (226:0) and `/dev/dri/renderD128` (226:128),
  `class/drm/renderD128`, `dev/char/226:128`, `uevent` =
  `MAJOR=226 / MINOR=128 / DEVNAME=dri/renderD128 / DEVTYPE=drm_minor`.

## Out of scope

- Real dma-buf: PRIME stays a handle round-trip stub and `DRM_CAP_PRIME` is `0`.
- `DRM_AUTH` enforcement per command (the magic handshake is a compatibility
  protocol); `MASTER` and the render allowlist are enforced.
- Command submission (execbuf/virgl) and a control node.
