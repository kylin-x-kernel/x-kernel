# SPDX-License-Identifier: Apache-2.0
#
# This Makefile is a thin wrapper around `xkmake` (see xtask/xkmake). Every
# target below delegates to a single Rust binary that reads `.config` (via
# xconfig) as the source of truth for platform, arch, cargo features, etc.
#
# Available make variables (all optional):
#
# * General:
#     - TARGET_DIR          Artifact / cargo output directory
#     - UNITTEST            Build the unittest entry and run it under QEMU (y/n)
#     - UNITTEST_CRATE      Limit unittest to crate(s), comma-separated
#
# * QEMU / runtime — `make run`, `make justrun`, `make debug`:
#     - DISK_IMG            virtio-blk backing disk image (see RAMDISK_IMG below)
#     - NET / BLK / VSOCK   Enable virtio-net / virtio-blk / vhost-vsock (y/n)
#     - ACCEL               Enable KVM (linux) / HVF (macOS) acceleration (y/n)
#     - GRAPHIC             Enable graphical output + virtio-gpu (y/n)
#     - UEFI                Boot x86_64 via OVMF `.uefi.img` instead of LinuxBoot (y/n)
#     - MEM                 Guest memory size (default 1g)
#     - SMP                 Number of vCPUs (default: configured NR_CPUS)
#     - HOSTFWD_PORT        Host port forwarded to guest :5555 (default 5555)
#     - VSOCK_CID           Guest CID for the vhost-vsock device (default 103)
#     - QEMU_ARGS           Extra QEMU args, appended verbatim after `--`
#     - XKMAKE_ARGS         Extra flags forwarded to every xkmake invocation
#
# * Test harness — `make ci-test`:
#     - CI_TEST_DIR         Where to clone the Starry test harness
#     - CI_TEST_REPO        Harness Git URL
#     - CI_TEST_BRANCH      Harness branch
#     - CI_TEST_ARCH        Limit to one arch (e.g. aarch64)
#     - CI_TEST_CASES       Limit to specific test cases
#     - CI_TEST_JOBS        Parallel harness jobs
#
# * RAM disk (embedded into the kernel when KFEAT_DRIVER_RAMDISK_STATIC=y;
#   distinct from DISK_IMG, the virtio-blk backing file):
#     - RAMDISK_IMG         Path to the embedded ramdisk image
#     - RAMDISK_IMG_SIZE    Ramdisk size in MiB
#     - RAMDISK_IMG_FS      Ramdisk filesystem: ext4 | fat
#     - RAMDISK_ROOTFS      Source image to inject a minimal /bin/sh + musl
#                           runtime from (default DISK_IMG; empty = empty ramdisk)
#
# * Root filesystem download (`make rootfs`):
#     - ROOTFS_URL          Base URL for prebuilt rootfs images
#     - ROOTFS_VARIANT      Rootfs variant (default alpine-busybox)

.DEFAULT_GOAL := build

HOST_TARGET := $(shell rustc -vV | sed -n 's/^host: //p')
TARGET_DIR ?= $(CURDIR)/target
XKMAKE_TARGET_DIR ?= $(TARGET_DIR)/tools/xkmake
XCONFIG_TARGET_DIR ?= $(TARGET_DIR)/tools/xconfig
UAPP_TARGET_DIR ?= $(TARGET_DIR)/tools/uapp
XTASK_TARGET_DIR ?= $(CURDIR)/xtask/target

XKMAKE := env \
	CARGO_BUILD_TARGET=$(HOST_TARGET) \
	RUSTFLAGS= \
	CARGO_ENCODED_RUSTFLAGS= \
	cargo run --quiet \
		--target-dir $(XKMAKE_TARGET_DIR) \
		--manifest-path xtask/Cargo.toml \
		-p xkmake --
