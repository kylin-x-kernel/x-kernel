# getcwd output-buffer ABI regression

The raw Linux `getcwd` syscall returns a NUL-inclusive byte count and takes
an unsigned machine-word size. Unlike the libc wrapper, it does not allocate
storage for a NULL buffer. A short size returns `ERANGE`; an adequate size
with an inaccessible buffer returns `EFAULT`.

References: [Linux v6.12 fs/d_path.c](https://github.com/torvalds/linux/blob/v6.12/fs/d_path.c#L412)
and the C library/kernel differences in
[getcwd(3)](https://man7.org/linux/man-pages/man3/getcwd.3.html).

## Reproduce

```sh
gcc -O2 -Wall -Wextra -Werror probe.c -o probe
./probe

aarch64-linux-musl-gcc -static -O2 -Wall -Wextra -Werror probe.c -o probe-aarch64
```

Copy the cross-compiled binary to `/root/probe-aarch64` in an offline test
rootfs, unmount it, boot the kernel, and run that path in the guest. The probe
saves its cwd, tests from `/`, and restores its cwd. Exit status 0 means all
assertions passed, 1 means an ABI mismatch, and 2 means fixture/cleanup failure.

All calls use `syscall(SYS_getcwd, ...)`, avoiding libc allocation or fallback.
The 11 assertions cover NUL-inclusive return counts, exact and short sizes,
NULL/invalid outputs, the ERANGE-before-EFAULT order, an unsigned `ULONG_MAX`
capacity, and sentinel bytes beyond the returned pathname.

## Validation

Three kernel regressions in `posix/fs/ut-tests/getcwd.rs` use `#[def_test(user)]` and
the existing `unittest_support` fixture. They call the actual `sys_getcwd`
entry in an isolated process with mapped user buffers, covering unsigned
capacities, the NUL-inclusive count and untouched suffix, short-buffer error
priority, and NULL/unmapped output pointers. Jenkins runs them in its kernel
unit-test stages; no separate probe installation is required. Locally:

```sh
cp platforms/kplat-aarch64/qemu_defconfig .config
make defconfig
make unittest UNITTEST_CRATE=posix_fs ACCEL=n VSOCK=n DISK_IMG=/path/to/test.img
```

The standalone probe additionally checks the raw syscall dispatcher from user
space. On upstream `540d07bb22b5c5cf7aae46f1e20dbb3c1c292bf3` plus this patch:

- AArch64 `make unittest UNITTEST_CRATE=posix_fs`: 45/45 passed, including the
  three new syscall regressions.
- Linux x86_64 reference and the AArch64 `getcwd-abi` guest case: 11/11 passed.
- AArch64 `make build`, `make clippy`, and the pinned formatter check passed.

The guest case runs through a local `ci-test-iter` workspace using Starry Test
Harness `d4e61b9693a39b10876f784fdf3e9e112dca9ae8`. It injects the static
probe and checks its exit status. This is a targeted run, not the full suite.
The original unmodified upstream `1b62d828` failed five of the same 11 probe
assertions. An x86_64 guest was not run.
