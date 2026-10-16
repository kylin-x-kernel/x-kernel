# fchdir descriptor-validation regression

`fchdir` accepts an actual directory fd, including an `O_PATH` directory fd.
`AT_FDCWD` is a sentinel for pathname-relative interfaces, not a descriptor:
`fchdir(AT_FDCWD)` must fail with `EBADF`. Errors must preserve the cwd.

References: [Linux v6.12 fs/open.c](https://github.com/torvalds/linux/blob/v6.12/fs/open.c#L581)
and [chdir(2)](https://man7.org/linux/man-pages/man2/chdir.2.html).

## Reproduce

```sh
gcc -O2 -Wall -Wextra -Werror probe.c -o probe
./probe

aarch64-linux-musl-gcc -static -O2 -Wall -Wextra -Werror probe.c -o probe-aarch64
```

Copy the cross-compiled binary to `/root/probe-aarch64` in an offline test
rootfs, unmount it, boot the kernel, and run that path in the guest. The probe
uses a unique temporary directory and restores its original cwd. Exit status
0 means all assertions passed, 1 means an ABI mismatch, and 2 means fixture
or cleanup failure.

The eight assertions cover the sentinel, negative/closed fds, regular files,
`O_PATH` regular files, ordinary/`O_PATH` directory success, and search
permission denial. Error cases also verify unchanged cwd. The permission
check runs in a child, dropping root to uid 65534 when necessary, so it cannot
change the parent credentials. The child compares cwd before and after the
rejected call and preserves errno before reading cwd. A child fixture failure
is reported as exit status 2, not an ABI assertion failure. The parent attempts
all cleanup steps even when setup, the child, or an earlier cleanup fails.

## Validation

The probe is also run as the `fchdir-abi` case in a local `ci-test-iter`
workspace with Starry Test Harness `d4e61b9693a39b10876f784fdf3e9e112dca9ae8`.
The case injects the static probe and checks its exit status.

On upstream `540d07bb22b5c5cf7aae46f1e20dbb3c1c292bf3` plus this patch:

- Linux x86_64 and QEMU AArch64: 8/8 assertions passed.
- AArch64 `make build`, `make clippy`, and the pinned formatter passed.
- A Linux fault-injection run made `getcwd` fail only in the child. The probe
  returned 2 and removed its temporary directory.

The original upstream `1b62d828` failed the `AT_FDCWD` assertion. These are
focused guest and fixture tests, not the full Starry suite. An x86_64 guest
was not run.