XCONFIG := env \
	CARGO_BUILD_TARGET=$(HOST_TARGET) \
	RUSTFLAGS= \
	CARGO_ENCODED_RUSTFLAGS= \
	cargo run --quiet \
		--target-dir $(XCONFIG_TARGET_DIR) \
		--manifest-path xtask/Cargo.toml \
		-p xconfig --bin xconf --
UAPP_TOOL := env \
	CARGO_BUILD_TARGET=$(HOST_TARGET) \
	RUSTFLAGS= \
	CARGO_ENCODED_RUSTFLAGS= \
	cargo run --quiet \
		--target-dir $(UAPP_TARGET_DIR) \
		--manifest-path xtask/Cargo.toml \
		-p uapp --

# QEMU virtio-blk backing file. Distinct from RAMDISK_IMG (embedded into the
# kernel at build time when KFEAT_DRIVER_RAMDISK_STATIC=y).
DISK_IMG ?= $(CURDIR)/disk.img
RAMDISK_IMG ?= $(CURDIR)/ramdisk.img
RAMDISK_IMG_SIZE ?= 8
RAMDISK_IMG_FS ?= ext4
RAMDISK_ROOTFS ?= $(DISK_IMG)
ROOTFS_URL ?= https://gitee.com/openkylin/x-kernel-image/releases/download/rootfs
ROOTFS_VARIANT ?= alpine-busybox

UNITTEST ?= n
UNITTEST_CRATE ?=
NET ?= y
BLK ?= y
VSOCK ?= y
ACCEL ?= y
UEFI ?= n
GRAPHIC ?= n
MEM ?=
SMP ?=
QEMU_ARGS ?=

UAPPS ?= all
UAPP_DIR ?= $(CURDIR)/uapps
UAPP_AUTOSTART_TARGET ?= /etc/profile.d/99-autostart.sh

OBJDUMP ?= rust-objdump -d --print-imm-hex --x86-asm-syntax=intel
GDB ?= gdb

# Starry test harness (make ci-test)
CI_TEST_DIR ?= $(CURDIR)/ci-test
CI_TEST_REPO ?= https://gitee.com/openkylin/starry-test-harness
CI_TEST_BRANCH ?= master
CI_TEST_ARCH ?=
CI_TEST_CASES ?=
CI_TEST_JOBS ?=

XKMAKE_BUILD_ARGS := \
	--target-dir "$(TARGET_DIR)" \
	$(if $(filter y,$(UNITTEST)),--unittest) \
	$(if $(and $(filter y,$(UNITTEST)),$(strip $(UNITTEST_CRATE))),--unittest-crate "$(UNITTEST_CRATE)")
XKMAKE_RUN_ARGS := \
	--disk-image "$(DISK_IMG)" \
	$(if $(strip $(HOSTFWD_PORT)),--hostfwd-port "$(HOSTFWD_PORT)") \
	$(if $(strip $(VSOCK_CID)),--vsock-cid "$(VSOCK_CID)") \
	$(if $(filter y,$(UEFI)),--boot uefi) \
	$(if $(strip $(OVMF_CODE)),--ovmf-code "$(OVMF_CODE)") \
	$(if $(strip $(OVMF_VARS_TEMPLATE)),--ovmf-vars-template "$(OVMF_VARS_TEMPLATE)") \
	$(if $(filter n,$(NET)),--no-net) \
	$(if $(filter n,$(BLK)),--no-block) \
	$(if $(filter n,$(VSOCK)),--no-vsock) \
	$(if $(filter n,$(ACCEL)),--no-accel) \
	$(if $(filter y,$(GRAPHIC)),--graphic) \
	$(if $(strip $(MEM)),--memory "$(MEM)") \
	$(if $(strip $(SMP)),--smp "$(SMP)")

_HOOKS_BOOTSTRAP := $(shell \
	top=$$(git rev-parse --show-toplevel 2>/dev/null); \
	if [ -n "$$top" ] && [ -x "$$top/.githooks/pre-commit" ]; then \
		cur=$$(git config --get core.hooksPath 2>/dev/null); \
		if [ -z "$$cur" ] || [ ! -x "$$cur/pre-commit" ]; then \
			git config core.hooksPath "$$top/.githooks"; \
		fi; \
	fi)

