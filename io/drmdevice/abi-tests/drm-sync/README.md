# DRM sync object / dma-buf regression cases

This directory is a standalone userspace Cargo package; it does not link the kernel libraries and does not change the dependencies of the kernel workspace.
The cases use real file descriptors and DRM / dma-buf / sync_file ioctls to check the
synchronization semantics introduced by G2, and apply to the 64-bit X-Kernel Linux user environment:

- The `SYNCOBJ_*` command words are all hard-coded (`0xC0`–`0xCA`), and `drain_sync_defines_the_uapi_words`
  re-derives the `repr(C)` struct sizes (8/8/24/40/24/56/32/24, `sync_file_info` 56,
  `sync_fence_info` 80, `dma_buf_sync` 16); layout drift only makes the call fall to `EOPNOTSUPP`
  and never passes quietly.
- `DRM_CAP_SYNCOBJ`, `DRM_CAP_SYNCOBJ_TIMELINE` and `DRM_CAP_PRIME` report only capabilities that are already implemented.
- Binary syncobj: signal, `WAIT`, timeout (success return with `first_signaled == -1`), `WAIT_ALL`,
  `HANDLE_TO_FD` exporting a readable sync_file, and `ENOENT` after `DESTROY`.
- Timeline: signaling by point, `QUERY` reporting the latest available point, earlier points equally available, and `RESET` clearing the points.
- `TRANSFER` and `FD_TO_HANDLE` chain a fence onto another object; importing a card fd as a sync_file returns `EINVAL`.
- Illegal flags, an empty object array and a negative timeout all return `EINVAL`.
- PRIME: `PRIME_HANDLE_TO_FD` returns a real dma-buf fd; `DMA_BUF_IOCTL_INFO` reports the byte count;
  START/NOWAIT of `DMA_BUF_IOCTL_SYNC` completes immediately, and a direction of 0 returns `EINVAL`;
  the sync_file of `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` has only one fence record;
  `PRIME_FD_TO_HANDLE` on the same fd again yields the same backing; a non-dma-buf fd returns `ENOENT`
  (corresponding to a Linux `dma_buf_get` failure; "not supported" is used only for exports that are indeed dma-bufs but cannot be imported by this card);
  the exported object stays valid after the GEM handle and the card descriptor are closed.
- Import the same dma-buf from a second card descriptor, use `ADDFB2` to create a framebuffer for the imported object
  (it only needs to fit in the object and need not have the same geometry as the exported object), do `DMA_BUF_IOCTL_SYNC`
  START/END in the write direction, and then `mmap` the dma-buf itself.
- `MODE_ATOMIC`: the plane exposes `IN_FENCE_FD` (reads back `-1`) and the CRTC exposes `OUT_FENCE_PTR`;
  a card fd used as `IN_FENCE_FD` returns `EINVAL`; `TEST_ONLY` writes `-1` into the out-fence word;
  an unmapped `OUT_FENCE_PTR` returns `EFAULT`; the out-fence fd returned by one real commit is
  readable after the page flip completes, with status 0 and a completion timestamp.

Build and run (same as `drm-gem-close`):

```sh
cd io/drmdevice/abi-tests/drm-sync
cargo test --target aarch64-unknown-linux-musl --no-run
```

Copy the generated executable into the X-Kernel user environment and run it:

```sh
DRM_DEVICE=/dev/dri/card0 /usr/tests/drm_sync_regression --nocapture
```

`dma_buf_exports_and_imports_one_backing` does not need exclusive use of the card; the other cases need an idle card
(`SET_MASTER` not held by another client).
