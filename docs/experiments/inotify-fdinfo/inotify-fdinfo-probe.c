// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.
//
// End-to-end fdinfo regression: an inotify instance must publish its watches
// in Linux's `/proc/<pid>/fdinfo/<fd>` format. Running as the only probe in
// the guest shell keeps `/proc/self/fdinfo` unambiguous.

#define _GNU_SOURCE
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/inotify.h>
#include <sys/stat.h>
#include <unistd.h>

static int failures;

static void check(const char *what, int ok) {
    printf("{\"event\":\"check\",\"what\":\"%s\",\"status\":\"%s\"}\n", what,
           ok ? "pass" : "fail");
    if (!ok)
        failures++;
}

/* Reads the whole fdinfo file for `fd` into buf; returns bytes read or -1. */
static ssize_t read_fdinfo(int fd, char *buf, size_t cap) {
    char path[64];
    snprintf(path, sizeof(path), "/proc/self/fdinfo/%d", fd);
    int f = open(path, O_RDONLY);
    if (f < 0)
        return -1;
    ssize_t n = read(f, buf, cap - 1);
    close(f);
    if (n < 0)
        return -1;
    buf[n] = '\0';
    return n;
}

int main(void) {
    int fd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    check("inotify_init1", fd >= 0);
    if (fd < 0)
        return 1;

    int wd = inotify_add_watch(fd, "/root", IN_CREATE | IN_DELETE);
    check("inotify_add_watch", wd >= 0);

    char buf[4096];
    ssize_t n = read_fdinfo(fd, buf, sizeof(buf));
    check("fdinfo_readable", n > 0);
    if (n <= 0)
        return 1;

    printf("---- fdinfo ----\n%s---- end ----\n", buf);

    check("has_pos_line", strstr(buf, "pos:") != NULL);
    check("has_flags_line", strstr(buf, "flags:\t0") != NULL);
    check("has_mnt_id_line", strstr(buf, "mnt_id:") != NULL);
    check("has_ino_line", strstr(buf, "ino:\t") != NULL);

    /* Linux renders: inotify wd:<wd hex> ino:<hex> sdev:<hex> mask:<hex> ignored:<hex> */
    char expect[64];
    snprintf(expect, sizeof(expect), "inotify wd:%x ino:", wd);
    check("has_inotify_wd_line", strstr(buf, expect) != NULL);
    check("has_mask_300", strstr(buf, "mask:300") != NULL); /* IN_CREATE|IN_DELETE */
    check("has_ignored_mask_0", strstr(buf, "ignored_mask:0") != NULL);

    /* The recorded inode must be the watched directory's real one, not a
     * placeholder. The inotify line renders it in hex (Linux does too); the
     * generic "ino:" line checked above is decimal and belongs to the fd
     * itself. Compare the parsed value against stat(2) of the watch target. */
    const char *wline = strstr(buf, expect);
    unsigned long long reported =
        wline != NULL ? strtoull(wline + strlen(expect), NULL, 16) : 0;
    struct stat dir_st;
    check("ino_is_not_placeholder",
          wline != NULL && stat("/root", &dir_st) == 0 &&
              reported == (unsigned long long)dir_st.st_ino && reported != 0);

    /* A watch that asked for IN_EXCL_UNLINK|IN_ONESHOT reports them back:
     * Linux's `inotify_mark_user_mask()` ORs those two flags into the display
     * mask from the separate mark flags it keeps them in. */
    int wd_flags = inotify_add_watch(fd, "/tmp", IN_CREATE | IN_EXCL_UNLINK | IN_ONESHOT);
    check("add_watch_flags", wd_flags >= 0);
    n = read_fdinfo(fd, buf, sizeof(buf));
    check("fdinfo_reports_excl_unlink_and_oneshot",
          n > 0 && strstr(buf, "mask:84000100") != NULL);
    check("rm_watch_flags", inotify_rm_watch(fd, wd_flags) == 0);

    /* Removing the watch empties the inotify line set; the generic lines stay. */
    check("rm_watch", inotify_rm_watch(fd, wd) == 0);
    n = read_fdinfo(fd, buf, sizeof(buf));
    check("fdinfo_after_rm", n > 0 && strstr(buf, "inotify wd:") == NULL &&
                                strstr(buf, "flags:") != NULL);

    close(fd);

    /* A closed descriptor has no fdinfo entry. */
    check("fdinfo_gone_after_close", read_fdinfo(fd, buf, sizeof(buf)) < 0);
    check("open_fdinfo_missing", access("/proc/self/fdinfo/9999", F_OK) != 0);

    printf("{\"event\":\"summary\",\"failures\":%d}\n", failures);
    return failures ? 1 : 0;
}