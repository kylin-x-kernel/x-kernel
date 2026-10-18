# statx reserved-mask and sync-mode validation

Linux rejects `STATX__RESERVED` in the request mask and rejects the combination
`AT_STATX_FORCE_SYNC | AT_STATX_DONT_SYNC` with `EINVAL`. The output buffer is
not written. X-Kernel previously ignored both inputs, returning metadata for
invalid requests or allowing target lookup errors to obscure invalid input.

This change checks these two contracts before copying the pathname or looking up
the target. It keeps zero masks and unknown, nonreserved mask bits valid.
It does not change supported metadata, result-mask reporting, sync policy, or
the handling of other lookup flags.

## Run the probe

```sh
gcc -Wall -Wextra -Werror -O2 docs/experiments/statx-request-validation/probe.c -o /tmp/statx-probe
/tmp/statx-probe
aarch64-linux-musl-gcc -Wall -Wextra -Werror -O2 -static \
  docs/experiments/statx-request-validation/probe.c -o /tmp/statx-aarch64
cp platforms/kplat-aarch64/qemu_defconfig .config
make defconfig
make build
make clippy
```

Inject `/tmp/statx-aarch64` into a disposable guest image and execute it there.
The test only queries the root directory and closes its own descriptor. No
production rootfs or user files are changed.

The probe has 22 checks. Invalid requests assert both errno and an unchanged
output buffer. Controls cover zero and unknown nonreserved mask bits, each
sync mode alone, real descriptors, and valid requests with bad pathnames.

## Error precedence and automated coverage

The combination `statx(AT_FDCWD, "", 0, STATX__RESERVED, ...)` returns `EINVAL`
on Linux, as does an empty pathname with both sync flags. Without an invalid
request, the same empty pathname without `AT_EMPTY_PATH` returns `ENOENT`.
NULL or unmapped pathname addresses also do not override invalid request bits.
Linux v6.12 `do_statx` checks these bits before passing the result of
`getname_flags` to path lookup. The raw-syscall probe confirms this behavior on
Linux 7.0.0-38-generic.

Three kernel tests in `posix/fs/ut-tests/statx_request.rs` call the real
`sys_statx` entry with user mappings. They verify error precedence, untouched
output on failure, and accepted request combinations. They are collected by
the regular kernel unit-test stages:

```sh
make unittest UNITTEST_CRATE=posix_fs ACCEL=n VSOCK=n DISK_IMG=/path/to/test.img
```

The guest probe is registered as `statx-request-abi` in a local `ci-test-iter`
workspace using Starry Test Harness `d4e61b9693a39b10876f784fdf3e9e112dca9ae8`.
This is targeted coverage, not a full harness suite run.

On upstream `540d07bb` plus this patch: AArch64 filesystem unit tests passed
45/45; Linux and AArch64 probes passed 22/22; AArch64 build, clippy, and the
pinned formatter passed. An x86_64 guest was not run.

References: [statx(2)](https://man7.org/linux/man-pages/man2/statx.2.html)
and [Linux v6.12 do_statx](https://github.com/torvalds/linux/blob/v6.12/fs/stat.c#L711).
