# DRM render node / framebuffer flush / release-by-reference regression tests

This directory is a standalone userspace Cargo package; it does not link the kernel libraries or change the dependencies of the kernel workspace.
The tests check the visible ABI through real file descriptors and DRM ioctls, and apply to 64-bit X-Kernel Linux userspace environments.

`render_node` covers:

- `/dev/dri/renderD128` exists and can be opened read-write; the primary node `card0` and the render node share the same device number family (226).
- `GET_CAP` on the render node succeeds; `CREATE_DUMB` returns `EACCES` (Linux's `DRM_RENDER_ALLOW` allowlist,
  not `ENOTTY`).
- `PRIME_HANDLE_TO_FD` on the render node returns `EINVAL` for an unknown handle: the command itself is allowed by the node, only the handle
  does not exist, which distinguishes "node rejects" from "handle invalid".
- `DIRTYFB` looks up the framebuffer by `fb_id`: `fb_id == 0` returns `ENOENT` just like any unknown id,
  while a registered framebuffer is flushed successfully (consistent with Linux; Xorg depends on this distinction).
- `DESTROY_DUMB` returns success while a framebuffer still references the resource and the resource is being scanned out; the subsequent `RMFB`
  releases the reference, and creating and committing a new framebuffer still displays normally (no black flash).
- The handle table is isolated per open file: a handle created by another descriptor returns `EINVAL` on `GEM_CLOSE` on this descriptor,
  and closing the file that holds the handle does not affect the framebuffers already registered on this file.

Maintainers can build this directory as a standalone package in a build environment that provides a standard Rust Linux userspace toolchain:

```sh
cd io/drmdevice/abi-tests/drm-render-node
cargo test --target aarch64-unknown-linux-musl --no-run
```

Copy the generated `target/aarch64-unknown-linux-musl/debug/deps/drm_render_node_regression-*`
executable to the X-Kernel userspace environment and run it directly:

```sh
DRM_DEVICE=/dev/dri/card0 DRM_RENDER_NODE=/dev/dri/renderD128 \
    /usr/tests/drm_render_node_regression --nocapture
```

The tests need a standalone guest with DRM support, and the device must not be held by Xorg, a compositor, or other clients at the same time.
They fail explicitly when the device is missing or an external owner already exists, and do not skip assertions. On other 64-bit architectures, use the corresponding Linux musl target.

The kernel allows only one master, following Linux semantics, so the tests must run serially (each test opens the device itself
and releases master at the end):

```sh
/usr/tests/drm_render_node_regression --nocapture --test-threads=1
```
