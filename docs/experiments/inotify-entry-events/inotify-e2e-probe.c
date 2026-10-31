// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.
//
// End-to-end inotify regression: a real create/remove/rename sequence observed
// through the syscall ABI. It covers every directory-entry source the kernel
// wires -- creating open, mkdir, mknod, symlink, link, unlink, rmdir and rename
// -- through both absolute and relative path arguments (the relative case is
// what routes through the process pwd and the `dirfd` of an `*at` call).

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/inotify.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <unistd.h>

static int failures;

static void check(const char *what, int ok) {
    printf("{\"event\":\"check\",\"what\":\"%s\",\"status\":\"%s\"}\n", what,
           ok ? "pass" : "fail");
    if (!ok)
        failures++;
}

/* One read() can hand back several records, so the tail of a read is kept for
 * the next call: every check below looks at exactly one event. */
static char pending_buf[8192];
static size_t pending_len, pending_off;

struct event {
    int32_t wd;
    uint32_t mask;
    uint32_t cookie;
    char name[256];
    int present;
};

/* Reads the next record, or leaves `out->present` clear when none is queued. */
static void read_one(int fd, struct event *out) {
    out->present = 0;
    while (pending_off >= pending_len) {
        ssize_t n = read(fd, pending_buf, sizeof(pending_buf));
        if (n < (ssize_t)sizeof(struct inotify_event)) {
            pending_off = pending_len = 0;
            return;
        }
        pending_off = 0;
        pending_len = (size_t)n;
    }
    struct inotify_event ev;
    memcpy(&ev, pending_buf + pending_off, sizeof(ev));
    out->wd = ev.wd;
    out->mask = ev.mask;
    out->cookie = ev.cookie;
    size_t len = ev.len < sizeof(out->name) ? ev.len : sizeof(out->name) - 1;
    memcpy(out->name, pending_buf + pending_off + sizeof(ev), len);
    /* `len` counts the NUL and its padding, so the name is NUL-terminated by
     * truncating at the first one. */
    out->name[len] = '\0';
    if (len && out->name[len - 1] != '\0') {
        size_t end = strnlen(out->name, sizeof(out->name));
        out->name[end] = '\0';
    }
    out->present = 1;
    pending_off += sizeof(ev) + ev.len;
}

static void drain(int fd) {
    pending_off = pending_len = 0;
    char scratch[8192];
    while (read(fd, scratch, sizeof(scratch)) > 0)
        ;
}

/* Removes leftovers from an interrupted run. Without this the first create
 * would be an ordinary open of an existing file, which must *not* report
 * IN_CREATE, and the probe would fail on its own debris. */
static void clean_scratch(void) {
    static const char *files[] = {
        "/root/inotify-e2e/file-abs",       "/root/inotify-e2e/file-rel",
        "/root/inotify-e2e/link-rel",       "/root/inotify-e2e/hard-rel",
        "/root/inotify-e2e/node-at",        "/root/inotify-e2e/file-renamed",
        "/root/inotify-e2e/moved-out",      "/root/inotify-e2e-elsewhere/moved-in",
        "/root/inotify-e2e/gone/file-after-recreate",
        "/root/inotify-e2e/busy/keep",      "/root/inotify-e2e/busy/second",
        "/root/inotify-e2e/inner-renamed/file-inside",
    };
    static const char *dirs[] = {
        "/root/inotify-e2e/busy",            "/root/inotify-e2e/gone",
        "/root/inotify-e2e/inner-renamed",
        "/root/inotify-e2e/inner",
        "/root/inotify-e2e/sub",            "/root/inotify-e2e/subdir",
        "/root/inotify-e2e/subdir-renamed", "/root/inotify-e2e",
        "/root/inotify-e2e-elsewhere",
    };
    for (size_t i = 0; i < sizeof(files) / sizeof(files[0]); i++)
        unlink(files[i]);
    for (size_t i = 0; i < sizeof(dirs) / sizeof(dirs[0]); i++)
        rmdir(dirs[i]);
}

