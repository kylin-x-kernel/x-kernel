# xkmake — Design Document

## Positioning

`xkmake` is the build and run orchestrator inside the X-Kernel repository. The root `Makefile` only provides
a stable entry point; `xkmake` is responsible for connecting `xconfig`, Cargo, the image processing tools, and QEMU.

## Scope

```text
src/
├── main.rs       command dispatch
├── cli.rs        strongly typed command-line arguments
├── context.rs    final build context
├── build.rs      Cargo, images, and bundle
├── linker.rs     kernel linker script rendering and stable writing
├── coverage.rs   unittest coverage extraction and report generation
├── doc.rs        workspace rustdoc orchestration
├── hygiene.rs    repository hygiene tool orchestration
├── dylint.rs     Dylint lint library build and kernel scan
├── facts.rs      packing of the code facts exported by the Dylint scan
├── image_metadata.rs  ELF finalize for embedded build info and Build ID
├── qemu.rs       platform boot policy
├── x86.rs        LinuxBoot/UEFI boot media
├── process.rs    external process boundary
├── dwarf_embed.rs in-process DWARF section embedding (enabled with KFEAT_DWARF)
├── symtab.rs     compact kernel symbol table generation (KFEAT_SYMTAB)
├── symbolize.rs  host-side symbolization of log addresses
└── error.rs      tool-layer error model
```

## Architecture

```text
Makefile
   |
   v
xkmake ------> xconfig ------> .config / config.rs / Cargo config
   |
   +---------> Cargo --------> kernel ELF
   |                            |
   |                            +--> kernel.debug.elf (full symbols, DWARF and coverage maps)
   |
   +-- embed DWARF on demand (KFEAT_DWARF) / bootstrap symbol table (KFEAT_SYMTAB)
   |      + rust-objcopy (external)
   |                         |
   |                         v
   +--------------------> versioned bundle
                               |  \
                               |   +--> workspace-root xkernel_* copies
                               |
                               +--> make symbolize (host-side recovery of function names and line numbers)
                               |
                               v
                              QEMU
```

The conversion from LCOV to Cobertura XML is an internal stage of the unittest coverage flow; it does not expose
a separate command or a second argument model.

`xconfig::build_config::ResolvedKernelConfig` is the stable semantic
boundary between the configurator and the builder. `xkmake` does not read the `.config` text, nor does it derive
the architecture, platform, or features back from the generated TOML.

## Core Flows

### Build

1. Verify that the current directory is the X-Kernel workspace.
2. Evaluate `ConfigEngine` once to produce the complete `ResolvedKernelConfig` snapshot.
3. From that snapshot, generate `.config`, `auto.conf`, `autoconf.h`, `config.rs`,
   the rust-analyzer settings and the Cargo config, and render the linker script of the current target from the repository-root
   `linker.lds.S`; the generation phase does not parse
   Kconfig again. The linker script is rewritten only
   when its content changes, to avoid pointless mtime updates that make Cargo recompile.
4. Construct a one-shot `BuildContext`, after which the environment and configuration files are no longer read.
5. Invoke Cargo to produce the target ELF.
6. x86_64 additionally builds the boot stub and the EFI loader incrementally.
7. If the existing bundle's stable build source, config hash, input times, artifact names, sizes and
   mtime order still match, it is reused directly as a trusted local cache, keeping the original build time;
   an ordinary build does not rescan the complete ELF.
8. Otherwise, save the complete Cargo ELF at a temporary path for host analysis; embed DWARF into the boot ELF on demand
   (KFEAT_DWARF), then have `rust-objcopy --strip-all` remove the non-loaded debug data and
   coverage maps. On x86, keep the `rust_entry` and `boot_stack_top` that the bootloader looks up.
   Finally, fill in place the `.note.xkernel.build-info` and `.note.gnu.build-id` reserved by the linker; the automatic
   build time takes the current UTC time at this point and does not take part in the earlier cache check.
9. The Build ID is the SHA-256 over all `PT_LOAD` content after build-info has been written and the
   Build ID descriptor zeroed.
10. Generate the raw BIN from the finalized ELF.
11. x86_64 generates the LinuxBoot header/image in-process and creates the UEFI FAT image.
    LinuxBoot's i386 setup code is assembled and linked with GNU tools: x86_64 hosts use the native
    `gcc`/`ld`/`objcopy`, and other hosts use `x86_64-linux-gnu-*`.
    The FAT image size is computed from the actual sizes of the bootloader, kernel ELF and config file, reserving two FATs,
    directory and allocation alignment space, MiB-aligned and at least 256 MiB, to support larger UT/coverage artifacts.
