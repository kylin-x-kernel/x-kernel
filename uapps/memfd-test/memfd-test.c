// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

/* Exercise the syscall boundary, not a reimplementation of its name helper. */
static int check_label(const char *name, const char *colliding_name)
{
    const char payload[] = "memfd slash regression";
    char observed[sizeof(payload)];
    const char zeros[sizeof(payload)] = {0};
    const size_t mapping_len = 4096;
    int first = -1;
    int second = -1;
    void *mapping = MAP_FAILED;
    int result = 1;

    first = (int)syscall(SYS_memfd_create, name, 0);
    if (first < 0) {
        perror("memfd_create slash label");
        goto cleanup;
    }
    second = (int)syscall(SYS_memfd_create, colliding_name, 0);
    if (second < 0) {
        perror("memfd_create colliding label");
        goto cleanup;
    }
    if (ftruncate(first, (off_t)mapping_len) < 0 ||
        ftruncate(second, (off_t)mapping_len) < 0) {
        perror("ftruncate");
        goto cleanup;
    }
    mapping = mmap(NULL, mapping_len, PROT_READ | PROT_WRITE, MAP_SHARED,
                   first, 0);
    if (mapping == MAP_FAILED) {
        perror("mmap");
        goto cleanup;
    }
    memcpy(mapping, payload, sizeof(payload));
    /* Check mapped read/write access; pread after unmap checks fd contents. */
    if (memcmp(mapping, payload, sizeof(payload)) != 0) {
        fprintf(stderr, "mapped content mismatch\n");
        goto cleanup;
    }
    if (munmap(mapping, mapping_len) < 0) {
        perror("munmap");
        goto cleanup;
    }
    mapping = MAP_FAILED;

    ssize_t count;
    do {
        count = pread(first, observed, sizeof(observed), 0);
    } while (count < 0 && errno == EINTR);
    if (count != (ssize_t)sizeof(observed) ||
        memcmp(observed, payload, sizeof(payload)) != 0) {
        fprintf(stderr, "fd content mismatch after unmap (count=%zd)\n", count);
        goto cleanup;
    }
    do {
        count = pread(second, observed, sizeof(observed), 0);
    } while (count < 0 && errno == EINTR);
    if (count != (ssize_t)sizeof(observed) ||
        memcmp(observed, zeros, sizeof(zeros)) != 0) {
        fprintf(stderr, "colliding labels alias file content (count=%zd)\n", count);
        goto cleanup;
    }
    result = 0;

cleanup:
    if (mapping != MAP_FAILED && munmap(mapping, mapping_len) < 0) {
        perror("cleanup munmap");
        result = 1;
    }
    /* Do not retry close: an interrupted close may already release the fd. */
    if (second >= 0 && close(second) < 0) {
        perror("close second");
        result = 1;
    }
    if (first >= 0 && close(first) < 0) {
        perror("close first");
        result = 1;
    }
    return result;
}

int main(void)
{
    if (check_label("ict/shared-buffer", "ict:shared-buffer") != 0 ||
        check_label("/../buffer", ":..:buffer") != 0)
        return 1;

    puts("memfd slash syscall regression: PASS");
    return 0;
}
