# Available arguments:
# * General options:
#     - `V`: Verbose level: (empty), 1, 2
#     - `TARGET_DIR`: Artifact output directory (cargo target directory)
#     - `EXTRA_CONFIG`: Extra config specification file
#     - `UIMAGE`: To generate U-Boot image
#     - `LD_SCRIPT`: Use a custom linker script file.
#     - `UNITTEST_CRATE`: Filter unittest by crate name (or comma-separated crate names)
# * App options:
#     - `A` or `APP`: Path to the application
#     - `FEATURES`: Features os modules to be enabled.
#     - `APP_FEATURES`: Features of (rust) apps to be enabled.
# * QEMU options:
#     - `BLK`: Enable storage devices (virtio-blk)
#     - `NET`: Enable network devices (virtio-net)
#     - `GRAPHIC`: Enable display devices and graphic output (virtio-gpu)
#     - `UEFI`: Boot x86_64 via OVMF and the generated `.uefi.img` instead of the default LinuxBoot/direct-boot image
#     - `VIRTIO_9P`: Enable 9P shared filesystem devices (virtio-9p)
#     - `VIRTIO_BUS`: Device bus type: mmio, pci
#     - `MEM`: Memory size (default is 128M)
#     - `DISK_IMG`: Path to the virtual disk image
#     - `ACCEL`: Enable hardware acceleration (KVM on linux)
#     - `QEMU_LOG`: Enable QEMU logging (log file is "qemu.log")
#     - `NET_DUMP`: Enable network packet dump (log file is "netdump.pcap")
#     - `NET_DEV`: QEMU netdev backend types: user, tap, bridge
#     - `VFIO_PCI`: PCI device address in the format "bus:dev.func" to passthrough
#     - `VHOST`: Enable vhost-net for tap backend (only for `NET_DEV=tap`)
# * Network options:
#     - `IP`: IPv4 address (default is 10.0.2.15 for QEMU user netdev)
#     - `GW`: Gateway IPv4 address (default is 10.0.2.2 for QEMU user netdev)

# Enable unstable features
export RUSTC_BOOTSTRAP := 1
export DISK_IMG ?= $(PWD)/disk.img

V ?=
LTO ?=
TARGET_DIR ?= $(PWD)/target
export TARGET_DIR
XCONF_TARGET_DIR ?= $(TARGET_DIR)/tools/xconf
UAPP_TARGET_DIR ?= $(TARGET_DIR)/tools/uapp
HOST_TARGET := $(shell rustc -vV | sed -n 's|host: ||p')
XCONF = env CARGO_BUILD_TARGET=$(HOST_TARGET) RUSTFLAGS= CARGO_ENCODED_RUSTFLAGS= cargo run --target-dir $(XCONF_TARGET_DIR) --manifest-path xtask/xconfig/Cargo.toml --bin xconf --
UAPP_TOOL = env CARGO_BUILD_TARGET=$(HOST_TARGET) RUSTFLAGS= CARGO_ENCODED_RUSTFLAGS= cargo run --target-dir $(UAPP_TARGET_DIR) --manifest-path xtask/uapp/Cargo.toml --
EXTRA_CONFIG ?=
UIMAGE ?= n
export UNITTEST ?= n
export UNITTEST_CRATE ?=
UAPPS ?= all
UAPP_DIR ?= $(PWD)/uapps
UAPP_AUTOSTART_TARGET ?= /etc/profile.d/99-autostart.sh

# App options
A := $(PWD)/entry
APP ?= $(A)

export MEMTRACK ?= n
ifeq ($(MEMTRACK), y)
	FEATURES += memtrack
endif

.DEFAULT_GOAL := all

BUILD_TARGETS := all build run justrun debug clippy disasm rootfs rootfs-uapps teefs uapps
KCONFIG_TARGETS := menuconfig defconfig saveconfig savedefconfig oldconfig olddefconfig
CLEAN_TARGETS := clean distclean
UTILITY_TARGETS := clippy check_deps check_header doc doc_check_missing fmt unittest unittest_no_fail_fast

