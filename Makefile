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
#     - `BUS`: Device bus type: mmio, pci
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
XCONF = env RUSTFLAGS= CARGO_ENCODED_RUSTFLAGS= cargo run --manifest-path xtask/xconfig/Cargo.toml --bin xconf --

V ?=
LTO ?=
TARGET_DIR ?= $(PWD)/target
EXTRA_CONFIG ?=
UIMAGE ?= n
export UNITTEST ?= n
export UNITTEST_CRATE ?=

# App options
A := $(PWD)/entry
APP ?= $(A)

export MEMTRACK := n
ifeq ($(MEMTRACK), y)
	APP_FEATURES += kapi/memtrack
endif

.DEFAULT_GOAL := all

BUILD_TARGETS := all build run justrun debug clippy disasm rootfs
KCONFIG_TARGETS := menuconfig defconfig saveconfig oldconfig
CLEAN_TARGETS := clean clean_c distclean
UTILITY_TARGETS := clippy doc doc_check_missing fmt unittest unittest_no_fail_fast

NON_BUILD_TARGETS := $(KCONFIG_TARGETS) $(CLEAN_TARGETS) $(UTILITY_TARGETS)

CURRENT_GOAL := $(or $(MAKECMDGOALS),$(.DEFAULT_GOAL))

IS_BUILD := $(filter $(BUILD_TARGETS),$(CURRENT_GOAL))
IS_NON_BUILD := $(filter $(NON_BUILD_TARGETS),$(CURRENT_GOAL))

# Install dependencies
include scripts/make/deps.mk

# .config check
ifneq ($(IS_BUILD),)
ifeq ($(wildcard .config),)
  $(error ❌ No .config found. Please run: make menuconfig)
endif

include scripts/make/kconfig.mk
include scripts/make/deps.mk

export K_ARCH=$(ARCH)
export K_MODE=$(MODE)
export K_LOG=$(LOG)
export K_TARGET=$(TARGET)
export K_IP=$(IP)
export K_GW=$(GW)
export KBUILD_BUILD_MACHINE ?= $(shell printf '%s@%s' "$$(id -un 2>/dev/null || whoami)" "$$(hostname 2>/dev/null)")
export KBUILD_BUILD_TIME ?= $(shell date -u +"%Y-%m-%dT%H:%M:%SZ")
export KBUILD_BUILD_INFO ?= machine=$(KBUILD_BUILD_MACHINE);time=$(KBUILD_BUILD_TIME)

# Binutils
CROSS_COMPILE ?= $(ARCH)-linux-musl-
CC := $(CROSS_COMPILE)gcc
# A temp export for rust-dice, after we change to real rust dice we need remove it
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

# Generate Rust const definitions from .config
CONFIG_RS := $(TARGET_DIR)/kbuild/config.rs

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

ROOTFS_URL = https://gitee.com/openkylin/x-kernel-image/releases/download/20260302/
ROOTFS_IMG = rootfs-$(ARCH).img

endif # end of IS_BUILD


menuconfig:
	@$(XCONF) menuconfig -k Kconfig -s .
	@if [ -f .config ]; then \
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

teefs:
	$(MAKE) -C tee_apps ARCH=$(ARCH)

defconfig:
	@$(XCONF) saveconfig -o .config -k Kconfig -s .
	@echo "✅ Default configuration saved to .config"

saveconfig:
	@$(XCONF) saveconfig -o .config -k Kconfig -s .

oldconfig:
	@if [ ! -f .config ]; then \
		echo "$(RED_C)Error$(END_C): .config not found."; \
		echo "Please run 'make defconfig' or 'make menuconfig' first."; \
		exit 1; \
	fi
	@$(XCONF) oldconfig -c .config -k Kconfig -s .


$(CONFIG_RS): .config
	@echo "📝 Generating Rust const definitions from .config..."
	@$(XCONF) gen-const
	@echo "✅ Generated config.rs"


# Generate const definitions before build
gen-const: $(CONFIG_RS)

build: $(CONFIG_RS) $(OUT_DIR) $(FINAL_IMG)

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

clippy: $(CONFIG_RS)
ifeq ($(origin ARCH), command line)
	$(call cargo_clippy,--target $(TARGET))
else
	$(call cargo_clippy)
endif

doc:
	$(call cargo_doc)

doc_check_missing:
	$(call cargo_doc)

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

clean: clean_c
	rm -rf $(APP)/*.bin $(APP)/*.elf
	cargo clean
	@rm -f target/kbuild/config.rs .cargo/config.toml

distclean: clean
	@rm -f .config .config.old auto.conf autoconf.h
	@echo "✅ Removed all configuration files"

clean_c::
	rm -rf $(app-objs)

# Note: gen-const is kept as PHONY to allow manual invocation,
# but the actual dependency is on $(CONFIG_RS) which is file-based
.PHONY: all defconfig oldconfig menuconfig saveconfig gen-const \
	build disasm run justrun debug \
	clippy doc doc_check_missing fmt fmt_c unittest unittest_no_fail_fast \
	disk_img clean distclean clean_c
