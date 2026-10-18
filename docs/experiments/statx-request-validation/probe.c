// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/stat.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

static unsigned checks, failures;

static void expect_error(const char *name, int fd, const char *path,
                         unsigned flags, unsigned mask, int expected_errno)
{
    struct statx result, sentinel;
    memset(&result, 0xa5, sizeof(result));
    sentinel = result;
    errno = 0;
    long rc = syscall(SYS_statx, fd, path, flags, mask, &result);
    int got_errno = errno;
    int ok = rc == -1 && got_errno == expected_errno && !memcmp(&result, &sentinel, sizeof(result));
    ++checks;
    failures += !ok;
    printf("%s: %s rc=%ld errno=%d output_unchanged=%d\n",
           ok ? "PASS" : "FAIL", name, rc, got_errno, !memcmp(&result, &sentinel, sizeof(result)));
}

static void expect_invalid(const char *name, int fd, const char *path,
                           unsigned flags, unsigned mask)
{
    expect_error(name, fd, path, flags, mask, EINVAL);
}

static void expect_valid(const char *name, int fd, const char *path,
                         unsigned flags, unsigned mask)
{
    struct statx result = {0};
    errno = 0;
    long rc = syscall(SYS_statx, fd, path, flags, mask, &result);
    int ok = rc == 0;
    ++checks;
    failures += !ok;
    printf("%s: %s rc=%ld errno=%d\n", ok ? "PASS" : "FAIL", name, rc, errno);
}

int main(void)
{
    int fd = open("/", O_PATH | O_DIRECTORY);
    if (fd < 0) {
        perror("open root");
        return 2;
    }
    expect_valid("normal basic metadata", AT_FDCWD, "/", 0, STATX_BASIC_STATS);
    expect_valid("zero request mask", AT_FDCWD, "/", 0, 0);
    expect_valid("unknown nonreserved mask bits remain forward compatible",
                 AT_FDCWD, "/", 0, 0x40000000);
    expect_valid("force sync alone", AT_FDCWD, "/", AT_STATX_FORCE_SYNC, STATX_BASIC_STATS);
    expect_valid("dont sync alone", AT_FDCWD, "/", AT_STATX_DONT_SYNC, STATX_BASIC_STATS);
    expect_valid("real descriptor empty pathname", fd, "", AT_EMPTY_PATH, STATX_BASIC_STATS);

    expect_invalid("reserved mask bit", AT_FDCWD, "/", 0, STATX__RESERVED);
    expect_invalid("reserved mask with basic fields", AT_FDCWD, "/", 0,
                   STATX__RESERVED | STATX_BASIC_STATS);
    expect_invalid("all mask bits are not STATX_ALL", AT_FDCWD, "/", 0, ~0u);
    expect_invalid("conflicting sync modes", AT_FDCWD, "/", AT_STATX_SYNC_TYPE, STATX_BASIC_STATS);
    expect_invalid("reserved mask also rejected for descriptor", fd, "", AT_EMPTY_PATH,
                   STATX__RESERVED);
    expect_invalid("conflicting sync modes also rejected for descriptor", fd, "",
                   AT_EMPTY_PATH | AT_STATX_SYNC_TYPE, STATX_BASIC_STATS);
    expect_invalid("reserved mask checked before descriptor lookup", -123, "", AT_EMPTY_PATH,
                   STATX__RESERVED);
    expect_invalid("conflicting sync checked before descriptor lookup", -123, "",
                   AT_EMPTY_PATH | AT_STATX_SYNC_TYPE, STATX_BASIC_STATS);
    expect_invalid("empty pathname plus reserved mask", AT_FDCWD, "", 0, STATX__RESERVED);
    expect_invalid("empty pathname plus conflicting sync", AT_FDCWD, "", AT_STATX_SYNC_TYPE,
                   STATX_BASIC_STATS);
    expect_error("empty pathname with valid request still requires flag", AT_FDCWD, "", 0,
                 STATX_BASIC_STATS, ENOENT);
    expect_invalid("NULL pathname plus reserved mask", AT_FDCWD, NULL, 0, STATX__RESERVED);
    expect_invalid("NULL pathname plus conflicting sync", AT_FDCWD, NULL, AT_STATX_SYNC_TYPE, 0);
    expect_invalid("unmapped pathname plus reserved mask", AT_FDCWD, (const char *)1, 0,
                   STATX__RESERVED);
    expect_invalid("unmapped pathname plus conflicting sync", AT_FDCWD, (const char *)1,
                   AT_STATX_SYNC_TYPE, 0);
    expect_error("valid request still checks pathname memory", AT_FDCWD, (const char *)1, 0,
                 STATX_BASIC_STATS, EFAULT);
    if (close(fd)) { perror("close root"); return 2; }
    printf("SUMMARY checks=%u failures=%u\n", checks, failures);
    return failures ? 1 : 0;
}