int main(void) {
    const char *dir = "/root/inotify-e2e";
    const char *elsewhere = "/root/inotify-e2e-elsewhere";
    char path[160];
    struct event ev;

    clean_scratch();
    mkdir(dir, 0755);
    mkdir(elsewhere, 0755);
    snprintf(path, sizeof(path), "%s/file-abs", dir);

    int fd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    check("inotify_init1", fd >= 0);
    if (fd < 0)
        return 1;

    const uint32_t watched =
        IN_CREATE | IN_DELETE | IN_MOVED_FROM | IN_MOVED_TO;
    int wd = inotify_add_watch(fd, dir, watched);
    check("inotify_add_watch", wd >= 0);
    int wd_elsewhere = inotify_add_watch(fd, elsewhere, watched);
    check("inotify_add_watch_second_dir", wd_elsewhere >= 0 && wd_elsewhere != wd);

    /* Absolute-path create: the parent comes out of the path itself. */
    int f = open(path, O_CREAT | O_WRONLY, 0644);
    check("create_absolute", f >= 0);
    if (f >= 0)
        close(f);

    int rfd = dup(fd); /* FIONREAD must answer on the same instance */
    int pending = -1;
    check("fionread_reports_pending",
          ioctl(rfd, FIONREAD, &pending) == 0 && pending > 0);
    close(rfd);

    read_one(fd, &ev);
    check("absolute_create_event", ev.present && ev.mask == IN_CREATE &&
                                       ev.wd == wd &&
                                       strcmp(ev.name, "file-abs") == 0);

    /* Relative-path create after chdir: routed through the process pwd. */
    check("chdir", chdir(dir) == 0);
    f = open("file-rel", O_CREAT | O_WRONLY, 0644);
    check("create_relative", f >= 0);
    if (f >= 0)
        close(f);
    read_one(fd, &ev);
    check("relative_create_event", ev.present && ev.mask == IN_CREATE &&
                                       ev.wd == wd &&
                                       strcmp(ev.name, "file-rel") == 0);

    /* Relative unlink: IN_DELETE, reported only after the removal succeeded. */
    check("unlink_relative", unlink("file-rel") == 0);
    read_one(fd, &ev);
    check("relative_delete_event", ev.present && ev.mask == IN_DELETE &&
                                       ev.wd == wd &&
                                       strcmp(ev.name, "file-rel") == 0);

    /* Unsubscribed bits stay silent: IN_ATTRIB was not in the watch mask. */
    check("chmod_silent", chmod(path, 0600) == 0);
    read_one(fd, &ev);
    check("no_unsubscribed_event", !ev.present);

    /* mkdir: a directory creation carries IN_ISDIR. */
    check("mkdir_relative", mkdir("sub", 0755) == 0);
    read_one(fd, &ev);
    check("mkdir_carries_isdir", ev.present &&
                                     ev.mask == (IN_CREATE | IN_ISDIR) &&
                                     ev.wd == wd && strcmp(ev.name, "sub") == 0);

    /* symlink: a new directory entry, reported in the directory that holds it. */
    check("symlink_relative", symlink("file-abs", "link-rel") == 0);
    read_one(fd, &ev);
    check("symlink_create_event", ev.present && ev.mask == IN_CREATE &&
                                      ev.wd == wd &&
                                      strcmp(ev.name, "link-rel") == 0);

    /* link: the hard link is a new entry in *this* directory. */
    check("link_hard", link("file-abs", "hard-rel") == 0);
    read_one(fd, &ev);
    check("link_create_event", ev.present && ev.mask == IN_CREATE &&
                                   ev.wd == wd &&
                                   strcmp(ev.name, "hard-rel") == 0);

    /* mknod with dirfd-relative arguments: an `*at` call routes through the
     * directory descriptor. */
    int dfd = open(".", O_RDONLY | O_DIRECTORY);
    check("open_dir_fd", dfd >= 0);
    check("mknodat_relative", mknodat(dfd, "node-at", S_IFIFO | 0644, 0) == 0);
    read_one(fd, &ev);
    check("mknod_create_event", ev.present && ev.mask == IN_CREATE &&
                                    ev.wd == wd && strcmp(ev.name, "node-at") == 0);
    if (dfd >= 0)
        close(dfd);

    /* A rename inside one watched directory: both halves, one cookie. */
    check("rename_in_place", rename("file-abs", "file-renamed") == 0);
    read_one(fd, &ev);
    uint32_t cookie = ev.cookie;
    check("rename_reports_moved_from",
          ev.present && ev.mask == IN_MOVED_FROM && ev.wd == wd &&
              strcmp(ev.name, "file-abs") == 0);
    read_one(fd, &ev);
    check("rename_reports_moved_to", ev.present && ev.mask == IN_MOVED_TO &&
                                         ev.wd == wd && ev.cookie == cookie &&
                                         strcmp(ev.name, "file-renamed") == 0);
    check("rename_cookie_is_nonzero", cookie != 0);

    /* A rename that moves nothing reports nothing: Linux's vfs_rename()
     * returns before fsnotify_move() when source and target are one inode, so
     * the watcher must not see a phantom move pair. */
    drain(fd);
    check("rename_same_name", rename("file-renamed", "file-renamed") == 0);
    read_one(fd, &ev);
    check("no_event_for_noop_rename", !ev.present);

    /* A rename of a directory carries IN_ISDIR on both halves. */
    check("mkdir_for_rename", mkdir("subdir", 0755) == 0);
    drain(fd);
    check("rename_directory", rename("subdir", "subdir-renamed") == 0);
    read_one(fd, &ev);
    uint32_t dir_cookie = ev.cookie;
    check("rename_dir_moved_from",
          ev.present && ev.mask == (IN_MOVED_FROM | IN_ISDIR) &&
              strcmp(ev.name, "subdir") == 0);
    read_one(fd, &ev);
    check("rename_dir_moved_to", ev.present &&
                                     ev.mask == (IN_MOVED_TO | IN_ISDIR) &&
                                     ev.cookie == dir_cookie &&
                                     strcmp(ev.name, "subdir-renamed") == 0);

    /* A cross-directory move: each watched directory sees its own half, and the
     * two halves still share one cookie. */
    snprintf(path, sizeof(path), "%s/moved-out", dir);
    f = open(path, O_CREAT | O_WRONLY, 0644);
    if (f >= 0)
        close(f);
    drain(fd);
    snprintf(path, sizeof(path), "%s/moved-in", elsewhere);
    check("rename_across_directories", rename("moved-out", path) == 0);
    read_one(fd, &ev);
    check("cross_dir_moved_from", ev.present && ev.mask == IN_MOVED_FROM &&
                                      ev.wd == wd &&
                                      strcmp(ev.name, "moved-out") == 0);
    uint32_t cross_cookie = ev.cookie;
    read_one(fd, &ev);
    check("cross_dir_moved_to", ev.present && ev.mask == IN_MOVED_TO &&
                                    ev.wd == wd_elsewhere &&
                                    ev.cookie == cross_cookie &&
                                    strcmp(ev.name, "moved-in") == 0);

    /* rmdir: a directory removal carries IN_ISDIR too. */
    check("rmdir_relative", rmdir("sub") == 0);
    read_one(fd, &ev);
    check("rmdir_carries_isdir", ev.present &&
                                     ev.mask == (IN_DELETE | IN_ISDIR) &&
                                     strcmp(ev.name, "sub") == 0);

    /* A removal that fails must retire nothing: rmdir on a non-empty directory
     * returns ENOTEMPTY, and its watch has to keep working afterwards. */
    check("mkdir_busy", mkdir("busy", 0755) == 0);
    drain(fd);
    int wd_busy = inotify_add_watch(fd, "busy", IN_CREATE | IN_DELETE);
    check("add_watch_busy", wd_busy >= 0);
    f = open("busy/keep", O_CREAT | O_WRONLY, 0644);
    check("create_keep", f >= 0);
    if (f >= 0)
        close(f);
    drain(fd);
    errno = 0;
    check("rmdir_non_empty_fails", rmdir("busy") == -1 && errno == ENOTEMPTY);
    f = open("busy/second", O_CREAT | O_WRONLY, 0644);
    check("create_second", f >= 0);
    if (f >= 0)
        close(f);
    read_one(fd, &ev);
    check("watch_survives_failed_removal",
          ev.present && ev.wd == wd_busy && ev.mask == IN_CREATE &&
              strcmp(ev.name, "second") == 0);
    check("rm_watch_busy", inotify_rm_watch(fd, wd_busy) == 0);
    drain(fd);
    unlink("busy/keep");
    unlink("busy/second");
    rmdir("busy");
    drain(fd);

    /* A watched directory keeps its watch when the directory itself is
     * renamed: a watch names the object, so events keep arriving on the same
     * descriptor under the new spelling. A table keyed by path text goes
     * silent here -- this is the check that tells the two apart. */
    check("mkdir_inner", mkdir("inner", 0755) == 0);
    drain(fd); /* the parent's IN_CREATE for `inner` */
    int wd_inner = inotify_add_watch(fd, "inner", IN_CREATE | IN_DELETE);
    check("add_watch_inner", wd_inner >= 0 && wd_inner != wd);
    check("rename_inner", rename("inner", "inner-renamed") == 0);
    drain(fd);
    f = open("inner-renamed/file-inside", O_CREAT | O_WRONLY, 0644);
    check("create_inside_renamed", f >= 0);
    if (f >= 0)
        close(f);
    read_one(fd, &ev);
    check("watch_survives_directory_rename",
          ev.present && ev.mask == IN_CREATE && ev.wd == wd_inner &&
              strcmp(ev.name, "file-inside") == 0);
    // Removing that watch reports IN_IGNORED for it, and only for it.
    check("rm_watch_inner", inotify_rm_watch(fd, wd_inner) == 0);
    read_one(fd, &ev);
    check("inner_ignored_event", ev.present && ev.mask == IN_IGNORED &&
                                     ev.wd == wd_inner);
    unlink("inner-renamed/file-inside");
    rmdir("inner-renamed");
    // That cleanup is itself two IN_DELETE records for the outer watch, which
    // the checks below are not counting.
    drain(fd);

    /* A watched directory that is removed retires its watch: it reports
     * IN_DELETE_SELF and IN_IGNORED and ceases to exist. The name is then
     * recreated -- which in this filesystem reuses the inode number -- and
     * the retired watch must stay retired rather than adopt the new object. */
    check("mkdir_gone", mkdir("gone", 0755) == 0);
    drain(fd);
    int wd_gone = inotify_add_watch(fd, "gone", IN_ALL_EVENTS);
    check("add_watch_gone", wd_gone >= 0 && wd_gone != wd);
    check("rmdir_gone", rmdir("gone") == 0);
    /* Linux's observable order: the directory's IN_DELETE lands first, the
     * dying object's FS_DELETE_SELF and IN_IGNORED follow (Linux delivers
     * them when the inode is evicted). */
    read_one(fd, &ev);
    check("removal_reports_parent_delete_first",
          ev.present && ev.mask == (IN_DELETE | IN_ISDIR) && ev.wd == wd);
    read_one(fd, &ev);
    /* The watched object is a directory, and Linux's fsnotify_inode() sets
     * FS_ISDIR on every inode event whose inode is one. */
    check("removal_reports_delete_self",
          ev.present && ev.mask == (IN_DELETE_SELF | IN_ISDIR) && ev.wd == wd_gone);
    read_one(fd, &ev);
    check("removal_reports_ignored",
          ev.present && ev.mask == IN_IGNORED && ev.wd == wd_gone);
    drain(fd);
    /* The retired descriptor is gone, not merely silent: the kernel reports it
     * as an invalid watch, so no later object can inherit it. */
    errno = 0;
    check("retired_descriptor_is_gone",
          inotify_rm_watch(fd, wd_gone) == -1 && errno == EINVAL);
    check("recreate_gone", mkdir("gone", 0755) == 0);
    drain(fd);
    int wd_new = inotify_add_watch(fd, "gone", IN_CREATE);
    check("add_watch_recreated", wd_new >= 0 && wd_new != wd_gone);
    f = open("gone/file-after-recreate", O_CREAT | O_WRONLY, 0644);
    check("create_after_recreate", f >= 0);
    if (f >= 0)
        close(f);
    read_one(fd, &ev);
    check("recreated_directory_reports", ev.present && ev.wd == wd_new &&
                                             ev.mask == IN_CREATE &&
                                             strcmp(ev.name, "file-after-recreate") == 0);
    check("rm_watch_recreated", inotify_rm_watch(fd, wd_new) == 0);
    read_one(fd, &ev);
    check("recreated_ignored_event", ev.present && ev.mask == IN_IGNORED &&
                                         ev.wd == wd_new);
    unlink("gone/file-after-recreate");
    rmdir("gone");
    drain(fd);

    /* Watch removal reports IN_IGNORED with no name. */
    check("rm_watch", inotify_rm_watch(fd, wd) == 0);
    read_one(fd, &ev);
    check("ignored_event", ev.present && ev.mask == IN_IGNORED && ev.wd == wd &&
                               ev.name[0] == '\0');

    /* The second watch is untouched by the first one's removal. */
    check("rm_watch_second", inotify_rm_watch(fd, wd_elsewhere) == 0);
    read_one(fd, &ev);
    check("second_ignored_event", ev.present && ev.mask == IN_IGNORED &&
                                      ev.wd == wd_elsewhere);

    unlink("/root/inotify-e2e/hard-rel");
    unlink("/root/inotify-e2e/link-rel");
    unlink("/root/inotify-e2e/node-at");
    unlink("/root/inotify-e2e/file-renamed");
    rmdir("/root/inotify-e2e/subdir-renamed");
    unlink("/root/inotify-e2e-elsewhere/moved-in");
    rmdir(dir);
    rmdir(elsewhere);
    close(fd);

    printf("{\"event\":\"summary\",\"failures\":%d}\n", failures);
    return failures ? 1 : 0;
}