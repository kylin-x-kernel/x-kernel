# inotify fdinfo lines: end-to-end regression

`/proc/<pid>/fdinfo/<fd>` must publish an inotify instance's watches in
Linux's field order and hex formatting. This probe drives the whole path in a
guest: create an instance, watch a real directory, read its own fdinfo file,
and check the rendered lines.

## Build and run

```sh
mkdir -p target/experiments/inotify-fdinfo
aarch64-linux-musl-gcc -std=c11 -O2 -Wall -Wextra -Werror -static -no-pie \
  docs/experiments/inotify-fdinfo/inotify-fdinfo-probe.c \
  -o target/experiments/inotify-fdinfo/inotify-fdinfo-probe
```

Copy the static binary into an AArch64 guest and run it there. It exits 0 when
every check passes and prints the fdinfo contents it read.

## Checks

| Check | Expectation |
| --- | --- |
| `fdinfo_readable` | The descriptor has an fdinfo entry |
| `has_pos_line` / `has_flags_line` / `has_mnt_id_line` / `has_ino_line` | Generic lines are present, in Linux's order and formats (`flags:` with the leading zero) |
| `has_inotify_wd_line` | `inotify wd:<wd>` in Linux's hex formatting |
| `has_mask_300` | The registered mask (`IN_CREATE\|IN_DELETE`) is rendered |
| `has_ignored_mask_0` | `ignored_mask:0` is rendered |
| `ino_is_not_placeholder` | The recorded inode came from path resolution, not a zero fill |
| `fdinfo_reports_excl_unlink_and_oneshot` | A watch asked with `IN_EXCL_UNLINK\|IN_ONESHOT` renders `mask:84000100`, the way `inotify_mark_user_mask()` ORs those two flags back |
| `fdinfo_after_rm` | After `inotify_rm_watch` the inotify lines are gone, generic lines stay |
| `fdinfo_gone_after_close` / `open_fdinfo_missing` | A closed (or never-open) descriptor has no entry |

## Recorded results

Run on 2026-10-25 against the branch that adds the fdinfo node, on QEMU
10.0.13 TCG (`virt,gic-version=3`, `cortex-a76`, four CPUs, 1 GiB) booting
this repository's Debian/OpenRC ext4 rootfs, probe built with AArch64 musl GCC
11.2.1. 18/18 checks pass, `failures: 0`, exit 0. The rendered content:

```
pos:	0
flags:	02004000
mnt_id:	6
ino:	2
inotify wd:1 ino:161 sdev:0 mask:300 ignored_mask:0
inotify wd:2 ino:1 sdev:0 mask:84000100 ignored_mask:0
```

The second line is the watch registered with `IN_CREATE|IN_EXCL_UNLINK|IN_ONESHOT`,
and it is the one worth reading twice: Linux keeps those two flags out of the
stored mask — `inotify_mark_user_mask()` intersects the mark with
`IN_ALL_EVENTS` and ORs the two back from the separate mark flags it tracks them
in. This kernel does the same: they live in the watch's `flags` field, not in
the stored mask, and the renderer ORs them back, so `84000100` is
`IN_CREATE (0x100) | IN_EXCL_UNLINK (0x0400_0000) | IN_ONESHOT (0x8000_0000)` —
the same value Linux prints for the same request.

Those lines are Linux's own set, in its order (`fs/proc/fd.c` prints
`pos/flags/mnt_id/ino`, with `flags:` octal behind a literal leading zero and
the descriptor's close-on-exec bit folded in; `fs/notify/fdinfo.c` prints the
inotify line, whose `ignored_mask` is hard-coded to 0 there too).

Honest notes on that output: `sdev` is the mount's synthetic device id, which is
0 for this rootfs rather than a fabricated block-device number; `pos` is the
file's real offset (`0` for a stream-like anonymous file whose reads do not
advance `f_pos`); and `ignored_mask` is always 0 because a removed watch is
dropped immediately instead of being parked in an ignored state. The trailing
`fhandle-*` fields from man 5 proc_pid_fdinfo are absent: they exist only for
file handles exported through an NFS file handle, which this kernel does not
produce.