NON_BUILD_TARGETS := $(KCONFIG_TARGETS) $(CLEAN_TARGETS) $(UTILITY_TARGETS)

REQUESTED_GOALS := $(if $(MAKECMDGOALS),$(MAKECMDGOALS),$(.DEFAULT_GOAL))
PRIMARY_GOAL := $(firstword $(REQUESTED_GOALS))

IS_BUILD := $(filter $(BUILD_TARGETS),$(REQUESTED_GOALS))
IS_NON_BUILD := $(filter $(NON_BUILD_TARGETS),$(REQUESTED_GOALS))
CONFIG_READY_STAMP := .config.prepared

CONFIG_NEEDS_PREPARE :=
CONFIG_PREPARE_REASON :=
ifneq ($(IS_BUILD),)
ifeq ($(wildcard .config),)
  $(error ❌ No .config found. Please copy a platform defconfig to .config, then run: make defconfig)
endif
ifeq ($(wildcard $(CONFIG_READY_STAMP)),)
  CONFIG_NEEDS_PREPARE := y
  CONFIG_PREPARE_REASON := .config has not been expanded yet
else ifeq ($(shell test .config -nt $(CONFIG_READY_STAMP); echo $$?),0)
  CONFIG_NEEDS_PREPARE := y
  CONFIG_PREPARE_REASON := .config changed after the last Kconfig refresh
endif
endif

ifeq ($(CONFIG_NEEDS_PREPARE),y)
.PHONY: $(REQUESTED_GOALS) __prepare_config_then_reexec

# Build-like targets parse ARCH/PLAT/TARGET from `.config` during Makefile
# evaluation, so a copied seed defconfig cannot be fixed up with a normal
# prerequisite. Re-expand `.config` first, then re-exec the original goals in
# a fresh make process that will parse the prepared configuration.
__prepare_config_then_reexec:
	@echo "⚙️  $(CONFIG_PREPARE_REASON); running 'make defconfig' first..."
	@$(MAKE) --no-print-directory defconfig
	@$(MAKE) --no-print-directory $(REQUESTED_GOALS)

$(PRIMARY_GOAL): __prepare_config_then_reexec

$(filter-out $(PRIMARY_GOAL),$(REQUESTED_GOALS)):
	@:
else
ifneq ($(IS_BUILD),)
# Install dependencies
include scripts/make/deps.mk

include scripts/make/kconfig.mk
include scripts/make/deps.mk

export K_ARCH=$(ARCH)
export K_MODE=$(MODE)
export K_TARGET=$(TARGET)
export K_PLAT_NAME=$(PLAT_NAME)
export K_IP=$(IP)
export K_GW=$(GW)
export KBUILD_BUILD_MACHINE ?= $(shell printf '%s@%s' "$$(id -un 2>/dev/null || whoami)" "$$(hostname 2>/dev/null)")
export KBUILD_BUILD_TIME ?= $(shell date -u +"%Y-%m-%dT%H:%M:%SZ")
export KBUILD_BUILD_INFO ?= machine=$(KBUILD_BUILD_MACHINE);time=$(KBUILD_BUILD_TIME)

# Binutils
CROSS_COMPILE ?= $(ARCH)-linux-musl-
CC := $(CROSS_COMPILE)gcc
# Export C toolchain variables for crates that build native support objects.
export CC
AR := $(CROSS_COMPILE)ar
RANLIB := $(CROSS_COMPILE)ranlib
LD := rust-lld -flavor gnu

OBJDUMP ?= rust-objdump -d --print-imm-hex --x86-asm-syntax=intel
OBJCOPY ?= rust-objcopy --binary-architecture=$(ARCH)
GDB ?= gdb

# Paths
OUT_DIR ?= $(PWD)
LD_SCRIPT ?= $(abspath $(TARGET_DIR)/$(TARGET)/$(MODE)/linker_$(PLAT_NAME).lds)
KBUILD_CONFIG_DIR := $(TARGET_DIR)/kbuild/$(PLAT_NAME)