12. Remove the old manifest, promote all artifacts, and finally promote the new manifest as the validity marker.
13. Refresh the compatibility artifacts `xkernel_<platform>.*` in the repository root from the valid bundle, for
    Jenkins artifact handoff and tools that still use the old names. The copy is first written to a nonexistent
    temporary path in the same directory, so that APFS/Linux CoW filesystems can take the clone/reflink fast path, and
    then renamed over the final file; when the filesystem does not support it, the standard library falls back to a plain copy.

### Artifacts

Each bundle contains two kinds of ELF:

- `kernel.elf`: the slimmed boot ELF, keeping the loadable segments and the two boot symbols required by x86.
  The loadable debug sections embedded with KFEAT_DWARF, the runtime symbol table of KFEAT_SYMTAB, and the
  runtime profile data and counters of unittest are all retained;
- `kernel.debug.elf`: **always** the unprocessed Cargo artifact (non-alloc `.debug_*`
  and `__llvm_covfun` / `__llvm_covmap` fully retained), used for host symbolization, GDB,
  disassembly and coverage reports. `config bundle-debug-elf` provides the path of this artifact.

Both LinuxBoot and UEFI wrap `kernel.elf`; the host debug data in the file no longer enters
the source-image retention area at boot. Bundle format v8 forces a rebuild of old-version artifacts, to avoid reusing
a boot ELF that has not yet been slimmed.

### Symbol Table Bootstrapping (KFEAT_SYMTAB)

The compact symbol table (kallsyms style) comes from the kernel's own symbols, so there is a circularity: "the table needs the link output,
and the output needs the table". The build converges in two rounds:

1. The first round links with an empty table; `create_bundle` extracts function symbols from the Cargo ELF and writes them to
   `$TARGET_DIR/kbuild/ksymtab.bin`;
2. When the content has changed, Cargo is invoked again to relink (`util/backtrace` copies
   that blob through build.rs and embeds it with `include_bytes!`), and then the bundle is rebuilt.

Convergence is guaranteed by the layout: the blob lives in `.rodata` (an ordinary static), the symbol table only records functions in
the `.text` family of sections, and `.text` comes before `.rodata`, so a change in the blob size
does not change the addresses recorded in the table. The blob in the second round is identical to that of the first round, and the build stops.

With KFEAT_SYMTAB=n, an empty 16-byte blob is likewise written to disk. The build.rs of `util/backtrace`
tracks `$TARGET_DIR/kbuild/
ksymtab.bin` with `cargo:rerun-if-changed`, while Cargo treats any nonexistent `rerun-if-changed` input as
dirty, which would cause backtrace and all of its transitive dependencies to be fully recompiled on every build.
Therefore xkmake prepares this input before the first Cargo invocation: when KFEAT_SYMTAB is off it writes
a stable empty blob; when it is on it keeps the existing symbol table and writes an empty blob only if the file is missing.
In this way, after a clean build finishes, the input going from missing to present does not trigger an extra next build,
and subsequent builds can fully hit the Cargo incremental cache.

### Symbolize

`make symbolize LOG=<log>` extracts the raw addresses of the `Backtrace:` block from the log (compatible with the
`func+0xoff/0xsize` annotations), and calls `llvm-addr2line`/`addr2line` against
`kernel.debug.elf` to output function names and file/line numbers. `--offset` supports a kernel load address
offset (such as KASLR).

`make run`/`make justrun` symbolize automatically: QEMU output is tee'd to the terminal in real time and mirrored to
`bundle/qemu.log`; after the run ends, the backtrace frames are scanned, and if any are found they are automatically resolved against
`kernel.debug.elf` and printed (`--no-symbolize` turns this off). The tee is implemented by
`Process::run_tee`: stdout goes through a pipe and is forwarded by a reader thread to the terminal and the log file,
while stdin/stderr remain inherited, so interactive sessions are unaffected.

### Run

1. Execute the Build flow and obtain the bundle.
2. Select a closed QEMU boot policy according to the resolved platform.
3. Add devices according to the drivers compiled into the kernel and the CLI options.
4. Launch QEMU with structured arguments and propagate the exit status.
5. After the unittest QEMU exits successfully, verify the `ALL_TESTS_PASSED` and
   `COVERAGE_PERSISTED` markers in this run's `qemu.log`. The latter is published only after the guest confirms that the coverage has been fully written, the file fsynced, and the
   filesystem synced. When a marker is missing or `TESTS_FAILED` is present, the profile from an old image is rejected.
