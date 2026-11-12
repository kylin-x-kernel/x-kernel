# memfd-test — Design Document

## Positioning and Scope

This user-space regression enters the syscall directly through `SYS_memfd_create` and does not simulate the kernel's name validation.
It covers `ict/shared-buffer` and `/../buffer`. Before the fix, the kernel returned `EINVAL` for the first name containing `/`,
and the program exited non-zero; after the fix, it must also pass the subsequent object-usability checks.

## Flow and Execution Context

The program is a single-threaded Linux ABI user process and requires no root, procfs, or global temporary paths.
For each label it creates two fds. The second label replaces `/` with `:`, so that after formatting inside X-Kernel
it has the same name as the first. Each is `ftruncate`d to 4096 bytes. The first is mapped with `MAP_SHARED`,
written with known content, unmapped, and then verified through `pread`; the second must still return zero data.
It verifies that the syscall accepts the label, that the anonymous object is usable, and that backings with the same name do not share content.

This is not a check of the kernel's private mount topology, nor does it require `/proc/self/fd` to match
Linux's display name character for character. The kernel formatter unit assertions only verify the single-component format and do not replace this regression.

## Build and Run

Run on a configured platform with a separate test image:

```sh
make uapps UAPPS=memfd-test
# Run manually in the test guest:
/usr/local/bin/memfd-test
```

The manifest uses the platform-provided `CROSS_COMPILE`, links statically, and adds no new Cargo dependencies.
There is no autostart, so a successful installation must not be taken as a successful execution. The program passes only if it exits with 0 and prints
`memfd slash syscall regression: PASS`. The Linux reference and
X-Kernel before and after the fix must each have their source, binary, and actual exit code recorded.

## Resource Release

All failure paths converge on cleanup logic that releases the created mappings and the two fds; no global hooks,
threads, fork, or fixed file names are used. A cleanup failure likewise makes the program exit non-zero.
