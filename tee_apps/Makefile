ROOT_DIR := $(abspath ..)
TARGET_DIR ?= $(ROOT_DIR)/target
ROOTFS_SIZE ?= 64M

APP_DIRS := $(patsubst %/Cargo.toml,%,$(wildcard */Cargo.toml))

ifeq ($(ARCH), aarch64)
  TARGET := aarch64-unknown-linux-musl
  LINKER := aarch64-linux-musl-gcc
else ifeq ($(ARCH), x86_64)
  TARGET := x86_64-unknown-linux-musl
  LINKER := x86_64-linux-musl-gcc
else
  $(error "ARCH" must be "aarch64" or "x86_64")
endif

APP_NAMES := $(foreach app,$(APP_DIRS),$(shell sed -n 's/^name *= *"\(.*\)"/\1/p' $(app)/Cargo.toml | head -n 1))
SH_APP := sh
TEE_APP_NAMES := $(filter-out $(SH_APP),$(APP_NAMES))
TEE_INIT_APPS := $(shell printf '/tee/%s,' $(TEE_APP_NAMES) | sed 's/,$$//')
APP_BINS := $(foreach name,$(APP_NAMES),$(TARGET_DIR)/$(TARGET)/release/$(name))
COPY_ARGS := --copy $(TARGET_DIR)/$(TARGET)/release/$(SH_APP):/bin/$(SH_APP) \
$(foreach name,$(TEE_APP_NAMES),--copy $(TARGET_DIR)/$(TARGET)/release/$(name):/tee/$(name))

TARGET_UPPER := $(shell echo $(TARGET) | tr a-z- A-Z_)
LINKER_ENV := CARGO_TARGET_$(TARGET_UPPER)_LINKER
CARGO_ENV := RUSTFLAGS= CC=$(LINKER) $(LINKER_ENV)=$(LINKER)
CARGO_BUILD := cargo build --release --target $(TARGET)

.PHONY: tee_apps
tee_apps:
	@set -e; \
	for app in $(APP_DIRS); do \
		echo "==> Building $$app ($(TARGET))"; \
		if [ "$$app" = "$(SH_APP)" ]; then \
			TEE_INIT_APPS="$(TEE_INIT_APPS)" $(CARGO_ENV) \
				$(CARGO_BUILD) --manifest-path $$app/Cargo.toml; \
		else \
			$(CARGO_ENV) $(CARGO_BUILD) --manifest-path $$app/Cargo.toml; \
		fi; \
	done
	@set -e; \
	for name in $(APP_NAMES); do \
		bin="$(TARGET_DIR)/$(TARGET)/release/$$name"; \
		if [ ! -f "$$bin" ]; then \
			echo "missing binary $$bin"; \
			exit 1; \
		fi; \
	done; \
	echo "==> Creating $(DISK_IMG)"; \
	cd $(ROOT_DIR) && env -u CARGO_BUILD_TARGET RUSTFLAGS= cargo run -p crate_rootfs --release -- \
		--image $(DISK_IMG) --size-bytes $(ROOTFS_SIZE) $(COPY_ARGS)