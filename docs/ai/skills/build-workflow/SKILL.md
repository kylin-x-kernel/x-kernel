# Build Workflow

Use this skill when an AI agent needs to configure, build, run,
lint, or format the X-Kernel project.

This skill is the canonical workflow for basic project operations.
Agents should follow it instead of inventing ad hoc cargo commands.

## Scope

This skill covers:

- preparing `.config` from a platform defconfig;
- interactively changing or refreshing configuration;
- building the kernel with the repository Makefile flow;
- running the kernel locally;
- running clippy with project configuration;
- formatting Rust code with the pinned toolchain.

This skill does not replace subsystem-specific test procedures,
debug workflows, or architecture-specific bring-up notes.

## Repository Assumptions

- The repository root contains `Kconfig`, `Makefile`,
  `.cargo/.xconfig.toml`, and `rust-toolchain.toml`.
- Kconfig-generated settings are part of the normal build flow.
- Bare `cargo check -p <crate>` is not a reliable substitute
  for the project Makefile flow.

## Prerequisites

- Rust toolchain `1.93.1` as pinned by `rust-toolchain.toml`
- Rust targets:
  `x86_64-unknown-none`,
  `aarch64-unknown-none-softfloat`,
  `riscv64gc-unknown-none-elf`,
  `loongarch64-unknown-none-softfloat`
- `cargo-binutils`
  (the build system may auto-install it if missing)
- a musl cross-compilation toolchain for the target architecture
- QEMU for run and QEMU-based test flows

## Architecture-Target Mapping

| `.config` `ARCH` | Rust target | `CROSS_COMPILE` |
|---|---|---|
| `aarch64` | `aarch64-unknown-none-softfloat` | `aarch64-linux-musl-` |
| `riscv64` | `riscv64gc-unknown-none-elf` | `riscv64-linux-musl-` |
| `x86_64` | `x86_64-unknown-none` | `x86_64-linux-musl-` |
| `loongarch64` | `loongarch64-unknown-none-softfloat` | `loongarch64-linux-musl-` |

## Supported Platforms

Platforms with checked-in defconfig:

| Platform | Description |
|---|---|
| `aarch64-qemu-virt` | QEMU ARM64 Virtual Machine |
| `riscv64-qemu-virt` | QEMU RISC-V 64 Virtual Machine |
| `x86_64-qemu-virt` | QEMU x86_64 (q35) |
| `x86-csv` | Hygon CSV (SEV) Platform |

Additional checked-in defconfig variants:

| Defconfig Path | Description |
|---|---|
| `platforms/aarch64-qemu-virt/crosvm_defconfig` | Capability bundle for the legacy CrosVM guest setup, built on the shared `aarch64-qemu-virt` platform crate |
| `platforms/aarch64-qemu-virt/ramdisk_defconfig` | RAM disk as the root filesystem: `KFEAT_DRIVER_RAMDISK_STATIC=y` (ext4 image embedded via `make ramdisk_img`) with `KFEAT_ROOT_BLOCK="ramdisk"`. virtio-blk may stay enabled (it coexists as a secondary data disk); boot with `make run` |

## Required Preparation

Before `make build`, `make run`, `make clippy`, `make unittest`,
or QEMU-based unit-test commands,
prepare `.config` from a platform defconfig.

Default example:

```bash
cp platforms/aarch64-qemu-virt/defconfig .config
make defconfig
```

If the task targets a different platform,
replace the defconfig path accordingly.
Do not assume an existing `.config` is correct for the requested target.
Copying a platform defconfig alone is not sufficient;
the copied file is only a minimal seed and must be expanded
with `make defconfig` before Make-based build, run, lint,
or unit-test commands.

Exception:

- `loongarch64-qemu-virt` currently has no checked-in platform defconfig;
  start from `make menuconfig` instead of copying `platforms/<platform>/defconfig`.

## Defconfig Workflow

In this repository,
`platforms/*/defconfig` files are minimal, checked-in baseline configurations.
They are not the full configuration consumed by the build.

The normal relationship is:

- `platforms/<platform>/defconfig`:
  minimal platform baseline stored in the repository;
- `.config`:
  expanded working configuration in the repository root;
