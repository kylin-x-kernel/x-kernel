// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.
/* Standalone Linux AArch64 EL0 instruction-permission regression. */
#define _GNU_SOURCE
#include <errno.h>
#include <inttypes.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/resource.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#if !defined(__aarch64__)
#error This probe requires an AArch64 compiler and execution environment.
#endif
#if __BYTE_ORDER__ != __ORDER_LITTLE_ENDIAN__
#error The generated instruction stores require little-endian AArch64.
#endif

enum { CHILD_SKIP = 77, CHILD_ERROR = 90 };
enum wait_result { WAIT_ERROR = -1, WAIT_TIMEOUT = 0, WAIT_REAPED = 1 };

extern uint64_t el0_probe_ctr(void);
extern uint64_t el0_probe_dczid(void);
extern void el0_probe_dc(void *address);
extern void el0_probe_ic(void *address);
extern void el0_probe_zva(void *address);
extern void el0_probe_udf(void);

/* The operand of each maintenance instruction is a live private mapping.
 * DCZID_EL0 is unconditionally readable at EL0. UDF is a real instruction
 * fault, not a software-generated SIGILL.
 */
__asm__(".text\n.balign 4\n"
        ".global el0_probe_ctr\n.type el0_probe_ctr, %function\n"
        "el0_probe_ctr:\nmrs x0, ctr_el0\nret\n"
        ".size el0_probe_ctr, .-el0_probe_ctr\n"
        ".global el0_probe_dczid\n.type el0_probe_dczid, %function\n"
        "el0_probe_dczid:\nmrs x0, dczid_el0\nret\n"
        ".size el0_probe_dczid, .-el0_probe_dczid\n"
        ".global el0_probe_dc\n.type el0_probe_dc, %function\n"
        "el0_probe_dc:\ndc cvau, x0\nret\n"
        ".size el0_probe_dc, .-el0_probe_dc\n"
        ".global el0_probe_ic\n.type el0_probe_ic, %function\n"
        "el0_probe_ic:\nic ivau, x0\nret\n"
        ".size el0_probe_ic, .-el0_probe_ic\n"
        ".global el0_probe_zva\n.type el0_probe_zva, %function\n"
        "el0_probe_zva:\ndc zva, x0\ndsb ish\nret\n"
        ".size el0_probe_zva, .-el0_probe_zva\n"
        ".global el0_probe_udf\n.type el0_probe_udf, %function\n"
        "el0_probe_udf:\n.inst 0x00000000\nret\n"
        ".size el0_probe_udf, .-el0_probe_udf\n");

static int monotonic_seconds(double *seconds)
{
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC, &now) != 0)
        return -1;
    *seconds = (double)now.tv_sec + (double)now.tv_nsec / 1e9;
    return 0;
}

/* These deadlines bound a scheduling userspace, not a hung kernel or VM. */
static enum wait_result wait_bounded(pid_t child, int *status, double timeout)
{
    double started;
    if (monotonic_seconds(&started) != 0)
        return WAIT_ERROR;
    for (;;) {
        pid_t got = waitpid(child, status, WNOHANG);
        if (got == child)
            return WAIT_REAPED;
        if (got < 0 && errno != EINTR)
            return WAIT_ERROR;
        double now;
        if (monotonic_seconds(&now) != 0)
            return WAIT_ERROR;
        if (now - started >= timeout)
            return WAIT_TIMEOUT;
        const struct timespec pause = {0, 10000000};
        if (nanosleep(&pause, NULL) != 0 && errno != EINTR)
            return WAIT_ERROR;
    }
}

static int child_error(const char *stage)
{
    printf("{\"event\":\"child_error\",\"stage\":\"%s\",\"errno\":%d}\n",
           stage, errno);
    return CHILD_ERROR;
}

static int affinity_unavailable(int error)
{
    return error == ENOSYS || error == ENOTSUP || error == EPERM ||
           error == EACCES || error == EINVAL;
}

