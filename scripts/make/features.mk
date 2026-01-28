# Features resolving.
#
# Inputs:
#   - `FEATURES`: a list of features to be enabled split by spaces or commas.
#     The features can be selected from the crate `kfeat` or the user library
#     (crate `axstd` or `axlibc`).
#   - `APP_FEATURES`: a list of features to be enabled for the Rust app.
#
# Outputs:
#   - `KFEAT`: features to be enabled for ArceOS modules (crate `kfeat`).
#   - `LIB_FEAT`: features to be enabled for the user library (crate `axstd`, `axlibc`).
#   - `APP_FEAT`: features to be enabled for the Rust app.

ifeq ($(APP_TYPE),c)
  kfeat_prefix := kfeat/
  lib_features := fp-simd irq alloc fs net fd pipe select epoll
else
  ifeq ($(NO_AXSTD),y)
    kfeat_prefix := kfeat/
  else
    kfeat_prefix := axstd/
  endif
  lib_features :=
endif

lib_feat_prefix := $(AX_LIB)/

override FEATURES := $(shell echo $(FEATURES) | tr ',' ' ')

ifeq ($(APP_TYPE), c)
  ifneq ($(wildcard $(APP)/features.txt),)    # check features.txt exists
    override FEATURES += $(shell cat $(APP)/features.txt)
  endif
  ifneq ($(filter fs net pipe select epoll,$(FEATURES)),)
    override FEATURES += fd
  endif
endif

override FEATURES := $(strip $(FEATURES))

kfeat :=
lib_feat :=

ifneq ($(MYPLAT),)
  kfeat += myplat
else
  kfeat += defplat
endif

ifeq ($(filter $(LOG),off error warn info debug trace),)
  $(error "LOG" must be one of "off", "error", "warn", "info", "debug", "trace")
endif

ifeq ($(BUS),mmio)
  kfeat += bus-mmio
endif

ifeq ($(DWARF),y)
  kfeat += dwarf
endif

ifeq ($(shell test $(SMP) -gt 1; echo $$?),0)
  lib_feat += smp
endif

kfeat += $(filter-out $(lib_features),$(FEATURES))
lib_feat += $(filter $(lib_features),$(FEATURES))

KFEAT := $(strip $(addprefix $(kfeat_prefix),$(kfeat)))
LIB_FEAT := $(strip $(addprefix $(lib_feat_prefix),$(lib_feat)))
APP_FEAT := $(strip $(shell echo $(APP_FEATURES) | tr ',' ' '))