- `./defconfig`:
  a minimized configuration generated from the current `.config`
  by `make savedefconfig`.

### `make defconfig`

`make defconfig` does not choose a platform by itself.
It expects `.config` to already exist,
usually because a platform `defconfig` was copied there first.
In this repository, `make build`, `make run`, `make clippy`,
and unit-test Make targets expect that expansion step
to have already happened.

Typical usage:

```bash
cp platforms/aarch64-qemu-virt/defconfig .config
make defconfig
```

What it does:

- reads the current root `.config`;
- fills in omitted symbols from `Kconfig` defaults;
- writes back a complete expanded `.config`.

Use it when:

- starting work on a specific platform;
- switching from one platform to another;
- resetting the working config to a known platform baseline.

If `.config` does not exist,
the Makefile will fail and ask you to copy a platform defconfig first.

### `make olddefconfig`

Use `make olddefconfig` when `.config` already exists
and you want to refresh only newly added Kconfig symbols
from their defaults without replacing the rest of the current configuration.

Typical usage:

```bash
make olddefconfig
```

Use it when:

- rebasing onto upstream changes that introduced new Kconfig options;
- keeping an existing local `.config`
  while accepting default values for new symbols.

Do not use `olddefconfig` as a substitute
for selecting a different platform baseline.

### `make menuconfig`

Use `make menuconfig` when you need to interactively inspect
or modify configuration values.

Typical usage:

```bash
make menuconfig
```

Use it when:

- changing configuration intentionally;
- exploring available Kconfig options;
- starting configuration for targets
  that do not have a checked-in platform defconfig.

### `make oldconfig`

Use `make oldconfig` when `.config` already exists
and you want the Linux-style interactive refresh flow
for newly introduced Kconfig symbols.

Typical usage:

```bash
make oldconfig
```

Use it when:

- rebasing onto upstream changes that introduced new Kconfig options;
- you want to review each newly introduced symbol manually
  instead of accepting defaults automatically.

### `make savedefconfig`

`make savedefconfig` minimizes the current root `.config`
and writes the result to `./defconfig`.

Typical usage:

```bash
cp platforms/aarch64-qemu-virt/defconfig .config
make menuconfig
make savedefconfig
```

What it does:

- compares the current `.config`
  against Kconfig defaults;
- keeps only non-default selections;
- writes the minimized result to a file named `defconfig`
  in the repository root.

Use it when:

- you intentionally changed configuration values;
- you want to update a checked-in platform defconfig;
- you need a reviewable minimal config delta
  instead of committing a full expanded `.config`.

Important:

- `make savedefconfig` does not update
  `platforms/<platform>/defconfig` automatically.
- Run `make defconfig`, `make menuconfig`, `make oldconfig`,
  `make olddefconfig`, or `make saveconfig` before `make savedefconfig`;
  `savedefconfig` is only valid on a prepared working `.config`.
- After generating `./defconfig`,
  compare it with the target platform defconfig
  and copy it into the correct platform directory if appropriate.

Typical update flow for a platform defconfig:

```bash
cp platforms/aarch64-qemu-virt/defconfig .config
make defconfig
make menuconfig
make savedefconfig
cp defconfig platforms/aarch64-qemu-virt/defconfig
```

Before committing an updated platform defconfig,
re-run the standard expansion flow to verify it is self-consistent:

```bash
cp platforms/aarch64-qemu-virt/defconfig .config
make defconfig
make build
```

## Standard Commands

Build:

```bash
make build
```

Verbose build output:

```bash
make build V=1
make build V=2
```

Expected build outputs may include:

| File | Description |
|---|---|
| `xkernel_<platform>.elf` | ELF binary |
| `xkernel_<platform>.bin` | Raw binary image |
| `xkernel_<platform>.uimg` | U-Boot image when `UIMAGE=y` |
| `xkernel_x86_64-*.bzimg` | x86_64 LinuxBoot image |
| `xkernel_x86_64-*.uefi.img` | x86_64 UEFI boot disk when `UEFI=y` |

Clippy:

```bash
make clippy
```

Notes:

- `make clippy` also enforces `clippy::undocumented_unsafe_blocks`;
  `unsafe { ... }` blocks should carry a nearby `SAFETY:` explanation.

