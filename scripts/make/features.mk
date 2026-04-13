# Features resolving.
#   - `KFEAT`: features to be enabled for modules (crate `kfeat`).
#   - `APP_FEAT`: features to be enabled for the Rust app.

kfeat_prefix := kfeat/
kfeat :=
config_kfeat := $(shell ./scripts/make/kfeat_features.sh .config)

ifeq ($(BUS),mmio)
  kfeat += bus-mmio
endif

kfeat += $(config_kfeat)

APP_FEATURES += $(subst -,_,$(PLAT))

KFEAT := $(strip $(addprefix $(kfeat_prefix),$(kfeat)))
APP_FEAT := $(strip $(shell echo $(APP_FEATURES) | tr ',' ' '))
