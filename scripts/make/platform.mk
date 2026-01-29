# Architecture and platform resolving

cargo_manifest_dir := $(APP)

define resolve_config
  $(if $(wildcard $(PLAT_CONFIG)),\
    $(PLAT_CONFIG),\
    $(shell cargo axplat info -C $(cargo_manifest_dir) -c $(PLAT_PACKAGE)))
endef

define validate_config
  $(eval package := $(shell axconfig-gen $(PLAT_CONFIG) -r package 2>/dev/null)) \
  $(if $(strip $(package)),,$(error PLAT_CONFIG=$(PLAT_CONFIG) is not a valid platform configuration file)) \
  $(if $(filter "$(PLAT_PACKAGE)",$(package)),,\
    $(error `PLAT_PACKAGE` field mismatch: expected $(PLAT_PACKAGE), got $(package)))
endef

# `PLAT` is specified, treat it as a package name
PLAT_PACKAGE := $(PLAT)
PLAT_CONFIG := $(strip $(call resolve_config))
ifeq ($(wildcard $(PLAT_CONFIG)),)
  $(error "PLAT=$(PLAT) is not a valid platform package name")
endif
$(call validate_config)

# Read the architecture name from the configuration file
_arch := $(patsubst "%",%,$(shell axconfig-gen $(PLAT_CONFIG) -r arch))
ifeq ($(origin ARCH),command line)
  ifneq ($(ARCH),$(_arch))
    $(error "ARCH=$(ARCH)" is not compatible with "PLAT=$(PLAT)")
  endif
endif
ARCH := $(_arch)

PLAT_NAME := $(patsubst "%",%,$(shell axconfig-gen $(PLAT_CONFIG) -r platform))
