// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdio.h>
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

int main(void)
{
    int saved = open(".", O_RDONLY | O_DIRECTORY);
    if (saved < 0 || chdir("/")) { perror("fixture"); return 2; }
    char buf[16];
    const unsigned long good_sizes[] = {sizeof(buf), 2, ULONG_MAX};
    for (size_t i = 0; i < sizeof(good_sizes) / sizeof(good_sizes[0]); ++i) {
        memset(buf, '!', sizeof(buf));
        errno = 0;
        long ret = syscall(SYS_getcwd, buf, good_sizes[i]);
        printf("size=%#lx ret=%ld errno=%d\n", good_sizes[i], ret, errno);
        check("returns NUL-inclusive byte count and leaves suffix intact",
              ret == 2 && buf[0] == '/' && buf[1] == '\0' && buf[2] == '!');
    }
    for (unsigned long size = 0; size < 2; ++size) {
        memset(buf, '!', sizeof(buf));
        errno = 0;
        long ret = syscall(SYS_getcwd, buf, size);
        check("short buffer returns ERANGE without writing", ret == -1 && errno == ERANGE && buf[0] == '!');
        errno = 0;
        ret = syscall(SYS_getcwd, NULL, size);
        check("short size precedes NULL output fault", ret == -1 && errno == ERANGE);
    }
    const unsigned long null_sizes[] = {2, sizeof(buf), ULONG_MAX};
    for (size_t i = 0; i < sizeof(null_sizes) / sizeof(null_sizes[0]); ++i) {
        errno = 0;
        long ret = syscall(SYS_getcwd, NULL, null_sizes[i]);
        check("adequate size with NULL output returns EFAULT", ret == -1 && errno == EFAULT);
    }
    errno = 0;
    long ret = syscall(SYS_getcwd, (void *)1, sizeof(buf));
    check("inaccessible non-NULL output returns EFAULT", ret == -1 && errno == EFAULT);
    if (fchdir(saved) || close(saved)) { perror("cleanup"); return 2; }
    printf("RESULT checks=%d failures=%d\n", checks, failures);
    return failures != 0;
}