.PHONY: all build run justrun debug disasm clippy doc doc_check_missing
.PHONY: unittest unittest_no_fail_fast
.PHONY: defconfig oldconfig olddefconfig menuconfig saveconfig savedefconfig gen-const
.PHONY: rootfs uapps rootfs-uapps teefs disk_img ramdisk_img
.PHONY: install-tools check_deps deps check_header header fmt hooks clean distclean
.PHONY: ci-test

all: build

build:
	@echo "Building x-kernel..."
	@$(XKMAKE) build $(XKMAKE_BUILD_ARGS) $(XKMAKE_ARGS)

run:
	@echo "Running x-kernel..."
	@$(XKMAKE) run $(XKMAKE_BUILD_ARGS) $(XKMAKE_RUN_ARGS) $(XKMAKE_ARGS) $(if $(strip $(QEMU_ARGS)),-- $(QEMU_ARGS))

justrun:
	@$(XKMAKE) run --no-build $(XKMAKE_BUILD_ARGS) $(XKMAKE_RUN_ARGS) $(XKMAKE_ARGS) $(if $(strip $(QEMU_ARGS)),-- $(QEMU_ARGS))

debug: build
	@set -eu; \
	elf="$$($(XKMAKE) config bundle-elf --target-dir "$(TARGET_DIR)")"; \
	$(XKMAKE) run --no-build $(XKMAKE_BUILD_ARGS) $(XKMAKE_RUN_ARGS) $(XKMAKE_ARGS) -- $(QEMU_ARGS) -s -S & \
	qemu_pid=$$!; \
	trap 'kill $$qemu_pid 2>/dev/null || true' EXIT INT TERM; \
	$(GDB) "$$elf" \
		-ex 'target remote localhost:1234' \
		-ex 'b __kplat_main' \
		-ex 'continue' \
		-ex 'disp /16i $$pc'

disasm:
	@set -eu; \
	elf="$$($(XKMAKE) config bundle-elf --target-dir "$(TARGET_DIR)")"; \
	test -f "$$elf" || { echo "kernel bundle not found: $$elf; run 'make build' first"; exit 1; }; \
	$(OBJDUMP) "$$elf" | less

clippy:
	@$(MAKE) --no-print-directory check_deps check_header
	@$(XKMAKE) clippy $(XKMAKE_BUILD_ARGS) $(XKMAKE_ARGS)

doc:
	@$(XKMAKE) doc --target-dir "$(TARGET_DIR)" $(XKMAKE_ARGS)

doc_check_missing:
	@$(XKMAKE) doc --target-dir "$(TARGET_DIR)" --check-missing $(XKMAKE_ARGS)

# Kernel unit tests: build the unittest entry and run it under QEMU.
unittest:
	@$(MAKE) --no-print-directory run UNITTEST=y

unittest_no_fail_fast: unittest

defconfig:
	@test -f .config || { echo "error: copy a platform defconfig to .config first"; exit 1; }
	@$(XCONFIG) defconfig .config --kconfig Kconfig --srctree .

oldconfig:
	@test -f .config || { echo "error: .config not found; run 'make defconfig' first"; exit 1; }
	@$(XCONFIG) oldconfig --config .config --kconfig Kconfig --srctree .

olddefconfig:
	@test -f .config || { echo "error: .config not found; run 'make defconfig' first"; exit 1; }
	@$(XCONFIG) olddefconfig --config .config --kconfig Kconfig --srctree .

menuconfig:
	@$(XCONFIG) menuconfig --kconfig Kconfig --srctree .

saveconfig:
	@test -f .config || { echo "error: .config not found; run 'make defconfig' first"; exit 1; }
	@$(XCONFIG) saveconfig --output .config --kconfig Kconfig --srctree .

