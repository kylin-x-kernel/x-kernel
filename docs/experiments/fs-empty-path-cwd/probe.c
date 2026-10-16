// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/stat.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef SYS_faccessat2
#define SYS_faccessat2 439
#endif
#ifndef SYS_fchmodat2
#define SYS_fchmodat2 452
#endif
#ifndef SYS_newfstatat
#define SYS_newfstatat SYS_fstatat
#endif

static int checks, failures;

static void check(const char *name, int ok)
{
    ++checks;
    if (!ok)
        ++failures;
    printf("%s: %s (errno=%d)\n", ok ? "PASS" : "FAIL", name, errno);
}

#define EXPECT_ERR(name, call, error) do { \
    errno = 0; \
    long result = (call); \
    check(name, result == -1 && errno == (error)); \
} while (0)

int main(void)
{
    char directory[] = "/tmp/xk-empty-path-XXXXXX";
    struct stat expected, actual;
    struct statx extended;
    struct timespec times[2] = {{123456789, 0}, {123456790, 0}};
    struct timespec omit[2] = {{0, UTIME_OMIT}, {0, UTIME_OMIT}};
    if (!mkdtemp(directory) || chdir(directory) || stat(".", &expected)) {
        perror("setup");
        return 2;
    }
    int fd = open(".", O_RDONLY | O_DIRECTORY);
    int path_fd = open(".", O_PATH | O_DIRECTORY);
    if (fd < 0 || path_fd < 0) {
        perror("open cwd");
        return 2;
    }

    long rc = syscall(SYS_newfstatat, AT_FDCWD, "", &actual, AT_EMPTY_PATH);
    check("empty pathname identifies cwd", rc == 0 && actual.st_ino == expected.st_ino
          && actual.st_dev == expected.st_dev && S_ISDIR(actual.st_mode));
    check("statx empty pathname identifies cwd",
          syscall(SYS_statx, AT_FDCWD, "", AT_EMPTY_PATH, STATX_BASIC_STATS, &extended) == 0
          && extended.stx_ino == expected.st_ino && S_ISDIR(extended.stx_mode));
    check("empty pathname access checks cwd",
          syscall(SYS_faccessat2, AT_FDCWD, "", F_OK, AT_EMPTY_PATH) == 0);
    check("empty pathname chmod changes cwd",
          syscall(SYS_fchmodat2, AT_FDCWD, "", 0710, AT_EMPTY_PATH) == 0
          && stat(".", &actual) == 0 && (actual.st_mode & 0777) == 0710);
    check("empty pathname chown keeps cwd owner",
          syscall(SYS_fchownat, AT_FDCWD, "", -1, -1, AT_EMPTY_PATH) == 0
          && stat(".", &actual) == 0 && actual.st_uid == expected.st_uid
          && actual.st_gid == expected.st_gid);
    check("empty pathname timestamps update cwd",
          syscall(SYS_utimensat, AT_FDCWD, "", times, AT_EMPTY_PATH) == 0
          && stat(".", &actual) == 0 && actual.st_mtim.tv_sec == times[1].tv_sec);
    check("directory descriptor empty pathname still works",
          syscall(SYS_newfstatat, fd, "", &actual, AT_EMPTY_PATH) == 0
          && actual.st_ino == expected.st_ino);
    check("O_PATH directory empty pathname still works",
          syscall(SYS_newfstatat, path_fd, "", &actual, AT_EMPTY_PATH) == 0
          && actual.st_ino == expected.st_ino);
    EXPECT_ERR("empty pathname requires AT_EMPTY_PATH",
               syscall(SYS_newfstatat, AT_FDCWD, "", &actual, 0), ENOENT);
    EXPECT_ERR("invalid descriptor remains EBADF",
               syscall(SYS_newfstatat, -123, "", &actual, AT_EMPTY_PATH), EBADF);

    EXPECT_ERR("fstat rejects AT_FDCWD", syscall(SYS_fstat, AT_FDCWD, &actual), EBADF);
    EXPECT_ERR("fchmod rejects AT_FDCWD", syscall(SYS_fchmod, AT_FDCWD, 0777), EBADF);
    EXPECT_ERR("fchown rejects AT_FDCWD", syscall(SYS_fchown, AT_FDCWD, -1, -1), EBADF);
    EXPECT_ERR("flistxattr rejects AT_FDCWD", syscall(SYS_flistxattr, AT_FDCWD, NULL, 0), EBADF);
    check("utimensat ignores pathname when both timestamps omitted",
          syscall(SYS_utimensat, AT_FDCWD, NULL, omit, 0) == 0);
    check("utimensat NULL pathname still accepts a real descriptor",
          syscall(SYS_utimensat, fd, NULL, times, 0) == 0);
    check("fstat still accepts a real descriptor",
          syscall(SYS_fstat, fd, &actual) == 0 && actual.st_ino == expected.st_ino);
    check("fchmod still accepts a real descriptor", syscall(SYS_fchmod, fd, 0700) == 0);
    check("fchown still accepts a real descriptor", syscall(SYS_fchown, fd, -1, -1) == 0);

    close(path_fd);
    close(fd);
    if (chdir("/") || rmdir(directory)) {
        perror("cleanup");
        return 2;
    }
    printf("SUMMARY checks=%d failures=%d\n", checks, failures);
    return failures ? 1 : 0;
}