6. After verification passes, extract `default.profraw` from the disk image, and then generate in order
   `default.profdata`, `coverage.txt`, `coverage.info`, and `coverage.xml`.

Coverage artifacts are located in `target/<rust-target>/<profile>/`. Old
artifacts are deleted before generation starts, and the failure of any step makes this run fail, so that CI does not consume stale reports.

### Doc

1. As in the build flow, obtain the configuration snapshot through a single `ConfigEngine` evaluation.
2. Generate the build configuration and the linker script from the snapshot, and obtain the
   Cargo feature set directly.
3. Invoke `cargo doc --workspace --no-deps` with the resolved kernel Rust target,
   without interpreting the kernel ABI on the host target and without parsing the generated Cargo TOML.
4. `--check-missing` rejects missing rustdoc, on top of retaining the broken-link/cfg checks.

### Hygiene Deps

1. Verify that the `cargo-shear` version in PATH matches the version pinned by XKMake;
   when the tool is missing or the version does not match, direct the user to run `make install-tools`.
2. Run the check in turn on the root workspace, `xtask`, `tee_apps`, and `uapps/hello`.
3. Pass `--locked` only to workspaces whose `Cargo.lock` is tracked by Git (the kernel root workspace),
   to prevent the hygiene check from implicitly rewriting the committed lockfile; the lockfiles of `xtask`,
   `tee_apps`, and `uapps/hello` are gitignored and have no committed lock,
   so `--locked` is not passed and cargo generates them as needed.
4. `--fix` only passes through the dependency-removal capability of cargo-shear; warnings keep their hint semantics, while
   an error or the failure of any workspace check terminates the command.

### Hygiene Header

1. Verify that the `licensure` version in PATH matches the version pinned by XKMake; when the tool is missing or
   the version does not match, direct the user to run `make install-tools`.
2. Obtain the tracked and the untracked-but-not-ignored Rust source files through Git, and filter out the paths that have been
   deleted in the working tree; non-Rust files do not enter the header check.
3. Pass the files to licensure in batches, to avoid hitting the process argument length limit as the repository grows.
4. In normal mode, run `--check`; `--fix` first runs `--in-place`, and then rechecks the final state with `--check`.
   The header template and comment format are defined by the repository-root `.licensure.yml`.

### Hygiene Lints

1. Run `cargo metadata --no-deps` in the root workspace and use the
   `workspace_members` resolved by Cargo as the source of truth; path dependencies inside the workspace directory are implicit members,
   so the `members` glob alone is not sufficient.
2. Parse each member manifest and require `lints.workspace = true`; members where it is missing or
   `false` are listed all at once and the check fails. There is no registered exemption list.
3. `make clippy`, `make dylint` and `make dylint_test` run
   this check before the Cargo checks, so the CI clippy stage rejects any crate that does not inherit `[workspace.lints]`.

The x86_64 bundle carries both `kernel.bzimg` and `kernel.uefi.img`. LinuxBoot is used by default,
and `--boot uefi` uses OVMF. The OVMF VARS template is copied to
`target/xkmake/runtime/<platform>/` on every run, so neither the firmware template nor the bundle is modified.

vsock uses capability probing: when the kernel enables virtio-socket and the user has not passed `--no-vsock`,
XKMake queries the device list of the current QEMU.
The device is added only if the corresponding `vhost-vsock-*` model
exists; if it is not supported, a warning is printed and startup continues.

The network forwarding port and the vsock CID are probed sequentially. `--hostfwd-port` (default 61005)
is the preferred host port forwarded to guest TCP/UDP 5555. If it is in use, XKMake searches `61005..=62005` in steps of 10
(61005, 61015, ..., 62005) for the first port that is free for both TCP and UDP;
when the probe range is exhausted, it exits with an error. Port probing treats only `AddrInUse` as
in use; any other bind error (such as a non-root user binding a port below 1024) is reported as an error directly, and the port is not silently
changed. `--vsock-cid` (default 103) is the preferred guest CID; if it is in use, XKMake searches `103..=203`
one by one in order for a free CID. CID probing is done by temporarily opening
`/dev/vhost-vsock` and performing `VHOST_VSOCK_SET_GUEST_CID`; the kernel uses
`EADDRINUSE` to indicate that the CID is already in use, and the probe fd is then closed to release the CID. If the device is missing or
the user lacks permission to probe, XKMake falls back to the preferred value so that a probe failure does not prevent the virtual machine from starting, while the kernel's `EINVAL`
(reserved CID) is reported as an error directly; `--vsock-cid` is validated during argument parsing to be at least 3 (the kernel
reserves 0/1/2). `--dry-run` performs no probing and builds
the command line directly from the preferred port and CID.

