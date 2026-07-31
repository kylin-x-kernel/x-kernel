# XKMake Build Architecture

Status: first implementation

## Purpose

XKMake is X-Kernel's repository-local build and QEMU orchestration tool. The
stable user interface is:

```bash
make build
make run
```

The implementation follows the same broad separation used by Asterinas OSDK:
a typed Rust tool owns orchestration, Cargo owns compilation, and the runtime
command consumes explicit build artifacts. X-Kernel additionally has Kconfig,
so `xconfig` remains a separate configuration authority.

```text
Makefile -> xkmake -> xconfig -> .config and generated build files
                   -> Cargo   -> kernel ELF
                   -> image tools -> versioned bundle
                   -> rustdoc / QEMU
                   -> cargo-shear -> dependency hygiene
                   -> licensure -> Rust header hygiene
```

## Responsibility Boundaries

### Makefile

The root `Makefile` provides short, stable command names and launches host
tools in host-only target directories. It does not parse `.config`, derive
Cargo features, construct image paths, or assemble QEMU command lines.

Repository utilities such as rootfs download, uapp installation, image
creation, formatting, dependency checks, and cleanup remain Make targets.
When they need resolved product information, they query `xkmake config`
instead of parsing `.config`.

Advanced XKMake arguments are passed through one explicit variable:

```bash
make run XKMAKE_ARGS='--memory 2g --smp 2 --no-net'
```

### xconfig

`xconfig` is the sole Kconfig engine. It owns:

- parsing and evaluating Kconfig;
- expanding seed configurations and writing `.config`, `auto.conf`, and
  `autoconf.h`;
- deriving Cargo features and typed build properties;
- generating `config.rs` and `.cargo/.xconfig.toml`.

The stable build-facing facade is `xconfig::build_config`. XKMake consumes
`ResolvedKernelConfig` and `KernelBuildFiles`; it never parses `.config` text
or imports xconfig CLI implementation modules.

One `ConfigEngine` evaluation produces the complete `ResolvedKernelConfig`
snapshot: effective symbol values, authoritative symbol types, architecture,
platform, profile, virtio transport, and Cargo features. `.config`,
`auto.conf`, `autoconf.h`, `config.rs`, rust-analyzer settings, and Cargo
configuration all consume that snapshot. Generation does not reopen `.config`
or parse Kconfig again.

Generated files use write-if-content-changed semantics. This is required for
Cargo incremental compilation: merely evaluating Kconfig must not make every
kernel crate appear stale.

### xkmake

`xkmake` owns one command-scoped `BuildContext` containing the resolved
architecture, platform, Rust target, profile, paths, network build inputs, and
execution flags. Later stages use this context instead of rereading config or
environment state.

It is responsible for:

- invoking xconfig's build facade;
- rendering the target linker script from the repository-root `linker.lds.S`
  and resolved configuration before Cargo runs, without rewriting unchanged
  content;
- invoking Cargo with the resolved target and feature set;
- processing the Cargo ELF into a bundle, including loadable build metadata
  and the GNU Build ID;
- constructing x86_64 LinuxBoot and UEFI boot media;
- invoking workspace rustdoc with the resolved Cargo feature set;
- selecting a supported platform run strategy;
- invoking QEMU with structured `std::process::Command` arguments;
- returning child-process failures to Make.

Repository hygiene is exposed under `xkmake hygiene`. XKMake orchestrates
pinned external tools rather than linking their implementation stacks:
`cargo-shear` provides Cargo-aware dependency analysis, while `licensure`
checks and updates the repository's exact Rust source header template. Rust
source scope comes from Git's tracked and non-ignored untracked files.
`make install-tools` delegates to `xkmake hygiene install-tools`, keeping tool
installation and version validation on the same pinned version constants.

### Cargo

Cargo remains responsible for dependency resolution, remaining crate build
scripts, Rust compilation, and source freshness. The kernel linker script is
configuration-derived host orchestration, so XKMake generates it before Cargo
runs instead of making `kernel-boot/build.rs` both produce and watch the same
file. XKMake always invokes Cargo on `build`; it does not attempt to replace
Cargo's incremental dependency graph.

