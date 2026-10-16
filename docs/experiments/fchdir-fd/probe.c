// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static int checks, failures;
static void check(const char *name, int ok)
{
    ++checks;
    failures += !ok;
    printf("%s %s\n", ok ? "PASS" : "FAIL", name);
}

/* Return 2 for a broken fixture, independently of the syscall assertion. */
static int reject(const char *name, int fd, int error)
{
    char before[4096], after[4096];
    if (!getcwd(before, sizeof(before))) return 2;
    errno = 0;
    long ret = syscall(SYS_fchdir, fd);
    int got_errno = errno;
    if (!getcwd(after, sizeof(after))) return 2;
    check(name, ret == -1 && got_errno == error && !strcmp(before, after));
    return 0;
}

static int denied_in_child(int fd)
{
    char before[4096], after[4096];
    if (!geteuid() && setuid(65534)) return 2;
    if (!getcwd(before, sizeof(before))) return 2;
    errno = 0;
    long ret = syscall(SYS_fchdir, fd);
    int got_errno = errno;
    if (!getcwd(after, sizeof(after))) return 2;
    return !(ret == -1 && got_errno == EACCES && !strcmp(before, after));
}

int main(void)
{
    char dir[] = "/tmp/fchdir-abi-XXXXXX";
    int saved = -1, root = -1, child = -1, path = -1;
    int regular = -1, regular_path = -1, closed = -1;
    int fixture_error = 1, created = 0;
    saved = open(".", O_RDONLY | O_DIRECTORY);
    if (saved < 0 || !mkdtemp(dir)) goto cleanup;
    created = 1;
    root = open(dir, O_RDONLY | O_DIRECTORY);
    if (root < 0 || fchdir(root) || mkdir("child", 0700)) goto cleanup;
    child = open("child", O_RDONLY | O_DIRECTORY);
    path = open("child", O_PATH | O_DIRECTORY);
    regular = open("regular", O_CREAT | O_EXCL | O_RDWR, 0600);
    regular_path = open("regular", O_PATH);
    closed = dup(child);
    if (child < 0 || path < 0 || regular < 0 || regular_path < 0 || closed < 0)
        goto cleanup;
    int closed_fd = closed;
    if (close(closed)) { closed = -1; goto cleanup; }
    closed = -1;
    if (reject("AT_FDCWD is not an fd: EBADF and unchanged cwd", AT_FDCWD, EBADF) ||
        reject("negative fd: EBADF and unchanged cwd", -1, EBADF) ||
        reject("closed fd: EBADF and unchanged cwd", closed_fd, EBADF) ||
        reject("regular file: ENOTDIR and unchanged cwd", regular, ENOTDIR) ||
        reject("O_PATH regular file: ENOTDIR and unchanged cwd", regular_path, ENOTDIR))
        goto cleanup;
    const int dirs[] = {child, path};
    for (size_t i = 0; i < sizeof(dirs) / sizeof(dirs[0]); ++i) {
        char expected[4096], actual[4096];
        snprintf(expected, sizeof(expected), "%s/child", dir);
        long ret = syscall(SYS_fchdir, dirs[i]);
        if (!getcwd(actual, sizeof(actual))) goto cleanup;
        check("directory fd including O_PATH changes cwd", ret == 0 && !strcmp(actual, expected));
        if (fchdir(root)) goto cleanup;
    }
    if (fchmod(child, 0)) goto cleanup;
    fflush(stdout);
    pid_t pid = fork();
    if (pid < 0) goto cleanup;
    if (!pid) _exit(denied_in_child(child));
    int status;
    pid_t waited;
    do { waited = waitpid(pid, &status, 0); } while (waited < 0 && errno == EINTR);
    if (waited != pid || !WIFEXITED(status) || WEXITSTATUS(status) == 2)
        goto cleanup;
    check("search permission: EACCES and unchanged cwd", WEXITSTATUS(status) == 0);
    fixture_error = 0;

cleanup:
    /* Try every cleanup step even after setup or a previous cleanup failed. */
    if (child >= 0 && fchmod(child, 0700)) fixture_error = 1;
    if (root >= 0) {
        if (unlinkat(root, "regular", 0) && errno != ENOENT) fixture_error = 1;
        if (unlinkat(root, "child", AT_REMOVEDIR) && errno != ENOENT) fixture_error = 1;
    }
    if (saved >= 0 && fchdir(saved)) fixture_error = 1;
    const int fds[] = {closed, child, path, regular, regular_path, root, saved};
    for (size_t i = 0; i < sizeof(fds) / sizeof(fds[0]); ++i)
        if (fds[i] >= 0 && close(fds[i])) fixture_error = 1;
    if (created && rmdir(dir)) fixture_error = 1;
    if (fixture_error) {
        fputs("Fixture or cleanup failed\n", stderr);
        return 2;
    }
    printf("RESULT checks=%d failures=%d\n", checks, failures);
    return failures != 0;
}
