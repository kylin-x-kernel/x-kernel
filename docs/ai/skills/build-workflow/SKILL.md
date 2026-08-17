# Build Workflow

Use this skill to configure, build, and run X-Kernel through the repository
Makefile and XKMake.

## Architecture

```text
Makefile -> xkmake -> xconfig + Cargo + image tools + QEMU
```

- `xconfig` owns Kconfig evaluation and generated configuration.
- `xkmake` owns build, bundle, rustdoc, and QEMU orchestration.
- Cargo owns Rust compilation and incremental builds.

During an XKMake build, xconfig parses and evaluates Kconfig once. All
generated configuration files consume the resulting resolved snapshot; build
stages must not call the standalone `gen-const` or `gen-cargo` CLI paths.
XKMake also renders the target linker script from the repository-root
`linker.lds.S` and that snapshot before Cargo runs. It must preserve the
existing file and mtime when the rendered content is unchanged so consecutive
builds remain incremental.

Do not use a bare `cargo check -p <crate>` as the main kernel validation path.

## Prerequisites

- the Rust toolchain and targets from `rust-toolchain.toml`;
- `cargo-binutils` and `rust-objcopy`;
- the target architecture's musl cross toolchain;
- the matching QEMU system emulator for `make run`;
- `cargo-shear 1.13.2` and `licensure 0.8.1` for repository hygiene checks;
- an ext4 `disk.img` when the configured virtio block device is enabled.
- for unit-test coverage: `debugfs`, `rust-profdata`, and `rust-cov`;
- for x86_64 boot media: `mkfs.fat`, mtools (`mmd` and `mcopy`), and OVMF/EDK2
  firmware for UEFI execution.

Install the pinned repository hygiene tools with:

```bash
make install-tools
```

This delegates to XKMake so the installer and version checks share the same
pinned version constants. Cargo installs these executables under
`~/.cargo/bin` by default; that directory must be present in `PATH` for
`make clippy` and the standalone hygiene targets.

## Configuration

Start from an explicit platform defconfig:

```bash
cp platforms/aarch64-qemu-virt/defconfig .config
make defconfig
```

Supported configuration commands:

```bash
make defconfig
make oldconfig
make olddefconfig
make menuconfig
make saveconfig
make savedefconfig
```

`make build` also expands an existing seed `.config` automatically. It does
not silently select a platform when `.config` is missing.

## Build

```bash
make build
```

The canonical output is a bundle:

```text
target/xkmake/<platform>/<profile>/
├── bundle.toml
├── kernel.elf
└── kernel.bin
```

x86_64 bundles additionally contain both supported boot media:

```text
kernel.bzimg
kernel.uefi.img
```

Successful builds also refresh compatibility copies in the repository root:

```text
xkernel_<platform>.elf
xkernel_<platform>.bin
xkernel_<platform>.bzimg      # x86_64 only
xkernel_<platform>.uefi.img   # x86_64 only
```

The bundle remains the canonical output; Jenkins and existing repository tools
use the root-level names for artifact handoff compatibility.

Each bundle ELF contains an allocated `.note.xkernel.build-info` note and a
SHA-256 `.note.gnu.build-id`. Inspect them with:

```bash
readelf -n target/xkmake/<platform>/<profile>/kernel.elf
```

XKMake always invokes Cargo, allowing Cargo to validate source freshness. It
reuses the image-processing result when the resolved configuration, build
inputs, Cargo ELF, and bundle manifest still match.

Normal `build` reuse is a trusted local-cache fast path. It validates the
atomically published manifest, artifact names, sizes, mtimes, and build inputs
without hashing the complete ELF. `run --no-build` is an artifact-consumption
boundary and recomputes the loadable-image SHA-256 before accepting the bundle.

The embedded `build_time` is the UTC time at which XKMake actually regenerates
the bundle. Automatic wall-clock time does not participate in cache matching:
an unchanged build reuses the existing ELF, Build ID, and build time. Explicit
`KBUILD_BUILD_TIME` and `SOURCE_DATE_EPOCH` values are build inputs and can
force regeneration when they differ from the bundled value.

Direct tool invocation is available for diagnostics:

```bash
cargo xkmake build -v
cargo xkmake build --dry-run -v
```

## Documentation

Generate workspace rustdoc with the resolved Kconfig feature set:

```bash
make doc
make doc XKMAKE_ARGS='--open'
```

Enforce rustdoc coverage on public APIs in addition to broken-link checks:

```bash
make doc_check_missing
```

XKMake consumes the same one-pass resolved configuration snapshot and Rust
target used by the build flow. Documentation stages must not recover features
by parsing `.cargo/.xconfig.toml` or document kernel crates for the host target.

## Run

```bash
make run
```

XKMake supports QEMU boot for:

- `aarch64-qemu-virt`;
- `riscv64-qemu-virt`;
- `loongarch64-qemu-virt`;
- `x86_64-qemu-virt`.

