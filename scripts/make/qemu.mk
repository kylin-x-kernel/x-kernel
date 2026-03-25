# QEMU arguments

# QEMU options
BLK ?= y
NET ?= y
GRAPHIC ?= n
INPUT ?= y
VSOCK ?= y
UEFI ?= n
BUS ?= pci
MEM ?= 1g
ACCEL ?= y
ICOUNT ?= n
QEMU_ARGS ?=

export DISK_IMG ?= $(PWD)/disk.img
QEMU_LOG ?= n
NET_DUMP ?= n
NET_DEV ?= user
VFIO_PCI ?=
VHOST ?= n

# Network options
IP ?= 10.0.2.15
GW ?= 10.0.2.2

QEMU := qemu-system-$(ARCH)
QEMU_RUN_DEPS :=

ifeq ($(ARCH), x86_64)
  UEFI_CFG := $(TARGET_DIR)/axboot_$(PLAT_NAME).toml
  OUT_UEFI_IMG := $(OUT_DIR)/$(APP_NAME)_$(PLAT_NAME).uefi.img
  OUT_LINUXBOOT := $(OUT_DIR)/$(APP_NAME)_$(PLAT_NAME).bzimg
  UEFI_IMG_SIZE_MIB ?= 128
  X86_LINUXBOOT_TOOLPREFIX ?= $(CROSS_COMPILE)
  X86_LINUXBOOT_CC ?= $(X86_LINUXBOOT_TOOLPREFIX)gcc
  X86_LINUXBOOT_LD ?= $(X86_LINUXBOOT_TOOLPREFIX)ld
  X86_LINUXBOOT_OBJCOPY ?= $(X86_LINUXBOOT_TOOLPREFIX)objcopy
  BOOTSTUB_PKG := x86_64-boot-stub
  BOOTSTUB_ELF := $(TARGET_DIR)/x86_64-unknown-none/release/$(BOOTSTUB_PKG)
  BOOTSTUB_RAW_BIN := $(TARGET_DIR)/x86_64-unknown-none/release/$(BOOTSTUB_PKG).bin
  BOOTSTUB_SOURCES := $(shell find $(PWD)/boot/x86_64-boot-stub -type f | sort)
  BOOTLOADER_PKG := x86_64-uefi-loader
  BOOTLOADER_EFI := $(TARGET_DIR)/x86_64-unknown-uefi/release/x86_64-uefi-loader.efi
  BOOTLOADER_SOURCES := $(shell find $(PWD)/boot/x86_64-uefi-loader -type f | sort)
  LINUXBOOT_DIR := $(PWD)/boot/x86_64-linuxboot
  LINUXBOOT_SETUP_OBJ := $(TARGET_DIR)/linuxboot_setup_$(PLAT_NAME).o
  LINUXBOOT_SETUP_ELF := $(TARGET_DIR)/linuxboot_setup_$(PLAT_NAME).elf
  LINUXBOOT_SETUP_BIN := $(TARGET_DIR)/linuxboot_setup_$(PLAT_NAME).bin
  LINUXBOOT_MKIMG := $(PWD)/scripts/boot/mk_linuxboot_image.py
  OVMF_CODE := $(firstword $(wildcard /usr/share/OVMF/OVMF_CODE_4M.fd /usr/share/OVMF/OVMF_CODE.fd /usr/share/edk2/x64/OVMF_CODE.fd))
  OVMF_VARS_TEMPLATE := $(firstword $(wildcard /usr/share/OVMF/OVMF_VARS_4M.fd /usr/share/OVMF/OVMF_VARS.fd /usr/share/edk2/x64/OVMF_VARS.fd))
  OVMF_VARS := $(TARGET_DIR)/OVMF_VARS_$(PLAT_NAME).fd
  X86_BOOT_MEDIA := $(OUT_UEFI_IMG) $(OUT_LINUXBOOT)
  build: $(X86_BOOT_MEDIA)
  ifeq ($(UEFI), y)
    ifeq ($(OVMF_CODE),)
      $(error UEFI=y requires OVMF firmware, checked /usr/share/OVMF and /usr/share/edk2/x64)
    endif
    ifeq ($(OVMF_VARS_TEMPLATE),)
      $(error UEFI=y requires an OVMF_VARS firmware template, checked /usr/share/OVMF and /usr/share/edk2/x64)
    endif
    QEMU_RUN_DEPS += $(OUT_UEFI_IMG) $(OVMF_VARS)
  else
    QEMU_RUN_DEPS += $(OUT_LINUXBOOT)
  endif
endif

ifeq ($(BUS), mmio)
  vdev-suffix := device
else ifeq ($(BUS), pci)
  vdev-suffix := pci
else
  $(error "BUS" must be one of "mmio" or "pci")
endif

ifeq ($(ARCH), x86_64)
  machine := q35
else ifeq ($(ARCH), riscv64)
  machine := virt