`--no-build` accepts only an existing bundle whose manifest, build information, embedded Build ID, artifacts and Cargo ELF
time all match. The reuse check recomputes the ELF loadable-image hash
to avoid consuming a modified or incomplete bundle. The `config` subcommand provides the Makefile tool targets with read-only queries for the architecture,
target, platform and bundle path, so the Makefile does not need to re-parse `.config`.

### Dylint

1. `make dylint` builds the lint library (`tools/lints`) with its pinned
   toolchain into `<target>/tools/lints`, then runs `cargo dylint
   --lib-path` on the kernel with the configuration's features, target,
   rustflags and environment; Dylint loads the prebuilt library and does
   not build it. `make dylint_test` runs the library's tests in the same
   target directory.
2. The library build runs with an empty `CARGO_ENCODED_RUSTFLAGS` and
   `CARGO_TARGET_DIR` set to that directory. Cargo in `tools/lints` still
   reads the repository `.cargo/config.toml` and the generated
   `.xconfig.toml` it includes, whose `[build] rustflags` change with the
   configuration (`--cfg k_plat_name=...`, `--cfg unittest`). The library
   uses none of them, but Dylint records it as a file dependency of every
   crate it checks, so relinking it for another configuration would
   re-check every crate. With the rustflags pinned, the library is rebuilt
   only when its sources, `Cargo.lock` or the toolchain change.
   `CARGO_TARGET_DIR` also reaches the library build that `dylint_testing`
   runs from the tests, which otherwise lands in `tools/lints/target`.
3. The cfgs the lints see (`k_plat_name`, `unittest`) belong to the
   kernel crates and still come from the configuration. Scans of different
   configurations share `<target>/dylint`; Cargo keeps separate artifacts
   for different rustflags. A crate is still re-checked when an input it
   shares between configurations changes, such as the generated
   `<target>/kbuild/<platform>/config.rs` that `kbuild_config` includes
   (it differs when, for example, lock statistics are on).

### Dylint Facts

`make dylint FACTS_OUT=<dir>` (`xkmake dylint --facts-out <dir>`) exports
code facts during the normal Dylint scan and packs them. The format and the
records are described in `tools/lints/README.md`.

1. Before anything else the run removes the previous artifact
   (`<dir>/status.json`, `manifest.json` and `crates/`), so that a run that
   fails while reading the kernel configuration leaves no status rather
   than a stale one. Once the configuration is read, it writes
   `status.json` (`running`); errors before the scan starts, such as a tool
   probe or the lint library build, are written as `failed`/`build`.
2. The scan process gets `XKERNEL_FACTS=1`, and Cargo gets
   `--message-format=json-render-diagnostics`: diagnostics are still
   rendered to stderr, and the JSON messages go to
   `<target>/dylint/facts-messages.jsonl`. The file is kept when the scan
   fails, so that the crates checked before the failure are packed.
3. With `workspace_members` from `cargo metadata --no-deps` as the member
   set, the metadata path of each unit is taken from the
   `compiler-artifact` messages (including `fresh` units), which gives the
   `<stem>.facts.jsonl` the lint writes next to it; build scripts are
   skipped.
4. Each file's header (schema, stem) and final `counts` line are
   validated; the file is zstd-compressed into `crates/` with its sha256
   and record count. Missing or invalid files are listed in `missing`.
5. `manifest.json` is written (commit, merge base, target, platform,
   profile, sha256 of `.config`, features, scan rustflags, toolchain, rustc
   version, exporter source digest and Dylint version, crate list and
   totals), and `status.json` last: `ready`, `partial` or `failed`, with
   reason `build` or `exporter`.

During an export the scan process also gets
`XKERNEL_FACTS_ROOT=<repository root>`: Cargo compiles crates.io and git
dependencies in their own package directories and passes relative paths, so
the exporter uses this root to write paths as repository-relative or as
`crates/<name>-<version>/…`. The variable is not registered as an
environment dependency and does not affect the cache.