After Cargo's freshness check, XKMake reuses the existing bundle when stable
build provenance, configuration, Cargo ELF, and platform boot inputs still
match. The automatic UTC `build_time` is sampled only when image processing
actually runs, so it does not invalidate an otherwise unchanged bundle.
Explicit `KBUILD_BUILD_TIME` and `SOURCE_DATE_EPOCH` overrides remain inputs.
The ordinary build path treats the atomically published bundle as a trusted
local cache and checks artifact sizes and mtimes. `run --no-build` consumes an
existing artifact without Cargo rebuilding it, so that path additionally
recomputes the complete loadable-image SHA-256.

The `doc` flow also delegates rustdoc execution to Cargo. XKMake owns only the
configuration snapshot, feature selection, lint policy, and command boundary;
it does not parse generated Cargo TOML or implement a documentation renderer.

For unit-test runs, XKMake owns the post-QEMU coverage pipeline. It extracts
the guest profile from the configured block image, invokes the Rust LLVM
coverage tools against the bundled ELF, and converts LCOV to Cobertura in
process.

## Configuration Lifecycle

The selected product is explicit. A normal session starts with a checked-in
platform defconfig:

```bash
cp platforms/aarch64-qemu-virt/defconfig .config
make defconfig
```

`make build` and `make run` also expand an existing seed `.config` before
building. If `.config` does not exist, XKMake fails with an actionable error;
it does not silently choose a platform.

The root Makefile exposes the xconfig-owned configuration operations:

```bash
make defconfig
make oldconfig
make olddefconfig
make menuconfig
make saveconfig
make savedefconfig
```

## Build Flow

```text
validate workspace and .config
        |
        v
parse, evaluate, and snapshot Kconfig once
        |
        v
write all configuration artifacts from the snapshot
        |
        v
render linker script if its content changed
        |
        v
finalize BuildContext
        |
        v
Cargo build
        |
        v
reuse compatible bundle? -- yes --> return bundle
        |
        no
        v
copy ELF to temporary path
        |
optional DWARF embedding
        |
finalize build-info and GNU Build ID notes
        |
rust-objcopy to temporary BIN
        |
build architecture-specific boot media
        |
write temporary manifest
        |
remove old manifest, promote all artifacts, promote manifest last
```

The linker reserves allocated X-Kernel build-info and GNU Build ID notes.
XKMake fills those descriptors in place after optional DWARF embedding, then
computes the SHA-256 Build ID over the final `PT_LOAD` contents with the Build
ID descriptor zeroed. It does not append a post-link section or change segment
layout.

The manifest is the validity marker. Image processing never writes directly
to the canonical ELF or BIN. If processing or promotion fails, no new valid
manifest is published, so a later invocation cannot reuse a partial bundle.
The manifest records each canonical artifact's size; ordinary local reuse also
requires artifact mtimes not to be newer than the manifest committed last.

## Bundle Model

The canonical output location is:

```text
target/xkmake/<platform>/<profile>/
├── bundle.toml
├── kernel.elf
└── kernel.bin
```

x86_64 adds two typed boot artifacts to the same bundle:

```text
kernel.bzimg
kernel.uefi.img
```

After a successful build, XKMake also refreshes compatibility artifacts in the
workspace root using the established names:

```text
xkernel_<platform>.elf
xkernel_<platform>.bin
xkernel_<platform>.bzimg      # x86_64 only
xkernel_<platform>.uefi.img   # x86_64 only
```

These copies support Jenkins artifact handoff and existing repository tooling.
The versioned bundle and its manifest remain the source of truth for reuse and
runtime selection.

The LinuxBoot image contains the Linux protocol setup, the low-address Rust
boot stub, and the kernel ELF. XKMake parses the stub ELF symbol table and
patches the Linux boot header in process; no script or shell pipeline owns the
image format. The UEFI FAT image contains `BOOTX64.EFI`, `axboot.toml`, and
`kernel.elf`.

The versioned manifest records:

- manifest format version;
- architecture, platform, target, profile, and application package;
- compile-time guest IP and gateway;
- SHA-256 of the effective Kconfig values and other build inputs;
- canonical artifact names and byte sizes;
- the embedded GNU Build ID.

Image processing is reused only when the manifest fields and hash match, all
required artifacts match their recorded size and commit-time ordering, and the
Cargo ELF and architecture-specific boot inputs are not newer than the
published manifest. Full ELF hashing is reserved for generation and explicit
existing-artifact consumption rather than the ordinary local-cache fast path.