else ifeq ($(ARCH), aarch64)
  ifeq ($(PLAT_NAME), aarch64-raspi4)
    machine := raspi4b
  else
    machine := virt
  endif
else ifeq ($(ARCH), loongarch64)
  machine := virt
endif

ifeq ($(UEFI), y)
qemu_args-x86_64 := \
  -cpu max \
  -machine $(machine) \
  -drive if=pflash,format=raw,unit=0,file=$(OVMF_CODE),readonly=on \
  -drive if=pflash,format=raw,unit=1,file=$(OVMF_VARS) \
  -drive if=ide,format=raw,index=0,file=$(OUT_UEFI_IMG)
else
qemu_args-x86_64 := \
  -machine $(machine) \
  -kernel $(OUT_LINUXBOOT)
endif

qemu_args-riscv64 := \
  -machine $(machine) \
  -bios default \
  -kernel $(FINAL_IMG)

qemu_args-aarch64 := \
  -cpu cortex-a72 \
  -machine $(machine) \
  -kernel $(FINAL_IMG)

qemu_args-loongarch64 := \
  -machine $(machine) \
  -kernel $(FINAL_IMG)

qemu_args-y := -m $(MEM) -smp $(SMP) $(qemu_args-$(ARCH))

qemu_args-$(BLK) += \
  -device virtio-blk-$(vdev-suffix),drive=disk0 \
  -drive id=disk0,if=none,format=raw,file=$(DISK_IMG)

qemu_args-$(NET) += \
  -device virtio-net-$(vdev-suffix),netdev=net0

ifeq ($(NET_DEV), user)
  qemu_args-$(NET) += -netdev user,id=net0,hostfwd=tcp::5555-:5555,hostfwd=udp::5555-:5555
else ifeq ($(NET_DEV), tap)
  qemu_args-$(NET) += -netdev tap,id=net0,script=scripts/net/qemu-ifup.sh,downscript=no,vhost=$(VHOST),vhostforce=$(VHOST)
  QEMU := sudo $(QEMU)
else ifeq ($(NET_DEV), bridge)
  qemu_args-$(NET) += -netdev bridge,id=net0,br=virbr0
  QEMU := sudo $(QEMU)
else
  $(error "NET_DEV" must be one of "user", "tap", or "bridge")
endif

ifneq ($(VFIO_PCI),)
  qemu_args-y += --device vfio-pci,host=$(VFIO_PCI)
  QEMU := sudo $(QEMU)
endif

ifeq ($(NET_DUMP), y)
  qemu_args-$(NET) += -object filter-dump,id=dump0,netdev=net0,file=netdump.pcap
endif

qemu_args-$(GRAPHIC) += \
  -device virtio-gpu-$(vdev-suffix) -vga none \
  -serial mon:stdio

ifeq ($(GRAPHIC), n)
  qemu_args-y += -nographic
endif

qemu_args-$(INPUT) += \
  -device virtio-mouse-pci -device virtio-keyboard-pci

qemu_args-$(VSOCK) += \
  -device vhost-vsock-pci,id=virtiosocket0,guest-cid=103

ifeq ($(QEMU_LOG), y)
  qemu_args-y += -D qemu.log -d in_asm,int,mmu,pcall,cpu_reset,guest_errors
endif

qemu_args-$(ICOUNT) += -icount shift=1

qemu_args-y += $(QEMU_ARGS)

qemu_args-debug := $(qemu_args-y) -s -S

ifeq ($(ARCH), x86_64)
$(UEFI_CFG): $(OUT_ELF)
	@printf "    $(GREEN_C)Generating$(END_C) axboot config \"$(notdir $@)\" ...\n"
	@printf '# bootloader config\nkernel_paths = ["%s"]\n' "$(notdir $(OUT_ELF))" > $@

$(OUT_UEFI_IMG): $(OUT_ELF) _dwarf $(BOOTLOADER_EFI) $(UEFI_CFG)
	@printf "    $(GREEN_C)Creating$(END_C) x86_64 UEFI disk image \"$(notdir $@)\" ...\n"
	$(call run_cmd,rm,-f $@)
	$(call make_disk_image,fat32,$@,$(UEFI_IMG_SIZE_MIB))
	$(call run_cmd,mmd,-i $@ ::/EFI ::/EFI/BOOT)
	$(call run_cmd,mcopy,-i $@ $(BOOTLOADER_EFI) ::/EFI/BOOT/BOOTX64.EFI)
	$(call run_cmd,mcopy,-i $@ $(OUT_ELF) ::/$(notdir $(OUT_ELF)))
	$(call run_cmd,mcopy,-i $@ $(UEFI_CFG) ::/axboot.toml)

$(OVMF_VARS): $(OVMF_VARS_TEMPLATE)
	@printf "    $(GREEN_C)Preparing$(END_C) OVMF vars \"$(notdir $@)\" ...\n"
	$(call run_cmd,cp,$< $@)

