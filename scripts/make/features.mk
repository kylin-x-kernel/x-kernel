# Features resolving.
#   - `KFEAT`: features to be enabled for modules (crate `kfeat`).
#   - `APP_FEAT`: features to be enabled for the Rust app.

kfeat_prefix := kfeat/
kfeat := $(strip $(shell echo $(FEATURES) | tr ',' ' '))
config_kfeat := $(shell ./scripts/make/kfeat_features.sh .config)

kfeat += $(config_kfeat)

KFEAT := $(strip $(addprefix $(kfeat_prefix),$(kfeat)))
APP_FEAT := $(strip $(shell echo $(APP_FEATURES) | tr ',' ' '))