savedefconfig:
	@test -f .config || { echo "error: .config not found; run 'make defconfig' first"; exit 1; }
	@$(XCONFIG) savedefconfig --config .config --output defconfig --kconfig Kconfig --srctree .
	@echo "saved defconfig to ./defconfig"

gen-const:
	@platform="$$($(XKMAKE) config platform)"; \
	$(XCONFIG) gen-const --config .config \
		--output-dir "$(TARGET_DIR)/kbuild/$$platform" \
		--kconfig Kconfig --srctree .

rootfs:
	@set -eu; \
	arch="$$($(XKMAKE) config arch)"; \
	image="x-kernel-$(ROOTFS_VARIANT)-$$arch.img"; \
	if [ ! -f "$$image" ]; then \
		echo "Image not found, downloading $$image..."; \
		curl -f -L "$(ROOTFS_URL)/$$image.xz" -o "$$image.xz"; \
		xz -df "$$image.xz"; \
	fi; \
	cp "$$image" "$(DISK_IMG)"

uapps:
	@test -f "$(DISK_IMG)" || { echo "disk image not found: $(DISK_IMG); run 'make rootfs' first"; exit 1; }
	@set -eu; \
	arch="$$($(XKMAKE) config arch)"; \
	target="$$($(XKMAKE) config target)"; \
	platform="$$($(XKMAKE) config platform)"; \
	cross_compile="$$($(XKMAKE) config cross-compile)"; \
	$(UAPP_TOOL) install \
		$(UAPP_ARGS) \
		--uapps-dir "$(UAPP_DIR)" \
		--disk-img "$(DISK_IMG)" \
		--select "$(UAPPS)" \
		--autostart-target "$(UAPP_AUTOSTART_TARGET)" \
		--repo-root "$(CURDIR)" \
		--build-dir "$(TARGET_DIR)/uapps" \
		--arch "$$arch" \
		--target "$$target" \
		--plat-name "$$platform" \
		--cross-compile "$$cross_compile"

rootfs-uapps: rootfs uapps

teefs:
	@arch="$$($(XKMAKE) config arch)"; \
	$(MAKE) -C tee_apps ARCH="$$arch" DISK_IMG="$(DISK_IMG)" TARGET_DIR="$(TARGET_DIR)"

disk_img:
	@if [ -e "$(DISK_IMG)" ]; then \
		echo "warning: disk image already exists: $(DISK_IMG)"; \
	else \
		echo "Creating FAT32 disk image $(DISK_IMG)..."; \
		dd if=/dev/zero of="$(DISK_IMG)" bs=1M count=64; \
		mkfs.fat -F 32 "$(DISK_IMG)"; \
	fi

ramdisk_img:
	@set -eu; \
	if [ -e "$(RAMDISK_IMG)" ]; then \
		echo "warning: ramdisk image already exists: $(RAMDISK_IMG)"; \
		exit 0; \
	fi; \
	arch="$$($(XKMAKE) config arch)"; \
	staging=""; \
	if [ "$(RAMDISK_IMG_FS)" = ext4 ] && [ -f "$(RAMDISK_ROOTFS)" ]; then \
		staging="$(TARGET_DIR)/ramdisk-staging"; \
		rm -rf "$$staging"; \
		mkdir -p "$$staging/bin" "$$staging/lib" "$$staging/root" "$$staging/tmp" \
			"$$staging/proc" "$$staging/sys" "$$staging/dev" "$$staging/etc"; \
		loader="ld-musl-$$arch.so.1"; \
		debugfs -R "dump /bin/busybox $$staging/bin/busybox" "$(RAMDISK_ROOTFS)" >/dev/null 2>&1; \
		debugfs -R "dump /lib/$$loader $$staging/lib/$$loader" "$(RAMDISK_ROOTFS)" >/dev/null 2>&1; \
		chmod 0755 "$$staging/bin/busybox" "$$staging/lib/$$loader"; \
		ln -s busybox "$$staging/bin/sh"; \
		ln -s "$$loader" "$$staging/lib/libc.musl-$$arch.so.1"; \
		for app in env ls cat mount umount mkdir rmdir uname ps dmesg cp mv rm echo pwd; do \
			ln -s busybox "$$staging/bin/$$app"; \
		done; \
	fi; \
	dd if=/dev/zero of="$(RAMDISK_IMG)" bs=1M count="$(RAMDISK_IMG_SIZE)" >/dev/null 2>&1; \
	if [ "$(RAMDISK_IMG_FS)" = fat32 ]; then \
		mkfs.fat -F 32 "$(RAMDISK_IMG)" >/dev/null; \
	elif [ "$(RAMDISK_IMG_FS)" = ext4 ]; then \
		mkfs.ext4 -q -F -b 4096 -O ^metadata_csum,^64bit $${staging:+-d "$$staging"} "$(RAMDISK_IMG)"; \
	else \
		echo "unsupported RAMDISK_IMG_FS: $(RAMDISK_IMG_FS)"; \
		rm -f "$(RAMDISK_IMG)"; \
		exit 1; \
	fi; \
	rm -rf "$$staging"