x86_64 defaults to LinuxBoot. Select the UEFI image with either form:

```bash
make run UEFI=y
make run XKMAKE_ARGS='--boot uefi'
```

XKMake locates common OVMF/EDK2 installations automatically. Use
`--ovmf-code` and `--ovmf-vars-template`, or the corresponding
`OVMF_CODE`/`OVMF_VARS_TEMPLATE` Make variables, for custom firmware paths.

Pass options through `XKMAKE_ARGS`:

```bash
make run XKMAKE_ARGS='--memory 2g --smp 2'
make run XKMAKE_ARGS='--no-net --no-block'
make run XKMAKE_ARGS='--disk-image images/rootfs.img'
make run XKMAKE_ARGS='--no-vsock'
make run ACCEL=n
make run QEMU_ARGS='-d guest_errors'
make run QEMU_ARGS='-no-reboot'
```

`QEMU_ARGS` forwards arbitrary extra arguments straight to the QEMU invocation
(they are appended after `--`), covering ad-hoc devices or debug flags that
don't have a dedicated Make variable.

Important defaults:

- memory: `1g`;
- SMP: configured `NR_CPUS`;
- disk image: `disk.img`;
- virtio block and network: enabled when compiled into the kernel;
- vsock: automatically attached when the kernel driver and selected QEMU
  device model both support it; otherwise XKMake warns and continues;
- acceleration: `KFEAT_VMM=y` forces QEMU TCG even when `ACCEL=y`, because
  CI hosts usually expose KVM only to the outer QEMU and do not support nested
  virtualization;
- display: serial/nographic unless `--graphic` is supplied;
- guest IP: `10.0.2.15`;
- gateway: `10.0.2.2`.

Guest IP and gateway are currently build inputs because the network stack uses
compile-time environment values:

```bash
make build XKMAKE_ARGS='--guest-ip 10.0.2.20 --gateway 10.0.2.2'
```

## Repository Utilities

The thin Makefile retains repository-level utility targets that do not own
kernel configuration or QEMU policy:

```bash
make rootfs
make uapps
make rootfs-uapps
make teefs
make disk_img
make ramdisk_img
make install-tools
make check_deps
make check_header
make doc
make doc_check_missing
make fmt
make clean
make distclean
```

`make check_deps` runs the pinned `cargo-shear 1.13.2` analyzer through
`xkmake hygiene deps`. Use `make deps` to apply its unused-dependency fixes.
The check covers the root, `xtask`, `tee_apps`, and `uapps/hello` Cargo
workspaces. Cargo-shear warnings remain advisory; dependency errors fail the
command.

`make check_header` runs the pinned `licensure 0.8.1` analyzer through
`xkmake hygiene header`. Use `make header` to add or update the configured
three-line header. XKMake passes tracked and non-ignored untracked Rust source
files to licensure; deleted worktree paths and non-Rust files are not checked.

`make unittest` means a kernel unit-test build followed by QEMU execution. It
is equivalent to the former `make run UNITTEST=y` workflow:

```bash
make unittest
make unittest UNITTEST_CRATE=kvfs,kprocess
```

After a successful unit-test QEMU run, XKMake extracts
`/.llvm-cov/default.profraw` from the configured disk image and writes these
artifacts under `target/<rust-target>/<profile>/`:

```text
default.profraw
default.profdata
coverage.txt
coverage.info
coverage.xml
```

Extraction or report-generation failures fail the run. Old coverage artifacts
are removed before processing so they cannot be mistaken for current output.

`make justrun` runs an existing bundle without invoking Cargo.
`make debug` builds first, starts QEMU with the GDB stub paused, and attaches
the configured `GDB` command. `make disasm` consumes the bundle ELF.

`make ci-test` builds the kernel, clones or updates the Starry test harness
into the ignored `ci-test/` directory, and runs its complete `ci-test` suite
against the current X-Kernel tree. Override `CI_TEST_REPO`, `CI_TEST_BRANCH`,
`CI_TEST_ARCH`, `CI_TEST_CASES`, or `CI_TEST_JOBS` when narrowing or
relocating the run.

```bash
make ci-test
CI_TEST_ARCH=riscv64 CI_TEST_CASES=timerfd-semantics make ci-test
```

## Validation

For build-system changes, run:

```bash
cargo test --manifest-path xtask/Cargo.toml -p xconfig -p xkmake
cargo clippy --manifest-path xtask/Cargo.toml -p xkmake --no-deps -- -D warnings
make build
make run
```

Use a bounded QEMU run in automation and verify kernel boot output before
terminating it.

## Formatting

Use the repository-pinned formatter:

```bash
cargo +nightly-2026-03-08 fmt --all
```

When formatting only host tools during development, avoid unrelated formatting
churn in untouched xtask crates.
