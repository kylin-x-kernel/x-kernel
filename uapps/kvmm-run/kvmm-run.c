// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

// kvmm-run: launch and own a kvmm guest VM through /dev/kvmm-vm.
//
// The kvmm-vm device binds one VM to one open file description:
//   * the first write carries the "bootlinux ..." command and boots the VM;
//   * later writes are guest UART console input;
//   * closing the fd tears the VM down (stop vCPUs, free guest memory).
//
// A transient `echo bootlinux > /dev/kvmm-vm` would boot the VM and then close
// the fd immediately, killing it. This program is the intended long-lived
// owner: it boots the VM, then blocks holding the fd open and bridges the
// console until the user asks it to stop (Ctrl-] on the keyboard, SIGTERM, or
// EOF on stdin), at which point it closes the fd and the VM is reclaimed.
//
// While bridging, stdin is switched to raw mode so control characters, ESC and
// arrow-key sequences, and single keystrokes pass through to the guest instead
// of being cooked by the host tty line discipline. Because Ctrl-C is therefore
// forwarded to the guest, Ctrl-] is reserved as the local "stop" key.
//
// Guest->host serial (device read) is wired: the device drains the guest UART
// TX channel, reporting readable via poll only when output is pending. The poll
// loop below forwards that output to stdout as it arrives, so the guest console
// is displayed raw and per-byte.

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <termios.h>
#include <unistd.h>

#define DEV_PATH "/dev/kvmm-vm"
#define BOOT_PREFIX "bootlinux"

// Local escape key: Ctrl-] (0x1d, telnet's escape). In raw mode Ctrl-C is
// forwarded to the guest, so this is how the user stops kvmm-run and tears the
// VM down from the keyboard. Every other byte passes through untouched.
#define ESC_QUIT 0x1d

static volatile sig_atomic_t g_stop = 0;

static void on_signal(int sig) {
    (void)sig;
    g_stop = 1;
}

// Host tty raw-mode state. Saved so we can restore the terminal on exit.
static struct termios g_saved_tio;
static int g_raw_active = 0;

// Put stdin into raw mode so control characters (Ctrl-C = 0x03, Ctrl-Z, ...),
// ESC/arrow-key sequences, Tab and single keystrokes reach the guest verbatim
// instead of being cooked by the host tty line discipline. cfmakeraw clears
// ICANON/ECHO/ISIG/IEXTEN/IXON and sets VMIN=1/VTIME=0, which x-kernel's ktty
// honours per byte. We keep ICRNL on, though: the Enter key sends CR (0x0d),
// and the guest console expects it translated to NL so a line is submitted and
// the prompt advances — dropping the translation makes Enter emit a bare CR
// and the shell prompt fails to move to a new line. No-op when stdin is not a
// tty (piped/scripted input).
static void enter_raw_mode(void) {
    if (!isatty(STDIN_FILENO)) {
        return;
    }
    if (tcgetattr(STDIN_FILENO, &g_saved_tio) < 0) {
        return;
    }
    struct termios raw = g_saved_tio;
    cfmakeraw(&raw);
    raw.c_iflag |= ICRNL; // Enter (CR) -> NL, like the cooked terminal did
    if (tcsetattr(STDIN_FILENO, TCSANOW, &raw) < 0) {
        return;
    }
    g_raw_active = 1;
}

// Restore the terminal to the mode saved by enter_raw_mode. Idempotent.
static void restore_tty(void) {
    if (g_raw_active) {
        tcsetattr(STDIN_FILENO, TCSANOW, &g_saved_tio);
        g_raw_active = 0;
    }
}

// Build the boot command: always starts with "bootlinux" and appends any extra
// argv tokens (guest image paths, "@0xBASE", ...) so callers can override the
// defaults, e.g. `kvmm-run /guests/linux/linux.bin /guests/linux/linux.dtb`.
static size_t build_boot_cmd(int argc, char **argv, char *out, size_t cap) {
    size_t off = 0;
    int n = snprintf(out, cap, "%s", BOOT_PREFIX);
    if (n < 0) {
        return 0;
    }
    off = (size_t)n;
    for (int i = 1; i < argc && off < cap; i++) {
        if (strcmp(argv[i], BOOT_PREFIX) == 0) {
            continue; // avoid a duplicate prefix if the caller passed it
        }
        int m = snprintf(out + off, cap - off, " %s", argv[i]);
        if (m < 0) {
            break;
        }
        off += (size_t)m;
    }
    return off;
}

