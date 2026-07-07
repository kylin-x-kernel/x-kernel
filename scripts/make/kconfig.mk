# Extract ARCH and PLAT from .config if it exists
# BUT: Skip for Kconfig configuration targets (menuconfig, defconfig, etc.)


# Only read .config if:
# 1. .config exists AND
# 2. We're not running a Kconfig/utility target
ifneq ($(wildcard .config),)
  # Read .config once and extract ARCH and PLAT in a single pass
  CONFIG_VALUES := $(shell awk '/ARCH_[A-Z0-9_]+=y/ { print $$0 } /PLATFORM_[A-Z0-9_]+=y/ { print $$0 } /BUILD_TYPE_[A-Z]+=y/ { print $$0 }' .config 2>/dev/null)

  # APPEND NR_CPUS TO CONFIG_VALUES
  CONFIG_VALUES += $(shell awk '/NR_CPUS=[0-9]+/ { print $$0 }' .config 2>/dev/null)
  CONFIG_VALUES += $(shell awk '/KFEAT_DWARF=y/ { print $$0 }' .config 2>/dev/null)
  CONFIG_VALUES += $(shell awk '/KFEAT_VIRTIO_BUS_(PCI|MMIO)=y/ { print $$0 }' .config 2>/dev/null)
  CONFIG_VALUES += $(shell awk '/KFEAT_VMM=y/ { print $$0 }' .config 2>/dev/null)

  # Parse architecture (only if CONFIG_VALUES is not empty)
  ifneq ($(CONFIG_VALUES),)
    ifeq ($(findstring ARCH_AARCH64=y,$(CONFIG_VALUES)),ARCH_AARCH64=y)
      ARCH_FROM_CONFIG := aarch64
    else ifeq ($(findstring ARCH_RISCV64=y,$(CONFIG_VALUES)),ARCH_RISCV64=y)
      ARCH_FROM_CONFIG := riscv64
    else ifeq ($(findstring ARCH_X86_64=y,$(CONFIG_VALUES)),ARCH_X86_64=y)
      ARCH_FROM_CONFIG := x86_64
    else ifeq ($(findstring ARCH_LOONGARCH64=y,$(CONFIG_VALUES)),ARCH_LOONGARCH64=y)
      ARCH_FROM_CONFIG := loongarch64
    endif

    # Parse platform
    ifeq ($(findstring PLATFORM_AARCH64_QEMU_VIRT=y,$(CONFIG_VALUES)),PLATFORM_AARCH64_QEMU_VIRT=y)
      PLAT_FROM_CONFIG := aarch64-qemu-virt
    else ifeq ($(findstring PLATFORM_AARCH64_CROSVM_VIRT=y,$(CONFIG_VALUES)),PLATFORM_AARCH64_CROSVM_VIRT=y)
      PLAT_FROM_CONFIG := aarch64-crosvm-virt
    else ifeq ($(findstring PLATFORM_AARCH64_RASPI=y,$(CONFIG_VALUES)),PLATFORM_AARCH64_RASPI=y)
      PLAT_FROM_CONFIG := aarch64-raspi
    else ifeq ($(findstring PLATFORM_RISCV64_QEMU_VIRT=y,$(CONFIG_VALUES)),PLATFORM_RISCV64_QEMU_VIRT=y)
      PLAT_FROM_CONFIG := riscv64-qemu-virt
    else ifeq ($(findstring PLATFORM_X86_64_QEMU_VIRT=y,$(CONFIG_VALUES)),PLATFORM_X86_64_QEMU_VIRT=y)
      PLAT_FROM_CONFIG := x86_64-qemu-virt
    else ifeq ($(findstring PLATFORM_LOONGARCH64_QEMU_VIRT=y,$(CONFIG_VALUES)),PLATFORM_LOONGARCH64_QEMU_VIRT=y)
      PLAT_FROM_CONFIG := loongarch64-qemu-virt
    endif

    # Parse mode: BUILD_TYPE_RELEASE or BUILD_TYPE_DEBUG
    ifeq ($(findstring BUILD_TYPE_DEBUG=y,$(CONFIG_VALUES)),BUILD_TYPE_DEBUG=y)
      BUILD_TYPE_FROM_CONFIG := debug
    else ifeq ($(findstring BUILD_TYPE_RELEASE=y,$(CONFIG_VALUES)),BUILD_TYPE_RELEASE=y)
      BUILD_TYPE_FROM_CONFIG := release
    endif

    ifeq ($(findstring KFEAT_DWARF=y,$(CONFIG_VALUES)),KFEAT_DWARF=y)
      DWARF_FROM_CONFIG := y
    else
      DWARF_FROM_CONFIG := n
    endif


    # Parse NR_CPUS (required: compile-time cap that sizes static per-CPU arrays).
    NR_CPUS_FROM_CONFIG := $(shell awk -F= '/NR_CPUS=[0-9]+/ { print $$2 }' .config 2>/dev/null)
    ifeq ($(NR_CPUS_FROM_CONFIG),)
        $(error "`NR_CPUS` is not defined in the .config file")
    endif

    # SMP (the QEMU `-smp` value) is independent of the NR_CPUS cap: the kernel
    # discovers the actual CPU count at runtime from the device tree / ACPI
    # MADT, so `make run SMP=N` neither has to match NR_CPUS nor requires a
    # rebuild. It defaults to NR_CPUS (boot with as many cores as the image
    # supports) and is overridable on the command line or via the environment.
    SMP ?= $(NR_CPUS_FROM_CONFIG)


    # Use config values as defaults, but allow command line override
    ARCH ?= $(ARCH_FROM_CONFIG)
    PLAT ?= $(PLAT_FROM_CONFIG)
    PLAT_NAME ?= $(PLAT)
    MODE ?= $(BUILD_TYPE_FROM_CONFIG)
    DWARF ?= $(DWARF_FROM_CONFIG)
    $(info CONFIG_VALUES: $(CONFIG_VALUES))
    $(info "ARCH from .config: $(ARCH)")
    $(info "PLAT from .config: $(PLAT)")
    $(info "MODE from .config: $(MODE)")
    $(info "DWARF from .config: $(DWARF)")
    $(info "SMP (QEMU -smp, defaults to NR_CPUS, overridable): $(SMP)")
    export ARCH PLAT MODE SMP DWARF
  endif
endif

ifeq ($(ARCH), x86_64)
  TARGET := x86_64-unknown-none
else ifeq ($(ARCH), aarch64)
  TARGET := aarch64-unknown-none-softfloat
else ifeq ($(ARCH), riscv64)
  TARGET := riscv64gc-unknown-none-elf
else ifeq ($(ARCH), loongarch64)
  TARGET := loongarch64-unknown-none-softfloat
else
  $(error "ARCH" must be one of "x86_64", "riscv64", "aarch64" or "loongarch64")
endif
