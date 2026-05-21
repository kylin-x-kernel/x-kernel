# Main building script

include scripts/make/cargo.mk

rust_package := $(shell cat $(APP)/Cargo.toml | sed -n 's/^name = "\([a-z0-9A-Z_\-]*\)"/\1/p')
rust_elf := $(TARGET_DIR)/$(TARGET)/$(MODE)/$(rust_package)

ifneq ($(filter $(MAKECMDGOALS),doc doc_check_missing),)
  # run `make doc`
  $(if $(V), $(info RUSTDOCFLAGS: "$(RUSTDOCFLAGS)"))
  export RUSTDOCFLAGS
else ifneq ($(filter $(MAKECMDGOALS),unittest unittest_no_fail_fast),)
  # run `make unittest`
  :
else ifneq ($(filter $(or $(MAKECMDGOALS), $(.DEFAULT_GOAL)), all build run justrun debug),)
  # run `make build` and other above goals
  ifneq ($(V),)
    $(info APP: "$(APP)")
    $(info FEATURES: "$(FEATURES)")
    $(info PLAT_CONFIG: "$(PLAT_CONFIG)")
    $(info x-kernel features: "$(KFEAT)")
    $(info app features: "$(APP_FEAT)")
  endif
  ifeq ($(LTO), y)
    export CARGO_PROFILE_RELEASE_LTO=true
    export CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1
  endif

  ifeq ($(ARCH), x86_64)
    # Bare-metal x86_64 C code runs on kernel stacks, so it must not use the SysV red zone.
    CFLAGS_x86_64_unknown_none += -mcmodel=large -mno-red-zone
    export CFLAGS_x86_64_unknown_none
  endif
endif

_cargo_build: _gen_cargo
	@printf "    $(GREEN_C)Building$(END_C) App: $(APP_NAME), Arch: $(ARCH), Platform: $(PLAT_NAME)\n"
	$(call cargo_build,$(APP),$(KFEAT) $(APP_FEAT))

$(OUT_ELF): $(CONFIG_RS) _cargo_build | $(OUT_DIR)
	$(call run_cmd,cp,$(rust_elf) $@)

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
		-d $(OUT_BIN) $@)

.PHONY: _cargo_build _dwarf