# gen-cargo CLI flags
GEN_CARGO_FLAGS := --ld-script="$(LD_SCRIPT)" $(if $(filter y,$(UNITTEST)),--unittest) $(if $(filter y,$(DWARF)),--dwarf)

# Generate Rust const definitions from .config
CONFIG_RS := $(KBUILD_CONFIG_DIR)/config.rs

APP_NAME := xkernel
OUT_ELF := $(OUT_DIR)/$(APP_NAME)_$(PLAT_NAME).elf
OUT_BIN := $(patsubst %.elf,%.bin,$(OUT_ELF))
OUT_UIMG := $(patsubst %.elf,%.uimg,$(OUT_ELF))
ifeq ($(UIMAGE), y)
  FINAL_IMG := $(OUT_UIMG)
else
  FINAL_IMG := $(OUT_BIN)
endif

all: build

include scripts/make/features.mk
include scripts/make/utils.mk
include scripts/make/build.mk
include scripts/make/qemu.mk
include scripts/make/unittest.mk
ifeq ($(PLAT_NAME), aarch64-raspi4)
  include scripts/make/raspi4.mk
else ifeq ($(PLAT_NAME), aarch64-bsta1000b)
  include scripts/make/bsta1000b-fada.mk
endif

ROOTFS_URL = https://gitee.com/openkylin/x-kernel-image/releases/download/rootfs
ROOTFS_VARIANT ?= alpine-busybox
ROOTFS_IMG = x-kernel-$(ROOTFS_VARIANT)-$(ARCH).img

endif # end of IS_BUILD
endif

include scripts/make/hooks.mk


menuconfig:
	@$(XCONF) menuconfig -k Kconfig -s .
	@if [ -f .config ]; then \
		touch $(CONFIG_READY_STAMP); \
		echo "✅ Configuration saved to .config"; \
	else \
		echo "ℹ️  No changes saved"; \
	fi

rootfs:
	@if [ ! -f $(ROOTFS_IMG) ]; then \
		echo "Image not found, downloading..."; \
		curl -f -L $(ROOTFS_URL)/$(ROOTFS_IMG).xz -O; \
		xz -d $(ROOTFS_IMG).xz; \
	fi
	@cp $(ROOTFS_IMG) $(DISK_IMG)

uapps:
	@if [ ! -f "$(DISK_IMG)" ]; then \
		echo "disk image not found: $(DISK_IMG)"; \
		echo "Please run 'make rootfs' first."; \
		exit 1; \
	fi
	$(UAPP_TOOL) install \
	  --uapps-dir "$(UAPP_DIR)" \
	  --disk-img "$(DISK_IMG)" \
	  --select "$(UAPPS)" \
	  --autostart-target "$(UAPP_AUTOSTART_TARGET)" \
	  --repo-root "$(PWD)" \
	  --build-dir "$(TARGET_DIR)/uapps" \
	  --arch "$(ARCH)" \
	  --target "$(TARGET)" \
	  --plat-name "$(PLAT_NAME)" \
	  --cross-compile "$(CROSS_COMPILE)"

rootfs-uapps: rootfs uapps

teefs:
	$(MAKE) -C tee_apps ARCH=$(ARCH)

defconfig:
	@if [ ! -f .config ]; then \
		echo "$(RED_C)Error$(END_C): .config not found."; \
		echo "Please copy a platform defconfig to .config first."; \
		exit 1; \
	fi
	@$(XCONF) defconfig .config -k Kconfig -s .
	@touch $(CONFIG_READY_STAMP)
	@echo "✅ Default configuration expanded into .config"

saveconfig:
	@$(XCONF) saveconfig -o .config -k Kconfig -s .
	@touch $(CONFIG_READY_STAMP)

savedefconfig:
	@if [ ! -f .config ]; then \
		echo "$(RED_C)Error$(END_C): .config not found."; \
		echo "Please copy a platform defconfig to .config, then run 'make defconfig' first."; \
		exit 1; \
	fi
	@if [ ! -f $(CONFIG_READY_STAMP) ] || [ .config -nt $(CONFIG_READY_STAMP) ]; then \
		echo "$(RED_C)Error$(END_C): .config is not prepared."; \
		echo "Please run 'make defconfig', 'make menuconfig', or another config refresh target first."; \
		exit 1; \
	fi
	@$(XCONF) savedefconfig -c .config -o defconfig -k Kconfig -s .
	@echo "✅ Minimal defconfig saved to ./defconfig"

