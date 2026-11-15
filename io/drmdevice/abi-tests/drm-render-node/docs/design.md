# drm-render-node-regression — Design

## Purpose

Standalone userspace package (not part of the kernel workspace) that exercises
the DRM render node, framebuffer dirty flush and reference-counted release
through the real ioctl ABI, on a running X-Kernel guest.

## Scope

`render_node.rs` is a self-contained `#[test]` harness: a minimal set of
`#[repr(C)]` request structures, ioctl numbers derived from their sizes, and
raw `ioctl(2)` calls. It links no kernel crate and does not affect the kernel
workspace.

## Test groups

| Test | What it proves |
|---|---|
| `render_node_permission_matrix` | `renderD*` exists, `GET_CAP` works there (`PREFER_SHADOW == 1`), `CREATE_DUMB` is refused with `EACCES`, `PRIME` is allowed but rejects an unknown handle with `ENOENT` (Linux's failed `drm_gem_object_lookup`), `SET_MASTER` is refused |
| `every_card_has_a_matching_render_node` | every `/dev/dri/cardN` has `/dev/dri/renderD{128+N}` |
| `dirtyfb_flushes_without_rebinding` | `fb_id == 0` is `ENOENT` like any unknown id, a registered fb flushes, and the clip protocol is validated (`EINVAL` for a count without a pointer or an odd `ANNOTATE_COPY` count) |
| `dumb_release_outlives_the_scanout` | closing a handle that a framebuffer still names succeeds; the next frame still presents |
| `handles_are_scoped_to_the_opening_file` | a handle created by another file is not closable here |

## Constraints

- One master per card (Linux semantics), so the suite must run with
  `--test-threads=1`; each test opens and closes its own descriptors.
- The device must not be in use by a compositor: the tests change the scanout.

## Build and run

See `README.md`. The binary is a normal `cargo test` harness cross-built for a
64-bit Linux target (`aarch64-unknown-linux-musl`) and copied into the guest.