static int bind_child_cpu(int cpu)
{
    if (cpu < 0)
        return 0;
    cpu_set_t mask;
    CPU_ZERO(&mask);
    CPU_SET(cpu, &mask);
    if (sched_setaffinity(0, sizeof(mask), &mask) != 0) {
        int error = errno;
        printf("{\"event\":\"affinity\",\"cpu\":%d,\"stage\":\"set\","
               "\"errno\":%d,\"status\":\"%s\"}\n", cpu, error,
               affinity_unavailable(error) ? "skip" : "fail");
        return affinity_unavailable(error) ? CHILD_SKIP : CHILD_ERROR;
    }
    int actual_cpu = sched_getcpu();
    if (actual_cpu < 0) {
        int error = errno;
        printf("{\"event\":\"affinity\",\"cpu\":%d,\"stage\":\"getcpu\","
               "\"errno\":%d,\"status\":\"%s\"}\n", cpu, error,
               affinity_unavailable(error) ? "skip" : "fail");
        return affinity_unavailable(error) ? CHILD_SKIP : CHILD_ERROR;
    }
    printf("{\"event\":\"affinity\",\"cpu\":%d,\"actual_cpu\":%d,"
           "\"status\":\"%s\"}\n", cpu, actual_cpu,
           actual_cpu == cpu ? "pass" : "fail");
    return actual_cpu == cpu ? 0 : CHILD_ERROR;
}

static int run_jit(void *page, size_t page_size)
{
    const unsigned values[] = {42, 73};
    volatile uint32_t *code = page;
    /* AArch64 function pointers contain code addresses. The mapping is RX
     * before each call; only these two generated leaf instructions execute.
     */
    int (*generated)(void) = (int (*)(void))page;
    for (unsigned phase = 0; phase < 2; ++phase) {
        if (phase && mprotect(page, page_size, PROT_READ | PROT_WRITE) != 0)
            return child_error("mprotect_rw");
        code[0] = UINT32_C(0x52800000) | (values[phase] << 5); /* mov w0, #imm */
        code[1] = UINT32_C(0xd65f03c0); /* ret */
        __builtin___clear_cache((char *)page, (char *)page + 2 * sizeof(uint32_t));
        if (mprotect(page, page_size, PROT_READ | PROT_EXEC) != 0)
            return child_error("mprotect_rx");
        int actual = generated();
        printf("{\"event\":\"jit_value\",\"phase\":%u,\"expected\":%u,"
               "\"actual\":%d}\n", phase + 1, values[phase], actual);
        if (actual != (int)values[phase])
            return CHILD_ERROR;
    }
    return 0;
}

