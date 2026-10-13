// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

static int checks, failures;
static void check(const char *name, int ok)
{
    ++checks;
    failures += !ok;
    printf("%s %s\n", ok ? "PASS" : "FAIL", name);
}

static long call_readlink(long nr, const char *path, void *buf, unsigned long size)
{
    if (nr == SYS_readlinkat)
        return syscall(nr, AT_FDCWD, path, buf, size);
    return syscall(nr, path, buf, size);
}

static void run(long nr)
{
    static const char target[] = "target-value";
    char buf[64];
    const unsigned long sizes[] = {0, (unsigned long)-1, 0x80000000UL, 0xffffffffUL,
                                  0x100000000UL};
    printf("syscall=%ld\n", nr);
    for (size_t i = 0; i < sizeof(sizes) / sizeof(sizes[0]); ++i) {
        memset(buf, '!', sizeof(buf));
        errno = 0;
        long ret = call_readlink(nr, "link", buf, sizes[i]);
        printf("size=%#lx ret=%ld errno=%d\n", sizes[i], ret, errno);
        check("non-positive signed-int size returns EINVAL without writing",
              ret == -1 && errno == EINVAL && buf[0] == '!');
    }
    const unsigned long valid_sizes[] = {sizeof(buf), sizeof(target)-1, 3, 0x100000003UL};
    for (size_t i = 0; i < sizeof(valid_sizes) / sizeof(valid_sizes[0]); ++i) {
        memset(buf, '!', sizeof(buf));
        long ret = call_readlink(nr, "link", buf, valid_sizes[i]);
        size_t expected = (unsigned int)valid_sizes[i];
        if (expected > sizeof(target)-1) expected = sizeof(target)-1;
        printf("size=%#lx ret=%ld\n", valid_sizes[i], ret);
        check("positive size truncates target without appending NUL",
              ret == (long)expected && !memcmp(buf, target, expected) && buf[expected] == '!');
    }
    const char *paths[] = {NULL, "missing", "regular"};
    for (size_t i = 0; i < sizeof(paths) / sizeof(paths[0]); ++i) {
        errno = 0;
        long ret = call_readlink(nr, paths[i], buf, 0);
        check("zero size is rejected before pathname lookup", ret == -1 && errno == EINVAL);
    }
    errno = 0;
    long ret = call_readlink(nr, "missing", buf, sizeof(buf));
    check("positive size with missing path returns ENOENT", ret == -1 && errno == ENOENT);
    errno = 0;
    ret = call_readlink(nr, "regular", buf, sizeof(buf));
    check("positive size with non-symlink returns EINVAL", ret == -1 && errno == EINVAL);
    errno = 0;
    ret = call_readlink(nr, "link", NULL, sizeof(buf));
    check("positive size with NULL output returns EFAULT", ret == -1 && errno == EFAULT);
}

int main(void)
{
    char dir[] = "/tmp/readlink-abi-XXXXXX";
    int saved = open(".", O_RDONLY | O_DIRECTORY);
    if (saved < 0 || !mkdtemp(dir) || chdir(dir)) { perror("fixture"); return 2; }
    int fd = open("regular", O_CREAT | O_EXCL | O_WRONLY, 0600);
    if (fd < 0 || symlink("target-value", "link")) { perror("fixture files"); return 2; }
    close(fd);
    run(SYS_readlinkat);
#ifdef SYS_readlink
    run(SYS_readlink);
#endif
    int cleanup_failed = 0;
    if (unlink("link")) { perror("unlink link"); cleanup_failed = 1; }
    if (unlink("regular")) { perror("unlink regular"); cleanup_failed = 1; }
    if (fchdir(saved)) { perror("restore cwd"); cleanup_failed = 1; }
    if (close(saved)) { perror("close saved cwd"); cleanup_failed = 1; }
    if (rmdir(dir)) { perror("remove fixture"); cleanup_failed = 1; }
    if (cleanup_failed) return 2;
    printf("RESULT checks=%d failures=%d\n", checks, failures);
    return failures != 0;
}