Without `--facts-out` the run leaves `XKERNEL_FACTS` unchanged: a caller
that pins it to `1` (the whole CI dylint stage) keeps Cargo's cache in later
scans and only writes extra facts files.

`--facts-deps` (or `XKERNEL_FACTS_DEPS=1`) also exports dependency crates:
the scan sets XKMake itself as Cargo's `RUSTC_WRAPPER`
(`XKMAKE_FACTS_WRAPPER_TARGET` gives the kernel target). `main` recognizes
this environment variable before parsing the command line and, when the
first argument is a compiler (`rustc`, Cargo's `RUSTC` or Dylint's
driver), runs as the wrapper: workspace members (whose first argument is Dylint's
`RUSTC_WORKSPACE_WRAPPER`) and host crates are `exec`ed unchanged; a
dependency compiled for the kernel target is compiled by the Dylint driver
instead, with `--force-warn facts_export` appended and
`XKERNEL_FACTS_KIND=dep` set. The `--cap-lints allow` that Cargo passes for
dependencies still silences the other lints; only the export lint is forced
on. When packing, non-member units whose paths lie under the kernel target
directory are taken in as `dep` crates too; the manifest records
`deps: true`, and their crate entries carry the dependency's features.

## Design Decisions

- Cargo handles incremental source compilation; the bundle caches only the image-processing results.
- Reuse in an ordinary build belongs to the trusted local cache boundary, judged quickly from the invariant that the manifest is committed last,
  and the artifact sizes and mtime; `--no-build` belongs to the existing-artifact consumption boundary and additionally performs
  a full loadable-image SHA-256 check.
- Every build still invokes Cargo to check source freshness; when sources and config are unchanged, the Cargo
  incremental flow does not recompile, and XKMake also does not regenerate ELF metadata, BIN or boot images.
  The KFEAT_SYMTAB symbol-table bootstrap does not trigger a second link when the blob content is unchanged.
- Build-source metadata does not enter the Cargo compilation environment; the linker reserves a fixed note, and XKMake writes it in place at the
  bundle stage, so changes in machine name and time do not recompile the kernel crate.
- The automatic build time is the UTC time at which the bundle was actually generated this time, not a cache-check input;
  `KBUILD_BUILD_TIME` or `SOURCE_DATE_EPOCH` is an explicit override, and when its value changes it
  invalidates the existing bundle.
- The image-metadata ABI is owned by `boot/kernel-image-metadata`; XKMake is responsible for collection,
  and entry is only responsible for displaying the validated result.
- The linker script is a config-derived host orchestration artifact, generated by XKMake before invoking
  Cargo; `kernel-boot` no longer generates it with `build.rs`.
- `.config` remains the sole source of truth for the kernel product configuration.
- QEMU arguments are passed item by item with `Vec<OsString>` semantics, without shell evaluation.
- `xkmake` depends only on the `xconfig::build_config` facade, not on the CLI implementation modules.
- Without a `.config`, no default platform is guessed.
- The bundle is the canonical artifact location; the `xkernel_<platform>.*` files in the repository root are compatibility copies refreshed from the valid
  bundle and do not take part in bundle validity checks.
- The LinuxBoot image layout is generated by XKMake's Rust code and does not depend on Python scripts.
- The rustdoc features and the kernel build use the same resolved snapshot; no second config reader is maintained.
- Dependency analysis uses the external cargo-shear at a pinned version; it does not duplicate Cargo/Rust
  parsing logic inside XKMake nor link its large dependency tree into XKMake.
- Rust source file headers are checked and repaired by the external licensure at a pinned version according to the repository config;
  XKMake is only responsible for determining the file set, invoking in batches, and propagating failure status.
- Code facts are exported by the lint library inside rustc; XKMake only
  selects the units of this build, validates and compresses their files and
  writes the manifest. Units are selected from Cargo's JSON messages rather
  than by scanning directories, so facts files left by other configurations
  in the same target directory are never mixed in. The export switch is an
  environment variable with the fixed value `1`, so that a changing value
  does not make Cargo re-check every crate.
- The guest IP and gateway are currently build inputs, because the existing network stack compiles them in via
  `option_env!`; they should later be migrated to kernel command-line parameters.

## Current Limitations

- AArch64, RISC-V 64 and LoongArch64 QEMU virt boot directly from the raw BIN.
- x86_64 LinuxBoot and UEFI are supported; other x86 platforms still need a separate boot strategy.
- unittest already generates an LLVM coverage report; richer aggregation of test results has not yet been migrated.