oldconfig:
	@if [ ! -f .config ]; then \
		echo "$(RED_C)Error$(END_C): .config not found."; \
		echo "Please copy a platform defconfig to .config, then run 'make defconfig' first."; \
		exit 1; \
	fi
	@$(XCONF) oldconfig -c .config -k Kconfig -s .
	@touch $(CONFIG_READY_STAMP)

olddefconfig:
	@if [ ! -f .config ]; then \
		echo "$(RED_C)Error$(END_C): .config not found."; \
		echo "Please copy a platform defconfig to .config, then run 'make defconfig' first."; \
		exit 1; \
	fi
	@$(XCONF) olddefconfig -c .config -k Kconfig -s .
	@touch $(CONFIG_READY_STAMP)
	@echo "✅ New symbols refreshed from Kconfig defaults"


$(CONFIG_RS): .config
	@echo "📝 Generating Rust const definitions from .config..."
	@$(XCONF) gen-const -o $(KBUILD_CONFIG_DIR)
	@echo "✅ Generated config.rs"

_gen_cargo:
	@$(XCONF) gen-cargo $(GEN_CARGO_FLAGS)

# Generate const definitions before build
gen-const: $(CONFIG_RS)

build: $(CONFIG_RS) _gen_cargo $(OUT_DIR) $(FINAL_IMG)

disasm:
	$(OBJDUMP) $(OUT_ELF) | less

run: build justrun

justrun: $(QEMU_RUN_DEPS)
	$(call run_qemu)

debug: build $(QEMU_RUN_DEPS)
	$(call run_qemu_debug) &
	$(GDB) $(OUT_ELF) \
	  -ex 'target remote localhost:1234' \
	  -ex 'b __kplat_main' \
	  -ex 'continue' \
	  -ex 'disp /16i $$pc'

check_deps:
	python3 scripts/check_deps.py

deps:
	python3 scripts/check_deps.py --fix

check_header:
	python3 scripts/check_header.py

header:
	python3 scripts/check_header.py --fix

clippy: check_deps check_header $(CONFIG_RS) _gen_cargo
ifeq ($(origin ARCH), command line)
	$(call cargo_clippy,--target $(TARGET))
else
	$(call cargo_clippy)
endif

fmt:
	cargo +nightly-2026-03-08 fmt --all

unittest:
	$(call unit_test)

unittest_no_fail_fast:
	$(call unit_test,--no-fail-fast)

disk_img:
ifneq ($(wildcard $(DISK_IMG)),)
	@printf "$(YELLOW_C)warning$(END_C): disk image \"$(DISK_IMG)\" already exists!\n"
else
	$(call make_disk_image,fat32,$(DISK_IMG))
endif

clean:
	rm -rf $(CURDIR)/xkernel_*.bin $(CURDIR)/xkernel_*.elf $(CURDIR)/xkernel_*.uimg
	cargo clean --target-dir $(TARGET_DIR)
	cargo clean --target-dir $(XCONF_TARGET_DIR)
	cargo clean --target-dir $(UAPP_TARGET_DIR)
	@rm -rf $(TARGET_DIR)/kbuild

distclean: clean
	@rm -f .config .config.old .config.prepared auto.conf autoconf.h
	@echo "✅ Removed all configuration files"

# Note: gen-const is kept as PHONY to allow manual invocation,
# but the actual dependency is on $(CONFIG_RS) which is file-based
.PHONY: all defconfig oldconfig olddefconfig menuconfig saveconfig savedefconfig gen-const \
	build disasm run justrun debug \
	rootfs rootfs-uapps teefs uapps \
	clippy doc doc_check_missing fmt unittest unittest_no_fail_fast \
	_gen_cargo \
	disk_img clean distclean
