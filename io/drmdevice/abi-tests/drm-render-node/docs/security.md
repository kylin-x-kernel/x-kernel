# drm-render-node-regression — Security and reliability

## Unsafe code

The package has no dependencies and no kernel-side code. Every kernel
interaction goes through one `unsafe extern "C" { fn ioctl(...) }` declaration
and a single `unsafe` call site (`ioctl_raw()`), which is the entire FFI
boundary. There is no other `unsafe` block, no assembly, and no `libc`
dependency.

## Trust model

Every DRM ioctl payload is untrusted user input from the test's perspective:
request structures are defined locally with `#[repr(C)]` and passed through the
FFI wrapper, so the only attack surface is the guest itself (it runs as root
and changes the scanout). The test is never shipped inside the kernel.

The test binary runs as root inside the guest and trusts nothing: every
request structure is defined locally with `#[repr(C)]`, ioctl numbers are
derived from the structure size (`_IOWR('d', nr, T)`), and every failure is
asserted rather than skipped. It is a verification tool, not a shipped
component.

## Unsafe/FFI boundary: `ioctl(2)`

Every kernel interaction goes through one foreign function:

```rust
unsafe extern "C" { fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int; }
```

- **Call site**: `ioctl_raw()` in `render_node.rs`; it is the only `unsafe`
  block in the package. There is no other FFI, no assembly, and no `libc`
  dependency.
- **Unsafe prerequisites it honours**: the request number is derived from the
  structure it is paired with (`_IOWR('d', nr, T)` = `size_of::<T>()`), so the
  number and the buffer layout cannot drift; the variadic argument is always a
  pointer to a live, initialised `#[repr(C)]` structure owned by the caller for
  the duration of the call; output-only requests are zero-initialised before the
  call; negative return values are converted through
  `io::Error::last_os_error()` immediately, before any other syscall can
  overwrite `errno`.
- **Buffers touched**: `GetCap`, `CreateDumb`, `DestroyDumb`, `GemClose`,
  `FbCmd2`, `RmFb`, `DirtyFb`, `CrtcPageFlip`, `PrimeHandle` — all defined in
  the test file with `#[repr(C)]` and fixed size. No pointer is stored past the
  call and no kernel-provided pointer is dereferenced.
- **Failure containment**: the package runs as root in a disposable guest and
  only talks to `/dev/dri/*`; a wrong request is rejected by the kernel and
  reported as a test failure.

## Boundaries

- It cannot open the device when a compositor holds it as master; that is
  reported as a failure, not silently skipped, because a skipped assertion
  would hide a regression.
- It never depends on kernel-private headers: only the Linux UAPI layout.
- `DRM_DEVICE` / `DRM_RENDER_NODE` select the nodes; the defaults are
  `/dev/dri/card0` and `/dev/dri/renderD128`.

## Invariants asserted

- A render node accepts only render-allowed commands (`EACCES` otherwise) and
  never becomes master.
- A dirty flush does not require the caller to be the framebuffer's creator.
  `fb_id == 0` is not special: it fails with `ENOENT` like any unknown id.
- GEM handles are per open file, and closing a file that still owns handles is
  safe for framebuffers already registered.

## Failure behaviour

Panics on the first violated assertion, with the errno in the message. There is
no cleanup beyond closing descriptors: the guest is disposable and the driver
must tolerate clients that exit without `GEM_CLOSE` (that is one of the
properties under test).
