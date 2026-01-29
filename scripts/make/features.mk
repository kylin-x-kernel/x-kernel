# Features resolving.
#   - `KFEAT`: features to be enabled for modules (crate `kfeat`).
#   - `APP_FEAT`: features to be enabled for the Rust app.

kfeat_prefix := kfeat/
kfeat :=

ifeq ($(filter $(LOG),off error warn info debug trace),)
  $(error "LOG" must be one of "off", "error", "warn", "info", "debug", "trace")
endif

ifeq ($(BUS),mmio)
  kfeat += bus-mmio
endif

ifeq ($(DWARF),y)
  kfeat += dwarf
endif

APP_FEATURES += $(subst -,_,$(PLAT))

KFEAT := $(strip $(addprefix $(kfeat_prefix),$(kfeat)))
APP_FEAT := $(strip $(shell echo $(APP_FEATURES) | tr ',' ' '))
