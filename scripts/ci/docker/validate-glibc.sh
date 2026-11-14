#!/usr/bin/env bash
# Validate the GNU guest toolchains inside a candidate CI builder image.
# Run as its default jenkins user; no VM, privileged container or credentials.
set -euo pipefail
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
native="$(uname -m)"
cat > "$work/probe.c" <<'C'
#include <assert.h>
#include <gnu/libc-version.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <linux/vm_sockets.h>
static _Thread_local int value;
static void *child(void *arg) { assert(value == 0); value = 42; return arg; }
int main(void) {
    void *memory = NULL;
    pthread_t thread;
    value = 7;
    assert(gnu_get_libc_version()[0] == '2');
    assert(posix_memalign(&memory, 64, 4096) == 0);
    memset(memory, 0x5a, 4096);
    assert(((unsigned char *)memory)[4095] == 0x5a);
    free(memory);
    assert(pthread_create(&thread, NULL, child, NULL) == 0);
    assert(pthread_join(thread, NULL) == 0);
    assert(value == 7);
    printf("GNU C/pthread runtime: glibc %s\n", gnu_get_libc_version());
    return 0;
}
C
cat > "$work/probe.cc" <<'CPP'
#include <cassert>
#include <thread>
#include <vector>
int main() {
    std::vector<int> values(4, 7);
    std::thread worker([&] { values[0] = 42; });
    worker.join();
    assert(values[0] == 42);
}
CPP
cat > "$work/CMakeLists.txt" <<'CMAKE'
cmake_minimum_required(VERSION 3.10)
project(gnu_archive C)
add_library(gnu_archive STATIC probe.c)
CMAKE
for arch in x86_64 aarch64 riscv64; do
    prefix="${arch}-linux-gnu"
    machine="$("${prefix}-gcc" -dumpmachine)"
    [[ "$machine" == "$prefix" ]] || { echo "wrong GNU compiler: $machine" >&2; exit 1; }
    "${prefix}-ar" --version >/dev/null
    "${prefix}-gcc" -O2 -Wall -Werror -pthread "$work/probe.c" -o "$work/$arch-c"
    "${prefix}-g++" -O2 -Wall -Werror -pthread "$work/probe.cc" -o "$work/$arch-cxx"
    if ! cmake -S "$work" -B "$work/cmake-$arch" \
        -DCMAKE_C_COMPILER="$(command -v "${prefix}-gcc")" \
        -DCMAKE_AR="$(command -v "${prefix}-ar")" \
        -DCMAKE_TRY_COMPILE_TARGET_TYPE=STATIC_LIBRARY > "$work/$arch-cmake.log" 2>&1 \
        || ! cmake --build "$work/cmake-$arch" >> "$work/$arch-cmake.log" 2>&1; then
        cat "$work/$arch-cmake.log" >&2
        exit 1
    fi
    readelf -l "$work/$arch-c" | grep -q 'Requesting program interpreter: .*ld-linux'
    readelf -l "$work/$arch-cxx" | grep -q 'Requesting program interpreter: .*ld-linux'
    readelf -d "$work/$arch-c" | grep -q 'Shared library: \[libc.so.6\]'
    target_arch="$arch"
    [[ "$arch" != riscv64 ]] || target_arch=riscv64gc
    key="BINDGEN_EXTRA_CLANG_ARGS_${target_arch}_unknown_linux_gnu"
    [[ -n "${!key:-}" ]] || { echo "missing $key" >&2; exit 1; }
    read -r -a clang_flags <<< "${!key}"
    clang "${clang_flags[@]}" -fsyntax-only -Werror "$work/probe.c"
    if [[ "$arch" == "$native" ]]; then
        "$work/$arch-c"
        "$work/$arch-cxx"
    fi
    echo "GNU compiler/linker/C++/CMake/bindgen headers OK: $arch"
done
for tool in cmake make git python3 debugfs readelf flock qemu-system-x86_64 qemu-system-aarch64 qemu-system-riscv64; do
    command -v "$tool" >/dev/null
done
[[ -w "$RUSTUP_HOME" && -w "$CARGO_HOME" && -w /xkernel-target ]]
printf 'GNU_TOOLCHAIN_VALIDATED host=%s user=%s\n' "$native" "$(id -un)"
