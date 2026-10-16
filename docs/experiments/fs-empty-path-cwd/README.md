# AT_EMPTY_PATH current-directory regression

`fstatat(AT_FDCWD, "", ..., AT_EMPTY_PATH)` must identify the current directory.
The shared resolver previously looked up descriptor `-100`, returning `EBADF`.
The same error affected `statx`, `faccessat2`, `fchmodat2`, `fchownat`, and
`utimensat` with an actual empty pathname.

The fix clones the cwd path from a `with_fs` snapshot. The pathname match
explicitly distinguishes `Some("")` from the internal `None` form. It preserves
the resolver's internal `None` descriptor-only form, used by `fstat`, `fchmod`,
`fchown`, and fd-based xattr operations. Those calls must not accept `AT_FDCWD`
as a real descriptor. This patch does not expand NULL pathname support.

## Reproduce

Build and run a native Linux reference:

```sh
gcc -Wall -Wextra -Werror -O2 docs/experiments/fs-empty-path-cwd/probe.c -o /tmp/empty-path-probe
/tmp/empty-path-probe
```

Build the same probe for an AArch64 guest (adjust the compiler prefix to the
installed toolchain):

```sh
aarch64-linux-musl-gcc -Wall -Wextra -Werror -O2 -static \
  docs/experiments/fs-empty-path-cwd/probe.c -o /tmp/empty-path-aarch64
cp platforms/kplat-aarch64/qemu_defconfig .config
make defconfig
make build
make clippy
```

Inject the binary as `/root/empty-path-probe` into a disposable test image,
then execute it in the guest. The probe creates its own directory under `/tmp`
and removes it afterwards. Do not modify an image in use by QEMU.

## Evidence

Compared with upstream `772fe7ae`, using the same static probe and separate
64 MiB ext4 images, QEMU AArch64 with four CPUs and 1 GiB RAM:

| Run | Result |
| --- | --- |
| Native Linux 7.0.0-34-generic | 19 checks, 0 failures |
| Unmodified upstream | 19 checks, 6 failures, all six cwd operations return EBADF |
| Patched kernel | 19 checks, 0 failures |

Controls cover real directory descriptors, `O_PATH`, missing `AT_EMPTY_PATH`,
invalid descriptors, fd-only calls rejecting `AT_FDCWD`, and the existing
`utimensat` NULL-descriptor / double-`UTIME_OMIT` paths.

The standalone probe also runs as `empty-path-cwd-abi` in a local
`ci-test-iter` workspace using Starry Test Harness
`d4e61b9693a39b10876f784fdf3e9e112dca9ae8`. It is not a full suite run.

On upstream `540d07bb` plus this patch, AArch64 filesystem unit tests passed
45/45, the Linux and AArch64 probes both passed 19/19, and AArch64 build,
clippy, and formatting checks passed. An x86_64 guest was not run.

Linux references: [`fstatat` and `AT_EMPTY_PATH`](https://man7.org/linux/man-pages/man2/stat.2.html),
[`statx`](https://man7.org/linux/man-pages/man2/statx.2.html),
and [`fs/namei.c`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/fs/namei.c).

## Automatic regression tests

`posix/fs/ut-tests/empty_path.rs` registers three user-context kernel tests:
empty-path cwd metadata matches `.`, a missing flag remains `ENOENT`, and
internal fd-only resolution plus fstat/fchmod/fchown/flistxattr keep rejecting
`AT_FDCWD` with `EBADF`. A nonempty relative path still checks its real dirfd.

```sh
cp platforms/kplat-aarch64/qemu_defconfig .config
make defconfig
make unittest UNITTEST_CRATE=posix_fs ACCEL=n VSOCK=n DISK_IMG=/path/to/test.img
```
