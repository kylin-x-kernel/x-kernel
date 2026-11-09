#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef CLOSE_RANGE_CLOEXEC
#define CLOSE_RANGE_CLOEXEC (1U << 2)
#endif

/* Use the two real launch-time operations: close a tail, or mark it CLOEXEC. */
int main(void) {
    int passed = 0;
    int fd = open("/dev/null", O_RDONLY);
    if (fd < 0) return 2;
    errno = 0;
    long rc = syscall(SYS_close_range, (unsigned)fd, UINT_MAX, CLOSE_RANGE_CLOEXEC);
    int error = errno;
    int flags = fcntl(fd, F_GETFD);
    int ok = rc == 0 && flags >= 0 && (flags & FD_CLOEXEC);
    passed += ok;
    printf("CLOEXEC_UINT_MAX rc=%ld errno=%d fd_flags=%d pass=%d\n", rc, error, flags, ok);
    close(fd);

    fd = open("/dev/null", O_RDONLY);
    if (fd < 0) return 2;
    errno = 0;
    rc = syscall(SYS_close_range, (unsigned)fd, UINT_MAX, 0U);
    error = errno;
    errno = 0;
    flags = fcntl(fd, F_GETFD);
    ok = rc == 0 && flags == -1 && errno == EBADF;
    passed += ok;
    printf("CLOSE_UINT_MAX rc=%ld errno=%d fd_flags=%d pass=%d\n", rc, error, flags, ok);
    errno = 0;
    rc = syscall(SYS_close_range, UINT_MAX, UINT_MAX, 0U);
    error = errno;
    ok = rc == 0;
    passed += ok;
    printf("EMPTY_UINT_MAX rc=%ld errno=%d pass=%d\n", rc, error, ok);
    printf("{\"case\":\"L1\",\"subtests\":3,\"passed\":%d,\"status\":\"%s\"}\n", passed, passed == 3 ? "pass" : "fail");
    return passed == 3 ? 0 : 1;
}