static int child_run(const char *mode, int cpu, size_t page_size)
{
    struct sigaction action = {0};
    action.sa_handler = SIG_DFL;
    sigemptyset(&action.sa_mask);
    sigset_t unblock;
    sigemptyset(&unblock);
    const int signals[] = {SIGILL, SIGSEGV, SIGBUS};
    for (unsigned i = 0; i < sizeof(signals) / sizeof(signals[0]); ++i) {
        if (sigaction(signals[i], &action, NULL) != 0)
            return child_error("sigaction");
        sigaddset(&unblock, signals[i]);
    }
    if (sigprocmask(SIG_UNBLOCK, &unblock, NULL) != 0)
        return child_error("sigprocmask");
    int result = bind_child_cpu(cpu);
    if (result)
        return result;
    if (!strcmp(mode, "udf")) {
        el0_probe_udf();
        return CHILD_ERROR; /* Reaching here must not pass the negative control. */
    }
    void *page = mmap(NULL, page_size, PROT_READ | PROT_WRITE,
                      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (page == MAP_FAILED)
        return child_error("mmap");
    *(volatile unsigned char *)page = 0x5a;
    if (!strcmp(mode, "ctr")) {
        uint64_t ctr = el0_probe_ctr();
        printf("{\"event\":\"ctr_value\",\"value\":\"0x%016" PRIx64 "\"}\n", ctr);
    } else if (!strcmp(mode, "dc")) {
        el0_probe_dc(page);
    } else if (!strcmp(mode, "ic")) {
        el0_probe_ic(page);
    } else if (!strcmp(mode, "zva")) {
        uint64_t dczid = el0_probe_dczid();
        /* DZP (bit 4) set: DC ZVA is not supported, report an explicit skip. */
        if ((dczid & UINT64_C(0x10)) != 0) {
            printf("{\"event\":\"zva_unsupported\","
                   "\"dczid\":\"0x%016" PRIx64 "\"}\n", dczid);
            return CHILD_SKIP;
        }
        const size_t line = (size_t)4 << (dczid & UINT64_C(0xF));
        volatile unsigned char *bytes = page;
        for (size_t i = 0; i < line && i < page_size; ++i)
            bytes[i] = UINT8_C(0x5a);
        el0_probe_zva(page);
        for (size_t i = 0; i < line && i < page_size; ++i)
            if (bytes[i] != UINT8_C(0))
                return child_error("zva_not_zeroed");
        printf("{\"event\":\"zva_zeroed\",\"line_bytes\":%zu}\n", line);
    } else {
        result = run_jit(page, page_size);
    }
    /* Finish permitted maintenance before the operand mapping is released. */
    __asm__ volatile("dsb ish\nisb" ::: "memory");
    if (munmap(page, page_size) != 0)
        return child_error("munmap");
    return result;
}

/* The single-threaded parent owns this child and never installs a reaper.
 * Recheck ownership before signaling: ECHILD may mean the PID is reusable.
 */
static void cleanup_child(pid_t child)
{
    int status = 0;
    pid_t got = waitpid(child, &status, WNOHANG);
    if (got != 0) {
        printf("{\"event\":\"cleanup\",\"child_pid\":%ld,\"reaped\":%s,"
               "\"errno\":%d,\"signal_sent\":false}\n", (long)child,
               got == child ? "true" : "false", got < 0 ? errno : 0);
        return;
    }
    /* A live child or its unreaped zombie keeps the PID reserved for us. */
    int killed = kill(child, SIGKILL);
    int kill_errno = killed < 0 ? errno : 0;
    enum wait_result waited = wait_bounded(child, &status, 2.0);
    printf("{\"event\":\"cleanup\",\"child_pid\":%ld,\"kill_errno\":%d,"
           "\"signal_sent\":%s,\"reaped\":%s,\"wait_errno\":%d}\n",
           (long)child, kill_errno, killed == 0 ? "true" : "false",
           waited == WAIT_REAPED ? "true" : "false",
           waited == WAIT_ERROR ? errno : 0);
}

/* Return 0=pass, 1=fail, 2=skip; setup and timeout errors are failures. */
static int run_one(const char *mode, int cpu, size_t page_size)
{
    printf("{\"event\":\"start\",\"mode\":\"%s\",\"cpu\":%d}\n", mode, cpu);
    pid_t child = fork();
    if (child < 0) {
        printf("{\"event\":\"result\",\"mode\":\"%s\",\"cpu\":%d,"
               "\"status\":\"fail\",\"stage\":\"fork\",\"errno\":%d}\n",
               mode, cpu, errno);
        return 1;
    }
    if (child == 0)
        _exit(child_run(mode, cpu, page_size));
    int status = 0;
    enum wait_result waited = wait_bounded(child, &status, 10.0);
    if (waited != WAIT_REAPED) {
        int wait_errno = waited == WAIT_ERROR ? errno : 0;
        /* ECHILD explicitly forbids trying to signal this numerical PID. */
        if (wait_errno != ECHILD)
            cleanup_child(child);
        printf("{\"event\":\"result\",\"mode\":\"%s\",\"cpu\":%d,"
               "\"child_pid\":%ld,\"status\":\"fail\",\"timed_out\":%s,"
               "\"wait_errno\":%d}\n", mode, cpu, (long)child,
               waited == WAIT_TIMEOUT ? "true" : "false", wait_errno);
        return 1;
    }
    int signo = WIFSIGNALED(status) ? WTERMSIG(status) : 0;
    int code = WIFEXITED(status) ? WEXITSTATUS(status) : -1;
    /* Skips are mode-driven (zva on unsupported hardware) or affinity-driven. */
    int skipped = code == CHILD_SKIP;
    int passed = !strcmp(mode, "udf") ? signo == SIGILL : code == 0;
    printf("{\"event\":\"result\",\"mode\":\"%s\",\"cpu\":%d,"
           "\"child_pid\":%ld,\"signal\":%d,\"exit_code\":%d,"
           "\"status\":\"%s\"}\n", mode, cpu, (long)child, signo, code,
           skipped ? "skip" : passed ? "pass" : "fail");
    return skipped ? 2 : passed ? 0 : 1;
}

int main(int argc, char **argv)
{
    const char *modes[] = {"ctr", "dc", "ic", "zva", "jit", "udf"};
    int all = argc >= 2 && (!strcmp(argv[1], "all") || !strcmp(argv[1], "--all"));
    int per_cpu = argc == 3 && !strcmp(argv[2], "--all-cpus");
    int selected = -1;
    if (argc >= 2)
        for (unsigned i = 0; i < sizeof(modes) / sizeof(modes[0]); ++i)
            if (!strcmp(argv[1], modes[i]))
                selected = (int)i;
    if (argc < 2 || argc > 3 || (!all && selected < 0) ||
        (argc == 3 && (!all || !per_cpu))) {
        fprintf(stderr, "usage: %s all [--all-cpus] | ctr | dc | ic | zva | jit | udf\n", argv[0]);
        return 2;
    }
    setbuf(stdout, NULL);
    long system_page_size = sysconf(_SC_PAGESIZE);
    if (system_page_size < 8) {
        fprintf(stderr, "sysconf returned an invalid page size: %ld\n", system_page_size);
        return 2;
    }
    struct sigaction action = {0};
    action.sa_handler = SIG_DFL;
    sigemptyset(&action.sa_mask);
    /* Undo inherited SIG_IGN/SA_NOCLDWAIT so child PIDs remain ours until wait. */
    if (sigaction(SIGCHLD, &action, NULL) != 0) {
        perror("sigaction SIGCHLD");
        return 2;
    }
    const struct rlimit no_core = {0, 0};
    if (setrlimit(RLIMIT_CORE, &no_core) != 0) {
        perror("setrlimit RLIMIT_CORE");
        return 2;
    }
    if (prctl(PR_SET_DUMPABLE, 0) != 0)
        printf("{\"event\":\"warning\",\"stage\":\"disable_dumpable\",\"errno\":%d}\n", errno);
    unsigned counts[3] = {0};
    for (unsigned i = 0; i < sizeof(modes) / sizeof(modes[0]); ++i)
        if (all || selected == (int)i)
            counts[run_one(modes[i], -1, (size_t)system_page_size)]++;
    if (per_cpu) {
        cpu_set_t allowed;
        CPU_ZERO(&allowed);
        if (sched_getaffinity(0, sizeof(allowed), &allowed) != 0) {
            int error = errno;
            int skipped = affinity_unavailable(error);
            counts[skipped ? 2 : 1]++;
            printf("{\"event\":\"affinity_enumeration\",\"status\":\"%s\","
                   "\"errno\":%d}\n", skipped ? "skip" : "fail", error);
        } else if (CPU_COUNT(&allowed) == 0) {
            counts[1]++;
            printf("{\"event\":\"affinity_enumeration\",\"status\":\"fail\",\"reason\":\"empty_mask\"}\n");
        } else {
            for (int cpu = 0; cpu < CPU_SETSIZE; ++cpu)
                if (CPU_ISSET(cpu, &allowed))
                    for (unsigned i = 0; i < sizeof(modes) / sizeof(modes[0]); ++i)
                        counts[run_one(modes[i], cpu, (size_t)system_page_size)]++;
        }
    }
    printf("{\"event\":\"summary\",\"passed\":%u,\"failed\":%u,\"skipped\":%u}\n",
           counts[0], counts[1], counts[2]);
    return counts[1] ? 1 : counts[2] ? CHILD_SKIP : 0;
}