Run:

```bash
make run
```

Run without rebuilding:

```bash
make justrun
```

Host prerequisites for `make run`:

- the host must allow QEMU to access the configured `vhost-vsock` device;
- TCP/UDP port `5555` must be free for QEMU `hostfwd`;
- do not start multiple QEMU instances in parallel
  with the default networking arguments,
  or they may contend for the same forwarded port.

Useful variants:

```bash
make run UEFI=y
make run VSOCK=n
```

Notes:

- `make run UEFI=y` is relevant for x86_64 UEFI boot.
- `make run VSOCK=n` is useful on hosts
  where vsock is unsupported or intentionally disabled,
  such as some macOS environments.

QEMU-related variables commonly used with `make run`:

| Variable | Default | Description |
|---|---|---|
| `BLK` | `y` | Enable virtio-blk storage |
| `NET` | `y` | Enable virtio-net networking |
| `GRAPHIC` | `n` | Enable virtio-gpu display |
| `VIRTIO_BUS` | `pci` | Device bus: `pci` or `mmio` |
| `MEM` | `1g` | Memory size |
| `ACCEL` | auto | Hardware acceleration: `y` or `n`. Auto enables KVM only when `/dev/kvm` is a character device (not a path stub/directory). |
| `DISK_IMG` | `$(PWD)/disk.img` | Virtual disk image path |
| `UEFI` | `n` | x86_64 UEFI boot |
| `NET_DEV` | `user` | Network backend: `user`, `tap`, `bridge` |
| `IP` | `10.0.2.15` | Guest IPv4 |
| `GW` | `10.0.2.2` | Gateway IPv4 |
| `QEMU_LOG` | `n` | QEMU log to `qemu.log` |
| `NET_DUMP` | `n` | Packet capture to `netdump.pcap` |

Examples:

```bash
make run MEM=2g NET=n GRAPHIC=y
make run NET_DEV=tap
make run QEMU_LOG=y NET_DUMP=y
```

Run unit tests through QEMU:

```bash
make UNITTEST=y run
```

Filter QEMU unit tests by crate:

```bash
make UNITTEST=y run UNITTEST_CRATE=kvfs
make UNITTEST=y run UNITTEST_CRATE=kvfs,kprocess
```

Host prerequisites for `make UNITTEST=y run`:

- the host must allow QEMU to access the configured `vhost-vsock` device;
- TCP/UDP port `5555` must be free for QEMU `hostfwd`;
- do not run it in parallel with `make run`
  or another `make UNITTEST=y run`
  unless the QEMU networking arguments are changed.

Run the unit-test target:

```bash
make unittest
```

Additional test variants:

```bash
make unittest_no_fail_fast
```

Notes:

- `make unittest` is the host-side Rust test entry.
- `make unittest_no_fail_fast` is the host-side variant
  that continues after individual test failures.
- `UNITTEST_CRATE` is for the QEMU kernel-unit-test path
  (`make UNITTEST=y run`),
  not the host-side `make unittest` path.

Format Rust code:

```bash
cargo +nightly-2026-03-08 fmt --all
```

Pre-commit hooks (shared, auto-enabled):

A shared `pre-commit` hook is committed under `.githooks/` and runs
`make fmt` (auto-formats and re-stages the staged Rust files) followed by
`make clippy`, so commits that would fail CI on those two checks are
blocked locally first.

- It is enabled automatically on any `make ...` invocation — no manual
  setup is needed after `git clone` or `git pull`. To (re)enable it
  explicitly in a clone, run `make hooks`.
- It only runs when at least one `*.rs` file is staged.
- Skip a check for a single commit with environment variables:
  - `SKIP_FMT=1 git commit ...` — skip `make fmt`;
  - `SKIP_CLIPPY=1 git commit ...` — skip `make clippy`;
  - `SKIP_ALL=1 git commit ...` — skip both.
- `make clippy` requires a `.config`; if none is present the hook skips
  clippy for that commit (CI still checks it, so prepare a `.config`
  before pushing).

Clean generated artifacts:

```bash
make clean
make distclean
make clean_c
```