install-tools:
	@$(XKMAKE) hygiene install-tools

check_deps:
	@$(XKMAKE) hygiene deps

deps:
	@$(XKMAKE) hygiene deps --fix

check_header:
	@$(XKMAKE) hygiene header

header:
	@$(XKMAKE) hygiene header --fix

fmt:
	@cargo +nightly-2026-03-08 fmt --all

hooks:
	@git config core.hooksPath "$$(git rev-parse --show-toplevel)/.githooks"
	@echo "x-kernel: git hooks enabled -> $$(git config --get core.hooksPath)"

ci-test: build
	@set -eu; \
	if [ ! -d "$(CI_TEST_DIR)/.git" ]; then \
		test ! -e "$(CI_TEST_DIR)" || { \
			echo "error: $(CI_TEST_DIR) exists but is not a Git repository" >&2; exit 1; \
		}; \
		echo "Cloning Starry test harness into $(CI_TEST_DIR)..."; \
		git clone --branch "$(CI_TEST_BRANCH)" -- "$(CI_TEST_REPO)" "$(CI_TEST_DIR)"; \
	else \
		git -C "$(CI_TEST_DIR)" diff --quiet || { \
			echo "error: $(CI_TEST_DIR) has uncommitted changes" >&2; exit 1; \
		}; \
		echo "Updating Starry test harness..."; \
		git -C "$(CI_TEST_DIR)" fetch --prune origin "$(CI_TEST_BRANCH)"; \
		git -C "$(CI_TEST_DIR)" checkout -q "$(CI_TEST_BRANCH)"; \
		git -C "$(CI_TEST_DIR)" pull --ff-only origin "$(CI_TEST_BRANCH)"; \
	fi; \
	$(MAKE) --no-print-directory -C "$(CI_TEST_DIR)" ci-test run \
		XKERNEL_REMOTE="$(CURDIR)" \
		$(if $(strip $(CI_TEST_ARCH)),ARCH="$(CI_TEST_ARCH)") \
		$(if $(strip $(CI_TEST_CASES)),CASES="$(CI_TEST_CASES)") \
		$(if $(strip $(CI_TEST_JOBS)),JOBS="$(CI_TEST_JOBS)")

clean:
	@rm -f "$(CURDIR)"/xkernel_*.bin "$(CURDIR)"/xkernel_*.elf
	@rm -f "$(CURDIR)"/xkernel_*.bzimg "$(CURDIR)"/xkernel_*.uefi.img
	@cargo clean --target-dir "$(TARGET_DIR)"
	@cargo clean --target-dir "$(XTASK_TARGET_DIR)"
	@rm -rf "$(TARGET_DIR)/kbuild"

distclean: clean
	@rm -f .config .config.old .config.prepared auto.conf autoconf.h
	@echo "Removed all generated configuration files"
