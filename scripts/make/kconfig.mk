# Extract ARCH and PLAT from .config if it exists
# BUT: Skip for Kconfig configuration targets (menuconfig, defconfig, etc.)


# Only read .config if:
# 1. .config exists AND
# 2. We're not running a Kconfig/utility target
ifneq ($(wildcard .config),)
  # Read .config once and extract ARCH and PLAT in a single pass
  CONFIG_VALUES := $(shell awk '/ARCH_[A-Z0-9_]+=y/ { print $$0 } /PLATFORM_[A-Z0-9_]+=y/ { print $$0 } /BUILD_TYPE_[A-Z]+=y/ { print $$0 }' .config 2>/dev/null)

  # Append LOG_LEVEL_XXX TO CONFIG_VALUES
  CONFIG_VALUES += $(shell awk '/LOG_LEVEL_[A-Z0-9_]+=y/ { print $$0 }' .config 2>/dev/null)

  # APPEND CPU_NUM TO CONFIG_VALUES
  CONFIG_VALUES += $(shell awk '/CPU_NUM=[0-9]+/ { print $$0 }' .config 2>/dev/null)

  # Append KERNEL_BASE_PADDR=XXX TO CONFIG_VALUES
  CONFIG_VALUES += $(shell awk '/KERNEL_BASE_PADDR=0x[0-9a-fA-F]+/ { print $$0 }' .config 2>/dev/null)
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
    else ifeq ($(findstring PLATFORM_X86_CSV=y,$(CONFIG_VALUES)),PLATFORM_X86_CSV=y)
      PLAT_FROM_CONFIG := x86-csv
    else ifeq ($(findstring PLATFORM_LOONGARCH64_QEMU_VIRT=y,$(CONFIG_VALUES)),PLATFORM_LOONGARCH64_QEMU_VIRT=y)
      PLAT_FROM_CONFIG := loongarch64-qemu-virt
    endif

    # Parse mode: BUILD_TYPE_RELEASE or BUILD_TYPE_DEBUG
    ifeq ($(findstring BUILD_TYPE_DEBUG=y,$(CONFIG_VALUES)),BUILD_TYPE_DEBUG=y)
      BUILD_TYPE_FROM_CONFIG := debug
    else ifeq ($(findstring BUILD_TYPE_RELEASE=y,$(CONFIG_VALUES)),BUILD_TYPE_RELEASE=y)
      BUILD_TYPE_FROM_CONFIG := release
    endif

    # Parse log level
    ifeq ($(findstring LOG_LEVEL_ERROR=y,$(CONFIG_VALUES)),LOG_LEVEL_ERROR=y)
      LOG_LEVEL_FROM_CONFIG := error
    else ifeq ($(findstring LOG_LEVEL_WARN=y,$(CONFIG_VALUES)),LOG_LEVEL_WARN=y)
      LOG_LEVEL_FROM_CONFIG := warn
    else ifeq ($(findstring LOG_LEVEL_INFO=y,$(CONFIG_VALUES)),LOG_LEVEL_INFO=y)
      LOG_LEVEL_FROM_CONFIG := info
    else ifeq ($(findstring LOG_LEVEL_DEBUG=y,$(CONFIG_VALUES)),LOG_LEVEL_DEBUG=y)
      LOG_LEVEL_FROM_CONFIG := debug
    else ifeq ($(findstring LOG_LEVEL_TRACE=y,$(CONFIG_VALUES)),LOG_LEVEL_TRACE=y)
      LOG_LEVEL_FROM_CONFIG := trace
    endif

    # Parse CPU_NUM
    CPU_NUM_FROM_CONFIG := $(shell awk -F= '/CPU_NUM=[0-9]+/ { print $$2 }' .config 2>/dev/null)
    ifneq ($(CPU_NUM_FROM_CONFIG),)
       SMP := $(CPU_NUM_FROM_CONFIG)
    else
   	$(error "`CPU_NUM` is not defined in the .config file")
    endif

    # Parse KERNEL_BASE_PADDR
    KERNEL_BASE_PADDR_FROM_CONFIG := $(shell awk -F= '/KERNEL_BASE_PADDR=0x[0-9a-fA-F]+/ { print $$2 }' .config 2>/dev/null)
    ifneq ($(KERNEL_BASE_PADDR_FROM_CONFIG),)
      KERNEL_BASE_PADDR := $(KERNEL_BASE_PADDR_FROM_CONFIG)
    else
      $(error "`KERNEL_BASE_PADDR` is not defined in the .config file")
    endif


    # Use config values as defaults, but allow command line override
    ARCH ?= $(ARCH_FROM_CONFIG)
    PLAT ?= $(PLAT_FROM_CONFIG)
    PLAT_NAME ?= $(PLAT)
    MODE ?= $(BUILD_TYPE_FROM_CONFIG)
    LOG ?= $(LOG_LEVEL_FROM_CONFIG)
    $(info CONFIG_VALUES: $(CONFIG_VALUES))
    $(info "ARCH from .config: $(ARCH)")
    $(info "PLAT from .config: $(PLAT)")
    $(info "MODE from .config: $(MODE)")
    $(info "LOG from .config: $(LOG)")
    $(info "SMP from .config: $(SMP)")
    $(info "KERNEL_BASE_PADDR from .config: $(KERNEL_BASE_PADDR)")
    export ARCH PLAT MODE LOG SMP KERNEL_BASE_PADDR
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