This is intentionally a second-level cache. Cargo decides whether source code
must be recompiled; XKMake only decides whether the resulting ELF must be
copied, optionally transformed, and converted again.

## Run Flow

`xkmake run` always performs the build flow first and then starts QEMU from the
resulting bundle. Runtime arguments are assembled as individual OS strings:

```text
memory and SMP
  + platform machine/CPU/boot arguments
  + devices enabled by Kconfig and CLI policy
  + serial or graphical output
  + explicit arguments after `--`
```

Supported QEMU platforms are:

- `aarch64-qemu-virt`;
- `riscv64-qemu-virt`;
- `loongarch64-qemu-virt`.
- `x86_64-qemu-virt`.

x86_64 uses LinuxBoot by default and accepts `--boot uefi` for OVMF execution.
UEFI firmware is resolved from explicit CLI paths, environment variables,
standard Linux locations, or the active QEMU installation. The writable OVMF
variable store is copied into `target/xkmake/runtime/<platform>/`; it is
runtime state rather than an immutable bundle artifact. Unsupported platforms
fail explicitly instead of falling through to a guessed QEMU command.

## Documentation Flow

`xkmake doc` resolves Kconfig once, writes the normal generated configuration
from that snapshot, and invokes `cargo doc --workspace --no-deps` with the
resolved Rust target and `kfeat/*` set. `--check-missing` adds `missing-docs` denial while
retaining the normal broken intra-doc link and cfg checks. `--open` and
arguments after `--` remain explicit Cargo documentation controls.

Virtio devices are added only when the corresponding driver is compiled into
the kernel and the runtime option permits the device. PCI/MMIO suffixes come
from the resolved Kconfig transport. Block images are checked before QEMU is
started.

When `--unittest` is active and QEMU exits successfully, XKMake performs this
ordered post-processing pipeline:

```text
disk.img:/.llvm-cov/default.profraw
  -> default.profraw
  -> rust-profdata -> default.profdata
  -> rust-cov report -> coverage.txt
  -> rust-cov export -> coverage.info
  -> LCOV conversion -> coverage.xml
```

All outputs live beside the Cargo ELF under
`target/<rust-target>/<profile>/`. Existing coverage artifacts are removed
before extraction so a failed stage cannot be mistaken for a current report.

## CLI

Direct invocation is useful for diagnostics:

```bash
cargo xkmake build -v
cargo xkmake build --dry-run -v
cargo xkmake config arch
cargo xkmake config bundle-elf
cargo xkmake run --memory 2g --smp 2 --no-net
cargo xkmake run --no-build
cargo xkmake run --no-vsock
cargo xkmake run --boot uefi
cargo xkmake doc --open
cargo xkmake doc --check-missing
cargo xkmake run -- --d guest_errors
```

Important defaults are:

- config: `.config`;
- target directory: `target`;
- kernel application: `entry`;
- build profile: selected by Kconfig;
- memory: `1g`;
- SMP: configured `NR_CPUS`;
- disk: `disk.img`;
- guest IP: `10.0.2.15`;
- gateway: `10.0.2.2`;
- vsock: enabled automatically when both the kernel and QEMU support it;
- graphical output: disabled.
- x86_64 boot mode: LinuxBoot.

Guest IP and gateway remain build inputs because `knet` currently reads them
through compile-time environment variables. They should eventually become
kernel command-line or runtime configuration values.

## Security And Reliability

- External programs are invoked without `sh -c` or command-string assembly.
- CLI closed sets and numeric values are parsed before external work starts.
- The resolved configuration is the only source for architecture, target,
  feature, and virtio transport decisions.
- Child process failures stop the pipeline and their numeric exit status is
  preserved when available.
- Bundle validity is published by manifest-last promotion.
- No host package installation, network download, or privilege escalation is
  performed implicitly.
- Arguments after `--` intentionally give advanced users direct QEMU control;
  they can alter the virtual machine's security boundary.

## Deferred Work

The following are deliberately outside the current implementation:

- richer unit-test result aggregation beyond the kernel-emitted status and
  generated LLVM coverage reports;
- QEMU acceleration and non-user networking policies;
- a declarative `XKMake.toml` runtime scheme.

`XKMake.toml` should be considered only after the typed command model has
stabilized. It may describe runtime defaults and boot policy, but it must not
duplicate Kconfig product selections.