$(LINUXBOOT_SETUP_OBJ): $(LINUXBOOT_DIR)/setup.S
	@printf "    $(GREEN_C)Assembling$(END_C) x86_64 LinuxBoot setup \"$(notdir $@)\" ...\n"
	$(call run_cmd,$(X86_LINUXBOOT_CC),-m32 -c -nostdlib -o $@ $<)

$(LINUXBOOT_SETUP_ELF): $(LINUXBOOT_SETUP_OBJ) $(LINUXBOOT_DIR)/linker.lds
	@printf "    $(GREEN_C)Linking$(END_C) x86_64 LinuxBoot setup \"$(notdir $@)\" ...\n"
	$(call run_cmd,$(X86_LINUXBOOT_LD),-m elf_i386 -T $(LINUXBOOT_DIR)/linker.lds -o $@ $(LINUXBOOT_SETUP_OBJ))

$(LINUXBOOT_SETUP_BIN): $(LINUXBOOT_SETUP_ELF)
	@printf "    $(GREEN_C)Generating$(END_C) x86_64 LinuxBoot setup binary \"$(notdir $@)\" ...\n"
	$(call run_cmd,$(X86_LINUXBOOT_OBJCOPY),-O binary $< $@)

$(BOOTSTUB_RAW_BIN): $(BOOTSTUB_ELF)
	@printf "    $(GREEN_C)Generating$(END_C) x86_64 bootstub raw image \"$(notdir $@)\" ...\n"
	$(call run_cmd,rust-objcopy,--binary-architecture=x86_64 $< --strip-all -O binary $@)

$(OUT_LINUXBOOT): $(LINUXBOOT_SETUP_BIN) $(BOOTSTUB_RAW_BIN) $(BOOTSTUB_ELF) $(OUT_ELF) _dwarf $(LINUXBOOT_MKIMG)
	@printf "    $(GREEN_C)Creating$(END_C) x86_64 direct boot image \"$(notdir $@)\" ...\n"
	$(call run_cmd,python3,$(LINUXBOOT_MKIMG) --setup $(LINUXBOOT_SETUP_BIN) --stub-elf $(BOOTSTUB_ELF) --stub-bin $(BOOTSTUB_RAW_BIN) --kernel $(OUT_ELF) --output $@)
endif

$(BOOTSTUB_ELF): $(BOOTSTUB_SOURCES)
	@printf '$(WHITE_C)cd$(END_C) $(GRAY_C)$(PWD) && env RUSTFLAGS= CARGO_ENCODED_RUSTFLAGS= cargo build -p $(BOOTSTUB_PKG) --target x86_64-unknown-none --release --config target.x86_64-unknown-none.rustflags=[]$(END_C)\n'
	@cd $(PWD) && env RUSTFLAGS= CARGO_ENCODED_RUSTFLAGS= cargo build -p $(BOOTSTUB_PKG) --target x86_64-unknown-none --release --config 'target.x86_64-unknown-none.rustflags=[]'

$(BOOTLOADER_EFI): $(BOOTLOADER_SOURCES)
	@printf '$(WHITE_C)cd$(END_C) $(GRAY_C)$(PWD) && env RUSTFLAGS= CARGO_ENCODED_RUSTFLAGS= cargo build -p $(BOOTLOADER_PKG) --target x86_64-unknown-uefi --target-dir $(TARGET_DIR) --release$(END_C)\n'
	@cd $(PWD) && env RUSTFLAGS= CARGO_ENCODED_RUSTFLAGS= cargo build -p $(BOOTLOADER_PKG) --target x86_64-unknown-uefi --target-dir $(TARGET_DIR) --release

ifeq ($(ACCEL),)
  ifneq ($(findstring -microsoft, $(shell uname -r | tr '[:upper:]' '[:lower:]')),)
    ACCEL := n  # Don't enable kvm for WSL/WSL2
  else ifeq ($(ARCH), x86_64)
    ACCEL := $(if $(filter $(shell uname -m),x86_64),y,n)
  else ifeq ($(ARCH), aarch64)
    ACCEL := $(if $(filter $(shell uname -m),arm64 aarch64),y,n)
  else
    ACCEL := n
  endif
endif

# Do not use KVM for debugging
ifeq ($(shell uname), Darwin)
#   qemu_args-$(ACCEL) += -cpu host -accel hvf
#else ifneq ($(wildcard /dev/kvm),)
#  qemu_args-$(ACCEL) += -cpu host -accel kvm
endif

define run_qemu
  @printf "    $(CYAN_C)Running$(END_C) on qemu...\n"
  $(call run_cmd,$(QEMU),$(qemu_args-y))
  @printf "    $(CYAN_C)Finished$(END_C) running on qemu.\n"
  $(if $(filter y,$(UNITTEST)),$(call coverage_report))
endef

define run_qemu_debug
  @printf "    $(CYAN_C)Debugging$(END_C) on qemu...\n"
  $(call run_cmd,$(QEMU),$(qemu_args-debug))
endef
