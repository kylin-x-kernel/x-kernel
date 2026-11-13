# X-Kernel

[中文版](README_CN.md) · [Repository](https://gitee.com/openkylin/x-kernel) · [Issues](https://gitee.com/openkylin/x-kernel/issues) · [Community](https://x-kernel.zulipchat.com)

[![License: Apache-2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)

X-Kernel is a monolithic kernel written in Rust that targets the Linux ABI. It runs Linux userspace built for musl/glibc on x86_64, AArch64, RISC-V 64, and LoongArch64, provides in-kernel TEE support for confidential-computing guests, and runs Wayland/X11 desktop workloads including Chromium, niri, and Xorg/Xwayland.

<p align="center">
  <img src="docs/images/desktop-niri-bilibili-fm.png" alt="X-Kernel running niri with Chromium on Bilibili and PCManFM" width="100%">
</p>
<p align="center"><em>niri + Chromium (Bilibili) + PCManFM on X-Kernel (x86_64, virtio-gpu, 1600x900).</em></p>

Subsystems are separated into crates with explicit ownership boundaries; see [Source layout](#source-layout). The project is under active development on `main` (`0.3.0-dev`), with quarterly milestone releases ([release policy](docs/release.md)).

**Data baseline:** capability and CI data were last verified on 2026-11-13. Performance uses daily-test #139.

## Contents

- [Why X-Kernel](#why-x-kernel)
- [Quick Start](#quick-start)
- [Real Workloads](#real-workloads)
- [Platform Support](#platform-support)
- [Capability Status](#capability-status)
- [Desktop Workloads](#desktop-workloads)
- [Design](#design)
- [Verification](#verification)
- [Performance](#performance)
- [Configuration](#configuration)
- [Source Layout](#source-layout)
- [Documentation](#documentation)
- [Community and Contributing](#community-and-contributing)
- [Security](#security)
- [Known Limitations](#known-limitations)
- [Roadmap](#roadmap)
- [License](#license)
- [Acknowledgements](#acknowledgements)

## Why X-Kernel

- **Linux ABI with production userspace.** X-Kernel implements a Linux-compatible syscall and POSIX surface in `core/ksyscall`, `posix/`, and `api/linux_sysno`. systemd boots as PID 1 with glibc, Docker hello-world runs, and Wayland/X11 desktops run on the same kernel; the Starry Test Harness exercises the ABI from user space.
- **Four architectures, three in CI.** x86_64, AArch64, RISC-V 64, and LoongArch64 are supported; x86_64, AArch64, and RISC-V run the PR pipeline (build, unit tests, runtime tests, and TEE tests on x86_64/AArch64). LoongArch64, CSV, pKVM, and RK3588 are documented but not yet in PR CI.
- **Confidential computing in the kernel.** Hygon CSV has hardware-rooted key derivation (HUK) and attestation. ARM virtCCA has HUK derivation; pKVM is an experimental guest mode. TEE support includes TA (Trusted Application) lifecycle, secure storage, and SM2/ECC/RSA through RustCrypto.
- **Desktop-ready graphics.** DRM/KMS, virtio-gpu 2D/3D, EDID, page-flip/vblank, fbdev, and virtio-input support Chromium (Wayland), niri, and Xorg/Xwayland, including keyboard/mouse input and host-window resize. The compositor and rootfs are userspace artifacts; the kernel provides the display, GPU, and input interfaces.
- **Verification at scale.** The tree contains 4,733 `#[def_test]` cases and 1,167 `#[test]` cases; the external [Starry Test Harness](https://gitee.com/openkylin/starry-test-harness) adds 51 guest cases across `ci` and `ci-test-iter`. Dated public pass-rate reports are not yet published; see [Verification](#verification) and [Known Limitations](#known-limitations).

## Quick Start

### Prerequisites

| Requirement | Notes |
|---|---|
| Rust | Pinned by `rust-toolchain.toml` (1.95.0, with five targets). No manual `rustup target add` is required. |
| QEMU | A matching system emulator for `make run`. |
| `cargo-binutils`, `rust-objcopy` | Required for kernel image generation. |
| Musl cross toolchain | Only for uapps, TEE tests, and Harness guest cases. The kernel build itself does not compile C; see [Cross toolchains](#cross-toolchains). |
| Rootfs tools | `curl` and `xz` for `make rootfs`. |
| x86_64 boot media | OVMF/EDK2 for UEFI, `mkfs.fat` and mtools for FAT media, and a GNU i386 toolchain for the LinuxBoot setup code. |
| Unit-test coverage | `debugfs`, `rust-profdata`, and `rust-cov`. |
| Repository hygiene | `make install-tools` for the pinned `cargo-shear`, `licensure`, and dylint tools. |

The CI builder image provides the CI build environment (the project toolchain is installed by CI); see [`scripts/ci/docker/`](scripts/ci/docker/) and [`docs/jenkins-pipeline.md`](docs/jenkins-pipeline.md). The complete prerequisite reference is in [`docs/ai/skills/build-workflow/SKILL.md`](docs/ai/skills/build-workflow/SKILL.md).

### Build and run

```bash
# AArch64 QEMU: the shortest path
cp platforms/kplat-aarch64/qemu_defconfig .config
make defconfig
make rootfs          # needs curl and xz
make build
make run
```

```bash
# x86_64 QEMU, with optional UEFI boot
cp platforms/kplat-x86_64/qemu_defconfig .config
make defconfig
make run
make run UEFI=y
```

`make run` uses LinuxBoot on x86_64 by default; `make run UEFI=y` selects OVMF/UEFI. Custom firmware paths can be supplied with `OVMF_CODE` and `OVMF_VARS_TEMPLATE`.

For x86_64 CSV, `scripts/start.sh` builds the UEFI image and starts QEMU with SEV. It needs OVMF plus the SEV launch artifacts (`cvm_1_launch_blob.bin` and `cvm_1_guest_owner_dh.cert` by default).

```bash
bash scripts/start.sh
```

### Real workloads

These are the end-to-end workloads that distinguish X-Kernel from a syscall-only Linux ABI implementation.

| Workload | Command | Expected | Status |
|---|---|---|---|
| systemd + glibc | `make rootfs ROOTFS_VARIANT=debian-systemd` | systemd 257 + glibc 2.41 reach a shell | Captured 2026-11-12 |
| Docker | `docker run --network=host --rm hello-world` | `Hello from Docker!` | Manual |
| Wayland desktop | `make run GRAPHIC=y DISK_IMG=<desktop-image>` | Chromium/niri/Xorg starts | Captured 2026-11-12 |


![systemd + glibc on X-Kernel](docs/images/systemd-glibc.png)

*systemd 257 + glibc 2.41 on Debian 13 (x86_64, systemd `debian-systemd` rootfs).*

### Run options

```bash
make run XKMAKE_ARGS='--memory 2g --smp 2 --no-net'
make run XKMAKE_ARGS='--no-vsock'
make run QEMU_ARGS='-d guest_errors'
make run QEMU_ARGS='-no-reboot'
```

`XKMAKE_ARGS` is forwarded to xkmake; `QEMU_ARGS` is appended verbatim after `--` and is the right place for raw QEMU flags. `GRAPHIC=y` adds a GTK display and virtio keyboard/mouse; pair it with a desktop rootfs to run Chromium, niri, or Xorg/Xwayland; see [Desktop workloads](#desktop-workloads):

```bash
# Use a Wayland/X11-enabled desktop rootfs for these workloads.
make run GRAPHIC=y GPU_XRES=1920 GPU_YRES=1080
```

The default `make rootfs` image is a console image. Desktop rootfs and compositor images are maintained as separate userspace artifacts; they are not required to build or boot the kernel.

### Unit tests and the PR gate

```bash
make unittest
make unittest UNITTEST_CRATE=kvfs,kprocess
make ci-test          # clones the Starry Test Harness and runs the CI suite
```

With coverage enabled (the default), successful unit-test runs generate `coverage.txt`, `coverage.info`, and `coverage.xml` under `target/<rust-target>/<profile>/`.

### Cross toolchains

These are needed for uapps, TEE tests, and Harness guest cases; the kernel build does not compile C.

| Architecture | GCC | Musl | Download |
|---|---|---|---|
| x86_64 | 11.2.1 | git-b76f37f (2021-09-23) | [musl.cc](https://musl.cc/x86_64-linux-musl-cross.tgz) |
| aarch64 | 11.2.1 | git-b76f37f (2021-09-23) | [musl.cc](https://musl.cc/aarch64-linux-musl-cross.tgz) |
| riscv64 | 11.2.1 | git-b76f37f (2021-09-23) | [musl.cc](https://musl.cc/riscv64-linux-musl-cross.tgz) |
| loongarch64 | 13.2.0 | 1.2.5 | [LoongsonLab](https://github.com/LoongsonLab/oscomp-toolchains-for-oskernel/releases/download/loongarch64-linux-musl-cross-gcc-13.2.0/loongarch64-linux-musl-cross.tgz) |

## Platform Support

### Supported targets

| Target | Defconfig | CI coverage | Status | Notes |
|---|---|---|---|---|
| x86_64 QEMU | [`qemu_defconfig`](platforms/kplat-x86_64/qemu_defconfig) | Build, unit, runtime, TEE, dylint | CI-tested | Default x86_64 target. |
| x86_64 CSV | [`qemu_csv_defconfig`](platforms/kplat-x86_64/qemu_csv_defconfig) | None | Manual | CSV HUK + attestation; [`scripts/start.sh`](scripts/start.sh); OVMF and SEV artifacts required. |
| AArch64 QEMU | [`qemu_defconfig`](platforms/kplat-aarch64/qemu_defconfig) | Build, unit, runtime, TEE, dylint | CI-tested | Default AArch64 target. |
| RISC-V 64 QEMU | [`qemu_defconfig`](platforms/kplat-riscv64/qemu_defconfig) | Build, unit, runtime | CI-tested | No TEE tests. |

### Experimental and roadmap targets

| Target | Defconfig | CI coverage | Status | Notes |
|---|---|---|---|---|
| AArch64 crosvm | [`qemu_crosvm_defconfig`](platforms/kplat-aarch64/qemu_crosvm_defconfig) | Build check | Build-only | Runtime is not exercised in PR CI. |
| AArch64 virtCCA | [`qemu_virtcca_defconfig`](platforms/kplat-aarch64/qemu_virtcca_defconfig) | Build check | Build-only | HUK derivation only; no attestation implementation in tree. |
| AArch64 RK3588 | [`rk3588_defconfig`](platforms/kplat-aarch64/rk3588_defconfig) | None | Manual | Minimal UART/GIC/timer/SMP boot; VOP2 display lives on a feature branch, not `main`. |
| LoongArch64 QEMU | [`qemu_defconfig`](platforms/kplat-loongarch64/qemu_defconfig) | None | Manual | Runtime CI is on the roadmap. |
| Kylin-x pKVM | No defconfig | None | Experimental | `kvm-guest-mem-share` / `kvm-mmio-guard` feature flags only. |

Status definitions:

- **CI-tested** — build, boot, and unit/runtime tests run in the PR pipeline.
- **Build-only** — compiled and linted in CI; runtime is not exercised.
- **Manual** — a documented run path exists, but there is no automated CI.
- **Experimental** — feature-flag or feature-branch work, not a default `main` target.

CI stage definitions are in the [Jenkinsfile](Jenkinsfile).

## Capability Status

Status: **Implemented** means present and exercised by a test or workload; **Partial** means present but covering a known subset; **Not yet** means not available in `main`. The Validation column records whether the evidence is CI, unit tests, or manual only.

| Area | Status | Validation | Scope and known gaps | Source |
|---|---|---|---|---|
| Memory management | Implemented | CI (unit) | Address spaces, faults, COW, `mmap`/`brk`; Linux-semantics subset. | `mm/` |
| Process and scheduling | Implemented | CI (unit + Harness) | `fork`/`exec`, `wait*`, signals, futex, timers, EEVDF scheduling. | `process/`, `task/` |
| VFS and path resolution | Implemented | CI (unit + Harness) | `namei`, mount traversal, `openat2` `RESOLVE_*` constraints; Linux subset. | `fs/kvfs` |
| ext4 | Partial | Unit + Harness | Read/write and project quota; no full xfstests pass. | `fs/filesystems/kext4` |
| Other filesystems | Partial | Unit | tmpfs/ramfs, procfs, sysfs, devfs/devpts, pipefs, 9p, fat, cgroup2fs, bpffs; coverage varies. | `fs/filesystems/` |
| Networking | Partial | Unit + Harness | TCP/IP, Unix sockets, netfilter, vsock, netlink, TIPC; Linux subset. | `net/knet` |
| Drivers and device model | Implemented | Unit + build | Typed driver core; virtio blk/net/gpu/input/console; PCI and IRQ infrastructure. | `drivers/`, `io/` |
| Graphics (kernel) | Partial | Unit + manual | DRM/KMS, virtio-gpu 2D/3D, EDID, page-flip/vblank, fbdev; single-scanout virtio-gpu path. | `io/drmdevice`, `io/fbdevice`, `io/inputdev` |
| Desktop workloads | Implemented | Manual | Chromium (Wayland), niri, and Xorg/Xwayland; keyboard/mouse input and host-window resize. | userspace compositor + `io/drmdevice`, `io/inputdev` |
| TEE | Implemented | CI | TA (Trusted Application) lifecycle, secure storage, SM2/ECC/RSA via RustCrypto; CSV HUK + attestation; virtCCA HUK. | `tee/`, [`docs/tee.md`](docs/tee.md) |
| Container guest | Partial | Manual | `docker run --network=host --rm hello-world` smoke; user-namespace and cgroup gaps remain. | `uapps/mini-oci`, [`docs/container-architecture.md`](docs/container-architecture.md) |

### Desktop workloads

The desktop path uses the kernel's DRM/KMS, virtio-gpu (2D/3D, EDID), and virtio-input drivers. Validated userspace includes:

- **Chromium (Wayland)** — windowing, frame callbacks, and keyboard/mouse input.
- **niri** — reference compositor disk built from niri v26.04; host-window resize and pointer/keyboard input.
- **Xorg/Xwayland** — X11 workloads through the Xorg/Xwayland path.

Compositor and desktop rootfs images are maintained as separate userspace artifacts; the kernel build and boot do not depend on them. The reference desktop image is maintained in [openkylin/x-agentos](https://gitee.com/openkylin/x-agentos).

## Design

X-Kernel is monolithic at runtime but componentized by contract. The security value is API isolation and scoped audit, not runtime isolation between crates.

- **API-first ownership.** `mm/memspace`, `core/kuaccess`, `process/kexec`, and `kirq`/`device_res` are separate contracts, not implementation details of one catch-all crate. More than 200 crates define the ownership map.
- **Audit boundaries.** Each crate carries `docs/design.md` and `docs/security.md` where present; a reviewer can scope a change to its API contract and downstream users.
- **Unsafe boundaries.** Every `unsafe` block needs a `SAFETY:` justification; unsafe-heavy areas (MMIO, DMA, context switch, TEE glue) stay behind subsystem APIs.
- **Isolation boundaries.** Crate boundaries are compile-time, not privilege boundaries. TEE provides the runtime isolation for TA/guest workloads; the kernel remains a single address space.

```text
Linux userspace: systemd / Docker / Wayland / X11
------------------------------------------------
ABI: ksyscall / posix / tee
------------------------------------------------
subsystems: mm | process/task | fs | net | drivers/io
------------------------------------------------
arch HAL: x86_64 | AArch64 | RISC-V 64 | LoongArch64
------------------------------------------------
platforms: QEMU | crosvm | virtCCA | CSV | RK3588
```

## Verification

The project uses three primary verification layers (in-kernel unit tests, external [Starry Harness](https://gitee.com/openkylin/starry-test-harness) regressions, and TEE tests), plus desktop and performance workload tracks.

| Layer | How it runs | Scale | CI coverage |
|---|---|---|---|
| Kernel unit tests | `make unittest` | 4,733 `#[def_test]` cases; 1,167 `#[test]` cases; 122 `ut-tests` directories / 797 files | x86_64, AArch64, RISC-V |
| PR syscall regression | `make ci-test` (clones the Starry Harness) | 40 cases: 38 guest + 1 boot + 1 powercut | x86_64, AArch64, RISC-V |
| Iteration suite | Harness `make ci-test-iter run` | 14 cases: 13 guest + 1 powercut | Not a PR gate |
| Debian APT suite | Harness `make ci-debian run` | 1 APT guest case | Not a PR gate |
| TEE tests | Jenkins `TEE Tests` | Secure-storage and crypto regressions in `tee_apps/` | x86_64, AArch64 |
| Desktop workloads | `make run GRAPHIC=y` with a desktop rootfs | Chromium (Wayland), niri, Xorg/Xwayland | Manual; not PR CI |
| Performance and longevity | Harness `daily-test` / `longevity-test` | 37 daily cases; 9 longevity cases; results in [Performance](#performance) | External Jenkins jobs |

Notes:

- Counts are from Harness commit `2dcb0616` (2026-10-26); suite membership and case counts will change.
- The 51 guest cases are 38 from `ci` plus 13 from `ci-test-iter`; the 1 `ci-debian` APT case is separate and is not part of the PR gate.
- `ci` is the Harness directory name; the suite is registered as `ci-test` in `tests/ci/suite.toml`. The X-Kernel entry point is `make ci-test`.
- Dated pass-rate reports for LTP, Open POSIX, pjdfstest, xfstests, libc-test, and Go stdlib are not yet published. The Harness contains the workload definitions; the reports are a roadmap item.
- The x86_64 ABI table contains 467 entries; implemented and guest-tested counts are not yet published.

The [Jenkinsfile](Jenkinsfile) defines 18 arch/check stages (excluding Prepare/Post and the optional documentation check): build and unit/runtime tests for three architectures, TEE tests for two, crosvm/virtCCA build checks, and dylint for x86_64/AArch64.

## Performance

Source: [daily-test #139](https://jenkins.openkylin.top/job/daily-test/139/) (2026-11-11, kernel `8f866f9`, Harness `150d0c7`), with the Linux baseline from the same run and the same QEMU contract: 4 vCPU, 1 GiB, QEMU 11.0.2, virtio-blk with 4 queues, virtio-net user-net (SLIRP), 4k fio on ext4. The build is `UNSTABLE` because the dashboard reported nine metric-level regressions; the measured workloads completed and the CSV/`results.json` values are used below.

| Workload | x86_64 X-Kernel | x86_64 Linux | Δ | AArch64 X-Kernel | AArch64 Linux | Δ |
|---|---:|---:|---:|---:|---:|---:|
| UnixBench syscall | 868.4 | 809.6 | +7.3% | 210.7 | 319.1 | -34.0% |
| schbench p99 wakeup (µs) | 9 | 3,724 | -99.8% | 27 | 6,088 | -99.6% |
| schbench average RPS | 3,372 | 1,213 | +178.0% | 616 | 416 | +48.0% |
| fio 4k randread (IOPS) | 1,022,242 | 1,281,667 | -20.2% | 437,514 | 509,865 | -14.2% |
| fio 4k randwrite (IOPS) | 724,072 | 756,976 | -4.3% | 237,044 | 404,874 | -41.5% |
| iperf3 guest→host TCP (Mbps) | 65.5 | 2,256.4 | -97.1% | 14.9 | 1,484.4 | -99.0% |
| iperf3 host→guest TCP (Mbps) | 145.8 | 3,479.6 | -95.8% | 59.2 | 2,595.6 | -97.7% |

Δ = X-Kernel relative to Linux; lower is better for latency. x86_64 compute/scheduler results lead Linux, and fio write is close to parity; fio read and both TCP directions are behind. AArch64 is mixed: scheduler RPS leads, while UnixBench syscall, fio, and TCP throughput trail. TCP throughput is the largest remaining gap and should not be presented as "close to Linux" until it is fixed.

Hosts: x86_64 `xkernel-amd64-01` (Xeon Gold 6548Y+), AArch64 `xkernel-arm64-01` (Kunpeng 920). Raw logs, CSV ratios, and Linux `results.json` are linked from the [build page](https://jenkins.openkylin.top/job/daily-test/139/).

## Configuration

```bash
make menuconfig        # edit the configuration
make olddefconfig      # non-interactive refresh after Kconfig changes
make savedefconfig     # write a minimal ./defconfig
```

`make defconfig` expands a copied platform `defconfig` into a full `.config`. Build output is placed under `target/xkmake/<platform>/<profile>/`. Kernel configuration and build orchestration are described in [`docs/xkmake-design.md`](docs/xkmake-design.md).

## Source Layout

| Path | Role |
|---|---|
| `arch/` | Architecture HAL: CPU, exceptions, paging, timers, IPI. |
| `boot/` | Boot protocol handoff, early stubs, kernel image parsing. |
| `core/` | Syscall dispatch, runtime, tracing, user-memory access, services. |
| `mm/` | Address spaces, VMAs, faults, page cache, allocators. |
| `process/`, `task/` | Processes, threads, signals, scheduling, futexes. |
| `fs/` | VFS (`fs/kvfs`) and filesystem implementations. |
| `net/` | TCP/IP stack, sockets, netfilter, vsock, netlink. |
| `drivers/`, `io/` | Driver core, virtio, PCI, interrupt and display/input subsystems. |
| `tee/` | TEE kernel, TA lifecycle, secure storage, crypto. |
| `platforms/` | Platform glue and platform `defconfig`s. |
| `util/` | Shared utility crates. |

Crate-local design and threat-model notes live in `docs/design.md` and `docs/security.md` next to the code, where present.

## Documentation

- [Release policy](docs/release.md) and [v0.1.0-2606 release notes](docs/releases/v0.1.0-2606.md)
- [TEE build guide](docs/tee.md)
- [Container architecture](docs/container-architecture.md)
- [Jenkins pipeline contract](docs/jenkins-pipeline.md)
- [XKMake design](docs/xkmake-design.md)
- [Code guidelines](docs/code-guidelines/README.md)
- [Build workflow and prerequisites](docs/ai/skills/build-workflow/SKILL.md)

## Community and Contributing

- Repository: <https://gitee.com/openkylin/x-kernel>
- Issues: <https://gitee.com/openkylin/x-kernel/issues>
- Pull requests: <https://gitee.com/openkylin/x-kernel/pulls>
- Chat: <https://x-kernel.zulipchat.com>

Before opening a pull request:

1. Read [`docs/code-guidelines/README.md`](docs/code-guidelines/README.md) and the crate-local `docs/design.md` / `docs/security.md`.
2. Run `make fmt`, `make clippy`, and the narrowest relevant `make unittest` command.
3. Run `make ci-test` when the change affects syscall or userspace behavior.
4. Keep refactoring commits separate from feature or behavior commits, and update design/security documentation with the code.

## Security

X-Kernel uses Rust's memory-safety model, explicit `unsafe` boundaries with `SAFETY:` justifications, and TEE isolation for confidential-computing workloads. Host-side VulnHunter scans are run separately from PR CI and require API credentials.

Please report suspected vulnerabilities privately to the maintainers rather than opening a public issue. A formal `SECURITY.md` is not yet present.

## Known Limitations

- Linux ABI coverage is a subset; some syscalls, filesystem/network features, and memory-management paths remain incomplete. Dated public pass-rate reports are not yet published.
- Platform maturity varies; CSV, RK3588, pKVM, LoongArch, crosvm, and virtCCA have limited or no PR CI, and some require manual or feature-flag setup.
- Desktop and container workloads are validated manually; PR CI covers the kernel-side build and unit paths, not end-to-end desktop or OCI runs.
- Performance is workload-dependent: compute, scheduler, and pagecache results are competitive; storage and TCP throughput still trail Linux.

## Roadmap

- Publish dated, reproducible pass-rate reports for the conformance suites already wired into the Harness.
- Add LoongArch64 runtime CI.
- Add desktop end-to-end workload runs (Chromium, niri, Xorg/Xwayland) to CI.
- Broaden validated desktop applications and display hardware (multi-monitor, more modes, more GPU configurations).
- Bring RK3588 VOP2 display support into `main`.
- Complete user-namespace and cgroup support for full OCI containers.
- Track new Linux syscalls and ABI revisions.
- Close the storage/TCP throughput gaps shown by daily-test #139.
- Publish a stable performance report with the Linux baseline configuration.
- Publish the syscall-coverage list and the counting script.

## License

Licensed under the Apache License 2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).

## Acknowledgements

X-Kernel is inspired by [StarryOS](https://github.com/Starry-OS/StarryOS) and [ArceOS](https://github.com/arceos-org/arceos). The Starry Test Harness provides the shared user-space regression framework.
