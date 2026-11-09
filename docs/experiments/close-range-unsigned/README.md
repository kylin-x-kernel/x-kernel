# Unsigned close_range bounds

`close_range` takes unsigned 32-bit bounds on Linux. Before this fix,
`close_range(fd, UINT_MAX, 0)` and `close_range(fd, UINT_MAX,
CLOSE_RANGE_CLOEXEC)` returned `EINVAL` because the upper bound was interpreted
as signed `-1`. This prevents closing or marking the tail of a descriptor table.

The standalone probe exercises both operations and an empty range at `UINT_MAX`.
It reports the return value, errno, descriptor state and an aggregate JSON result.
It uses ordinary Linux syscalls and no kernel-internal assumptions.

```sh
gcc -std=c11 -O2 -Wall -Wextra -Werror close-range-upper.c -o close-range-native
./close-range-native
aarch64-linux-gnu-gcc -std=c11 -O2 -Wall -Wextra -Werror -static \
  close-range-upper.c -o close-range-aarch64
# Copy the AArch64 executable into the guest and run it there.
```

Expected: three passing subtests and exit status zero. Use the same AArch64 ELF
for before/after kernel comparison. A native Linux control is a separately built
ELF, not an AArch64 system reference or performance result.

References: Linux v6.12 `fs/open.c` (`SYSCALL_DEFINE3(close_range)`) and `fs/file.c`
(`__close_range`). The syscall validates unsigned ordering and flags, applies
`UNSHARE`, then operates on the requested range. X-Kernel narrows bounds only
after validation and preserves unsharing even for an empty descriptor domain.
