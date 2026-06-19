# uapps

`uapps/` describes host-built user-space applications that are injected into an
existing X-Kernel root filesystem image.

Each child directory is one uapp and must contain a `uapp.toml` manifest. The
uapp installer runs on the host. It reads manifests, runs host-side prepare
commands, copies declared files into `disk.img` with `debugfs`, and generates a
guest-side `/etc/profile.d/99-autostart.sh` script.

The first implementation is intentionally image-in-place:

- `make rootfs` prepares or refreshes the base `disk.img`.
- `make uapps` modifies the existing `disk.img` with `debugfs -w`.
- `make uapps` must not depend on `make rootfs`, because refreshing the rootfs
  would overwrite previously injected files.

## Layout

```text
uapps/
  README.md
  hello/
    Cargo.toml
    src/
      main.rs
    uapp.toml
```

## Workflow

Prepare a base rootfs image:

```bash
make rootfs
```

Inject enabled uapps into the existing image:

```bash
make uapps
```

Run X-Kernel with the modified image:

```bash
make run
```

Install only selected uapps:

```bash
make uapps UAPPS=hello
make uapps UAPPS=uapps/hello
```

## Manifest

Example:

```toml
[package]
name = "hello"
description = "Minimal Rust uapp example"
enabled = true
order = 10

[prepare]
env = [
  "RUSTFLAGS=",
  "CC=aarch64-linux-musl-gcc",
  "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-musl-gcc",
]
commands = [
  "CARGO_TARGET_DIR=\"${UAPP_BUILD_DIR}/cargo\" cargo build --release --target aarch64-unknown-linux-musl --manifest-path Cargo.toml",
  "mkdir -p out",
  "cp \"${UAPP_BUILD_DIR}/cargo/aarch64-unknown-linux-musl/release/hello-uapp\" out/hello-uapp",
]

[[install]]
src = "out/hello-uapp"
dest = "/usr/local/bin/hello-uapp"
mode = "0755"

[[autostart]]
name = "hello"
command = "/usr/local/bin/hello-uapp"
background = false
```

### `[package]`

```toml
[package]
name = "hello"
description = "Minimal Rust uapp example"
enabled = true
order = 10
```

- `name`: Required uapp name. It should match the directory name.
- `description`: Optional human-readable description.
- `enabled`: Optional. Defaults to `true`.
- `order`: Optional. Defaults to `100`. Lower values run first.

### `[prepare]`

```toml
[prepare]
env = [
  "KEY=value",
]
commands = [
  "sh scripts/build.sh",
]
```

Prepare commands run on the host before files are copied into the image.

- `env` entries use `KEY=value` syntax and apply to every command in this
  uapp's prepare phase.
- Commands run with the uapp directory as the working directory.
- Commands are shell commands and must return `0`.
- A failed command fails the whole uapp installation.
- Build outputs should be written under the uapp directory or a future
  tool-managed output directory.

The installer should provide these environment variables to prepare commands:

```text
REPO_ROOT
UAPP_NAME
UAPP_DIR
UAPP_BUILD_DIR
UAPP_OUT_DIR
DISK_IMG
K_ARCH
K_TARGET
K_PLAT_NAME
CROSS_COMPILE
```

### `[[install]]`

```toml
[[install]]
src = "out/hello-uapp"
dest = "/usr/local/bin/hello-uapp"
mode = "0755"
```

Install entries describe files or directories to copy into the guest rootfs.

- `src`: Required host path. Relative paths are resolved from the uapp
  directory.
- `dest`: Required guest path. It must be absolute.
- `mode`: Optional octal mode string such as `0755` or `0644`.

If `src` is a file, it is written to `dest`. If `src` is a directory, the
directory tree is recursively copied under `dest`.

The installer should create guest parent directories automatically. Re-running
`make uapps` should overwrite files declared by current manifests.

### `[[autostart]]`

```toml
[[autostart]]
name = "hello"
workdir = "/tmp"
command = "/usr/local/bin/hello-uapp"
background = false
exit = true
check_alive = false
```

Autostart entries are rendered into `/etc/profile.d/99-autostart.sh`.

- `name`: Required log name.
- `command`: Required guest-side shell command.
- `workdir`: Optional guest working directory.
- `background`: Optional. Defaults to `false`.
- `exit`: Optional. Defaults to `false`. When `true`, append `exit 0` after
  this entry finishes successfully.
- `check_alive`: Optional. Defaults to the value of `background`.

The generated script must be POSIX `sh` compatible because minimal rootfs images
may not provide Bash.

## Installer

`make uapps` is backed by the host-side Rust tool in `xtask/uapp`. The tool
performs typed TOML parsing, manifest validation, prepare command execution,
autostart generation, and `debugfs` command generation.

Make variables:

```make
UAPPS ?= all
UAPP_DIR ?= $(PWD)/uapps
UAPP_AUTOSTART_TARGET ?= /etc/profile.d/99-autostart.sh
```

Target shape:

```make
uapps:
	@if [ ! -f "$(DISK_IMG)" ]; then \
		echo "disk image not found: $(DISK_IMG)"; \
		echo "Please run 'make rootfs' first."; \
		exit 1; \
	fi
	cargo run --target-dir $(TARGET_DIR)/tools/uapp \
	  --manifest-path xtask/uapp/Cargo.toml -- \
	  install \
	  --uapps-dir $(UAPP_DIR) \
	  --disk-img $(DISK_IMG) \
	  --select $(UAPPS) \
	  --autostart-target $(UAPP_AUTOSTART_TARGET)
```

`make uapps` should not depend on `make rootfs`. If a combined reset-and-inject
workflow is useful, add a separate target:

```make
rootfs-uapps: rootfs uapps
```

The tool can also be invoked directly:

```bash
cargo uapp list --uapps-dir uapps
cargo uapp prepare --uapps-dir uapps --select hello
cargo uapp install --uapps-dir uapps --disk-img disk.img --select uapps/hello
cargo uapp install --uapps-dir uapps --disk-img disk.img --select uapps/hello --dry-run
```

## Installation Order

The installer should:

1. Scan `uapps/*/uapp.toml`.
2. Parse and validate manifests.
3. Filter disabled uapps and apply `UAPPS`.
4. Sort by `package.order`, then `package.name`.
5. Run `prepare.commands`.
6. Validate every `install.src`.
7. Generate a temporary `99-autostart.sh`.
8. Expand file and directory install entries.
9. Generate one temporary `debugfs` command file.
10. Run `debugfs -w -f <commands> <disk.img>`.
11. Verify the installed guest paths with `debugfs stat`.
12. Print an installation summary.

## debugfs Strategy

The installer should invoke `debugfs` once per installation:

```bash
debugfs -w -f /tmp/xkernel-uapp-debugfs.commands disk.img
```

The generated command file should:

- Create guest directories one path component at a time.
- Remove an existing target file before writing a replacement.
- `cd` into the target directory before `write`.
- Set file modes after writing.
- Quote host and guest paths.
- Require the autostart target parent directory to already exist. The installer
  must fail if `/etc/profile.d` is absent, because the startup mechanism depends
  on that profile directory.

Example command sequence:

```text
mkdir "/usr"
mkdir "/usr/local"
mkdir "/usr/local/bin"
cd "/usr/local/bin"
rm "hello-uapp"
write "/home/user/x-kernel/uapps/hello/out/hello-uapp" "hello-uapp"
set_inode_field "/usr/local/bin/hello-uapp" mode 0100755
cd "/etc/profile.d"
rm "99-autostart.sh"
write "/tmp/xkernel-uapp-99-autostart.sh" "99-autostart.sh"
set_inode_field "/etc/profile.d/99-autostart.sh" mode 0100755
```

`debugfs mkdir` may fail when a directory already exists. The installer should
treat that as acceptable when the existing path is the requested directory, or
generate commands in a way that repeated installs remain practical.

## Autostart Script

The installer owns `/etc/profile.d/99-autostart.sh`. Uapps must not install that
file directly.

The generated script should:

- Use `#!/bin/sh`.
- Guard against repeated profile loading with `XKERNEL_AUTOSTART_DONE`.
- Log each entry before starting it.
- Use `return`, not `exit`, because profile scripts are usually sourced.
- Support foreground and background commands.
- Optionally check that background commands are still alive after startup.

Minimal generated shape:

```sh
#!/bin/sh

[ -n "${XKERNEL_AUTOSTART_DONE:-}" ] && return 0
export XKERNEL_AUTOSTART_DONE=1

echo "[autostart] x-kernel uapp startup hook triggered"
date

start_background() {
    name="$1"
    shift

    echo "[autostart] starting ${name}"
    "$@" &
    pid="$!"

    sleep 1
    if ! kill -0 "${pid}" 2>/dev/null; then
        echo "[autostart] ${name} failed to start"
        return 1
    fi
}

start_foreground() {
    name="$1"
    shift

    echo "[autostart] starting ${name}"
    "$@"
}

start_foreground "hello" /usr/local/bin/hello-uapp || return 1
```

## Safety Rules

- `install.dest` must be an absolute guest path.
- The installer must reject attempts to install
  `/etc/profile.d/99-autostart.sh`; that file is generated centrally.
- The installer should reject empty paths and paths containing NUL bytes.
- The installer should avoid following host symlinks unless that behavior is
  explicitly documented.
- The install summary should include the image path, selected uapps, installed
  host-to-guest paths, file count, and autostart entry count.