Debug-oriented commands:

```bash
make debug
make disasm
make justrun QEMU_ARGS="-s -S"
```

Root filesystem helpers:

```bash
make rootfs
make disk_img
make ramdisk_img
```

`make ramdisk_img` builds the filesystem image (`ramdisk.img`) embedded into
the kernel as the RAM disk root filesystem when
`KFEAT_DRIVER_RAMDISK_STATIC=y` is selected. The image path, size, and format
are configurable:

| Variable | Default | Description |
|---|---|---|
| `RAMDISK_IMG` | `$(PWD)/ramdisk.img` | Image embedded into the RAM disk |
| `RAMDISK_IMG_SIZE` | `8` | Image size in MiB |
| `RAMDISK_IMG_FS` | `ext4` | Image format (`ext4` or `fat32`) |
| `RAMDISK_ROOTFS` | `$(DISK_IMG)` | Source image to extract a minimal shell from |

When `RAMDISK_ROOTFS` exists and `RAMDISK_IMG_FS=ext4`, `make ramdisk_img`
extracts `/bin/busybox` plus the musl runtime (`/lib/ld-musl-aarch64.so.1`)
from it and injects them (with applet symlinks and standard directories) into
the image via `mkfs.ext4 -d`, so the default ramdisk boots to a shell without
embedding a full rootfs. Set `RAMDISK_ROOTFS=` to build a truly empty image.

`RAMDISK_IMG_FS` must match the selected filesystem backend
(`KFEAT_FS_EXT4` / `KFEAT_FS_FAT`). Override `RAMDISK_IMG` to embed a custom
image. The image is embedded in `.data`, so the kernel binary grows by roughly
the image size.

For rootfs-based runtime customization,
the repository also documents workflows
that inject files into `disk.img`
without changing kernel source code.

## Agent Rules

When an agent needs to validate a code change:

1. Select the appropriate platform defconfig.
2. Copy it to the repository root as `.config`.
3. Refresh `.config` with `make defconfig`.
4. Run the narrowest meaningful validation for the task.
5. Prefer Makefile targets over bare cargo commands.

When an agent changes checked-in platform configuration:

1. Start from `platforms/<platform>/defconfig`.
2. Copy it to `.config`.
3. Expand it into `.config` with `make defconfig`.
4. Make the intended config changes.
5. Run `make savedefconfig`.
6. Move the generated root `./defconfig`
   into `platforms/<platform>/defconfig`.
7. Re-expand and validate again before finishing.

Examples:

- For a normal compile fix, run `make build`.
- For lint-related changes, run `make clippy`.
- For formatting-only edits, run the pinned `cargo fmt` command.
- For boot or runtime changes, run `make run`
  or the relevant unit-test flow.
- For host-side Rust unit tests, run `make unittest`
  or `make unittest_no_fail_fast`.
- For kernel-side unit tests with crate filtering,
  run `make UNITTEST=y run UNITTEST_CRATE=<crate-list>`.
- Before `make build`, `make run`, `make clippy`,
  or `make UNITTEST=y run`,
  prepare `.config` by copying the platform defconfig
  and then running `make defconfig`.
- Before `make run` or `make UNITTEST=y run`,
  check host prerequisites such as `vhost-vsock` access
  and whether port `5555` is already in use.

## Avoid

- Do not use bare `cargo check -p <crate>`
  as the main validation path.
- Do not skip `make defconfig` after copying a platform defconfig.
- Do not skip `.config` preparation before Make-based build,
  run, lint, or unit-test commands.
- Do not commit a hand-edited expanded `.config`
  as a substitute for updating a platform `defconfig`.
- Do not assume `make savedefconfig`
  writes directly into `platforms/<platform>/defconfig`.
- Do not run multiple QEMU-based commands in parallel
  with the default `hostfwd` settings.
- Do not assume `make run` or `make UNITTEST=y run`
  will work on a host without `vhost-vsock` access.
- Do not silently switch toolchains;
  use the version pinned by `rust-toolchain.toml`
  or the explicitly documented formatting command.

## Related Documents

- `AGENTS.md`
- `rust-toolchain.toml`
- `platforms/*/defconfig`
- `README.md`