// Write the whole buffer, retrying short/interrupted writes.
static int write_all(int fd, const char *buf, size_t len) {
    size_t off = 0;
    while (off < len) {
        ssize_t k = write(fd, buf + off, len - off);
        if (k < 0) {
            if (errno == EINTR) {
                continue;
            }
            return -1;
        }
        off += (size_t)k;
    }
    return 0;
}

int main(int argc, char **argv) {
    char cmd[512];
    size_t cmd_len = build_boot_cmd(argc, argv, cmd, sizeof cmd);
    if (cmd_len == 0) {
        fprintf(stderr, "kvmm-run: failed to build boot command\n");
        return 1;
    }

    int fd = open(DEV_PATH, O_RDWR);
    if (fd < 0) {
        fprintf(stderr, "kvmm-run: open %s: %s\n", DEV_PATH, strerror(errno));
        return 1;
    }

    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_signal;
    sigaction(SIGINT, &sa, NULL);
    sigaction(SIGTERM, &sa, NULL);

    // First write boots the VM into this fd's instance.
    if (write_all(fd, cmd, cmd_len) < 0) {
        fprintf(stderr, "kvmm-run: boot write failed: %s\n", strerror(errno));
        close(fd);
        return 1;
    }
    fprintf(stderr,
            "kvmm-run: booted via '%s'\n"
            "kvmm-run: console attached; press Ctrl-] to stop the VM\n",
            cmd);

    // Switch stdin to raw so special characters pass through to the guest.
    // Done after the boot write so the earlier error paths need no restore.
    enter_raw_mode();

    // Console bridge. stdin -> guest UART RX; guest UART TX -> stdout.
    int tx_dead = 0; // guest->host path unavailable (EOF / unsupported)
    char buf[1024];
    while (!g_stop) {
        struct pollfd fds[2];
        fds[0].fd = STDIN_FILENO;
        fds[0].events = POLLIN;
        fds[0].revents = 0;
        fds[1].fd = fd;
        fds[1].events = POLLIN;
        fds[1].revents = 0;
        int nfds = tx_dead ? 1 : 2;

        int r = poll(fds, nfds, 200);
        if (r < 0) {
            if (errno == EINTR) {
                continue; // a signal woke us; loop re-checks g_stop
            }
            // poll unsupported on this fd: just hold it open until a signal.
            fprintf(stderr, "kvmm-run: poll unavailable (%s); holding open until signal\n",
                    strerror(errno));
            while (!g_stop) {
                pause();
            }
            break;
        }

        if (fds[0].revents & POLLIN) {
            ssize_t n = read(STDIN_FILENO, buf, sizeof buf);
            if (n <= 0) {
                break; // stdin EOF (pipe closed) -> stop the VM
            }
            // Forward input to the guest, but stop at the local escape key
            // (Ctrl-]): send everything before it, then tear the VM down.
            size_t fwd = (size_t)n;
            for (size_t i = 0; i < (size_t)n; i++) {
                if (buf[i] == ESC_QUIT) {
                    fwd = i;
                    g_stop = 1;
                    break;
                }
            }
            if (fwd > 0 && write_all(fd, buf, fwd) < 0) {
                fprintf(stderr, "kvmm-run: console write failed: %s\n", strerror(errno));
                break;
            }
            if (g_stop) {
                break;
            }
        }
        if (fds[0].revents & (POLLERR | POLLHUP | POLLNVAL)) {
            break; // stdin gone
        }

        if (nfds > 1) {
            if (fds[1].revents & (POLLERR | POLLNVAL)) {
                tx_dead = 1;
            } else if (fds[1].revents & POLLIN) {
                ssize_t n = read(fd, buf, sizeof buf);
                if (n > 0) {
                    (void)write_all(STDOUT_FILENO, buf, (size_t)n);
                } else if (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR)) {
                    // Non-blocking device: no output ready right now. Keep
                    // polling; this is not end-of-stream.
                } else {
                    tx_dead = 1; // real EOF/error -> stop polling the device
                }
            }
        }
    }

    restore_tty(); // put the host terminal back before we exit
    fprintf(stderr, "\nkvmm-run: closing %s -> VM teardown\n", DEV_PATH);
    close(fd); // release -> Vm::stop_and_join -> drop VM (frees guest memory)
    return 0;
}
