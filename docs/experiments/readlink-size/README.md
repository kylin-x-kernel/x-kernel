# readlink signed-length ABI regression

`readlink(2)` and `readlinkat(2)` take an `int` length in the Linux syscall
ABI, even though their libc declarations use `size_t`. Reject a non-positive
low 32-bit value with `EINVAL` before accessing the pathname. Positive values
truncate the target without appending a NUL byte.

References: [Linux v6.12 fs/stat.c](https://github.com/torvalds/linux/blob/v6.12/fs/stat.c#L524)
and [readlink(2)](https://man7.org/linux/man-pages/man2/readlink.2.html).

## Reproduce

Compile on Linux for a native reference, or use the guest musl cross compiler:

```sh
gcc -O2 -Wall -Wextra -Werror probe.c -o probe
./probe

aarch64-linux-musl-gcc -static -O2 -Wall -Wextra -Werror probe.c -o probe-aarch64
```

Copy `probe-aarch64` into an offline test rootfs, unmount it, boot the kernel
with that image, then run `/root/probe-aarch64` in the guest. Do not mount an
image while QEMU is using it. The probe creates and removes a unique temporary
directory. Exit status 0 means all assertions passed, 1 means an ABI mismatch,
and 2 means fixture/cleanup failure.

The 15 assertions cover zero/negative lengths, truncation of raw syscall
registers to `int`, lookup error ordering, short/exact/long positive buffers,
no appended NUL, and an invalid output pointer. On x86_64 the same checks also
exercise the legacy `readlink` syscall (30 assertions total).

## Validation

The kernel regression tests in `posix/fs/ut-tests/namei.rs` call the real syscall
entry with zero, negative, and minimum signed lengths and NULL user pointers.
They check that length validation returns `InvalidInput` before user access;
the x86_64 build also tests the legacy `readlink` entry. These tests are
registered with the existing unittest runner and run in Jenkins's kernel
unit-test stages. For a local AArch64 run:

```sh
cp platforms/kplat-aarch64/qemu_defconfig .config
make defconfig
make unittest UNITTEST_CRATE=posix_fs ACCEL=n VSOCK=n DISK_IMG=/path/to/test.img
```

The standalone probe below additionally exercises raw syscall argument
truncation, successful copying, and filesystem lookup errors.

- Linux x86_64 reference: 30/30 passed.
- AArch64 QEMU, upstream `1b62d8289c66b6ed75af30b1ab97b99d389cbc89`:
  8/15 assertions failed before this patch.
- Updated baseline `150b016fd656dcc171839a90fff7593cb05ef98d`:
  AArch64 kernel tests passed 39/39, including the new readlinkat regression.
- The static AArch64 probe passed 15/15 through a local Starry
  `ci-test-iter` guest case (4 CPUs, 1 GiB); its process exited with status 0.
- AArch64 and x86_64 `make build`, `make clippy`, and the pinned formatter
  check passed. The x86_64 guest was not run locally.

The external Starry Test Harness was at
`d4e61b9693a39b10876f784fdf3e9e112dca9ae8`. The local suite registered one
`kind = "guest"` case with `command = "/root/pr-probe"` and an `inject` entry
for the compiled probe. This was one targeted case, not the full Starry suite.
The kernel tests above provide automatic coverage in the existing CI job.
