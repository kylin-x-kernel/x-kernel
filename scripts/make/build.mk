# Main building script

include scripts/make/cargo.mk

rust_package := $(shell cat $(APP)/Cargo.toml | sed -n 's/^name = "\([a-z0-9A-Z_\-]*\)"/\1/p')
rust_elf := $(TARGET_DIR)/$(TARGET)/$(MODE)/$(rust_package)

ifneq ($(filter $(MAKECMDGOALS),doc doc_check_missing),)
  # run `make doc`
  $(if $(V), $(info RUSTFLAGS: "$(RUSTFLAGS)") $(info RUSTDOCFLAGS: "$(RUSTDOCFLAGS)"))
  export RUSTFLAGS
  export RUSTDOCFLAGS
else ifneq ($(filter $(MAKECMDGOALS),unittest unittest_no_fail_fast),)
  # run `make unittest`
  $(if $(V), $(info RUSTFLAGS: "$(RUSTFLAGS)"))
  export RUSTFLAGS
else ifneq ($(filter $(or $(MAKECMDGOALS), $(.DEFAULT_GOAL)), all build run justrun debug),)
  # run `make build` and other above goals
  ifneq ($(V),)
    $(info APP: "$(APP)")
    $(info FEATURES: "$(FEATURES)")
    $(info PLAT_CONFIG: "$(PLAT_CONFIG)")
    $(info x-kernel features: "$(KFEAT)")
    $(info app features: "$(APP_FEAT)")
  endif
    RUSTFLAGS += $(RUSTFLAGS_LINK_ARGS)
  ifeq ($(DWARF), y)
    RUSTFLAGS += -C force-frame-pointers -C debuginfo=2 -C strip=none
  endif
  $(if $(V), $(info RUSTFLAGS: "$(RUSTFLAGS)"))
  export RUSTFLAGS
  ifeq ($(LTO), y)
    export CARGO_PROFILE_RELEASE_LTO=true
    export CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1
  endif
endif

ifeq ($(UNITTEST), y)
  RUSTFLAGS += --cfg unittest --check-cfg cfg(unittest)
  APP_FEAT += unittest
else
  RUSTFLAGS += --check-cfg cfg(unittest)
endif

_cargo_build: oldconfig
	@printf "    $(GREEN_C)Building$(END_C) App: $(APP_NAME), Arch: $(ARCH), Platform: $(PLAT_NAME)\n"
	$(call cargo_build,$(APP),$(KFEAT) $(APP_FEAT))
	@cp $(rust_elf) $(OUT_ELF)

$(OUT_DIR):
	$(call run_cmd,mkdir,-p $@)

_dwarf: $(OUT_ELF)
ifeq ($(DWARF), y)
	$(call run_cmd,./scripts/make/dwarf.sh,$(OUT_ELF) $(OBJCOPY))
endif

$(OUT_BIN): _cargo_build $(OUT_ELF) _dwarf
	$(call run_cmd,$(OBJCOPY),$(OUT_ELF) --strip-all -O binary $@)
	@if [ ! -s $(OUT_BIN) ]; then \
		echo 'Empty kernel image "$(notdir $(FINAL_IMG))" is built, please check your build configuration'; \
		exit 1; \
	fi

ifeq ($(ARCH), aarch64)
  uimg_arch := arm64
else ifeq ($(ARCH), riscv64)
  uimg_arch := riscv
else
  uimg_arch := $(ARCH)
endif

$(OUT_UIMG): $(OUT_BIN)
	$(call run_cmd,mkimage,\
		-A $(uimg_arch) -O linux -T kernel -C none \
		-a $(subst _,,$(shell axconfig-gen "$(OUT_CONFIG)" -r plat.kernel-base-paddr)) \
		-d $(OUT_BIN) $@)

.PHONY: _cargo_build _dwarf
