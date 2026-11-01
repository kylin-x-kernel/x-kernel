# inotify directory-entry events: end-to-end regression

This standalone C probe drives a real create/remove/rename sequence through the
syscall ABI and asserts the records `read` returns. It exists because the
in-kernel unit tests cover routing and record layout, but not the full chain
from a VFS hook to bytes in a user buffer.

## Build and run

```sh
mkdir -p target/experiments/inotify-entry-events
aarch64-linux-musl-gcc -std=c11 -O2 -Wall -Wextra -Werror -static -no-pie \
  docs/experiments/inotify-entry-events/inotify-e2e-probe.c \
  -o target/experiments/inotify-entry-events/inotify-e2e-probe
```

Copy the static binary into an AArch64 guest and run it there:

```sh
./inotify-e2e-probe
```

It exits 0 when every check passes, prints one JSON `check` line per step and a
final `summary`, and requires a writable `/root`. It removes its own scratch
tree first, so a run interrupted by a boot timeout does not turn the next one's
first create into an ordinary open of an existing file.

## Checks

| Check | Expectation |
| --- | --- |
| `inotify_init1` / `inotify_add_watch` | Real descriptor and watch descriptors, distinct per directory |
| `fionread_reports_pending` | `FIONREAD` reports a positive drain size before the first read |
| `absolute_create_event` | Creating `/root/inotify-e2e/file-abs` yields `IN_CREATE` with the watch's wd and name |
| `relative_create_event` | After `chdir`, creating `file-rel` yields `IN_CREATE` (this routes through the process pwd) |
| `relative_delete_event` | `unlink("file-rel")` yields `IN_DELETE` |
| `relative_watch_is_the_same_watch` | Re-registering with `"."` returns the descriptor the absolute call returned: one watch per directory, not per spelling |
| `relative_watch_relative_trigger` / `relative_watch_absolute_trigger` | A watch registered as `"."` receives creates triggered by both spellings (the registry key is the resolved path) |
| `no_unsubscribed_event` | `chmod` (IN_ATTRIB, not in the watch mask) queues nothing |
| `mkdir_carries_isdir` | `mkdir("sub")` yields `IN_CREATE\|IN_ISDIR` |
| `symlink_create_event` / `link_create_event` | `symlink` and `link` each yield `IN_CREATE` for the new entry |
| `mknod_create_event` | `mknodat` on a directory descriptor yields `IN_CREATE` |
| `rename_reports_moved_from` / `rename_reports_moved_to` | A rename inside one directory yields both halves, in order, with equal non-zero cookies |
| `rename_dir_moved_from` / `rename_dir_moved_to` | A renamed directory carries `IN_ISDIR` on both halves |
| `cross_dir_moved_from` / `cross_dir_moved_to` | A cross-directory move reports each half to its own watch, still sharing one cookie |
| `rmdir_non_empty_fails` / `watch_survives_failed_removal` | A removal that fails (`ENOTEMPTY`) retires nothing: the directory's watch still reports the next create |
| `removal_reports_delete_self` / `removal_reports_ignored` | Removing a watched directory reports `IN_DELETE_SELF` and then `IN_IGNORED` to that watch |
| `retired_descriptor_is_gone` / `add_watch_recreated` | The retired descriptor is invalid (`EINVAL` from `inotify_rm_watch`), and the recreated directory needs a fresh watch — a recycled inode number does not inherit the old one |
| `recreated_directory_reports` | The fresh watch on the recreated directory reports its entries |
| `watch_survives_directory_rename` | A watch on a directory keeps firing on the same descriptor after that directory is renamed — the table is keyed by the watched object, not by a path |
| `rmdir_carries_isdir` | `rmdir("sub")` yields `IN_DELETE\|IN_ISDIR` |
| `inner_ignored_event` | Removing the watch added on the renamed directory reports `IN_IGNORED` for exactly that descriptor |
| `ignored_event` / `second_ignored_event` | `inotify_rm_watch` yields a nameless `IN_IGNORED` for exactly that wd |

## Recorded results

Run on 2026-10-26 against the branch that adds the VFS event sources, on QEMU
10.0.13 TCG (`virt,gic-version=3`, `cortex-a76`, four CPUs, 1 GiB) booting this
repository's Debian/OpenRC ext4 rootfs, probe built with AArch64 musl GCC 11.2.1:
38 checks, 0 failures, exit 0.

```
{"event":"summary","failures":0}
PROBE_EXIT=0
```

One earlier run on a rootfs that still held a `file-abs` from an interrupted
sequence is worth recording: the create check passed but no `IN_CREATE` was
reported, which is correct — `O_CREAT` on an existing file is an ordinary open,
and a watcher must not be told a creation happened that the filesystem never
performed. The probe now clears its scratch tree first, so that state cannot
leak into a run.