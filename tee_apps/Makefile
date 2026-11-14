ROOT_DIR := $(abspath ..)
TARGET_DIR ?= $(ROOT_DIR)/target
ROOTFS_SIZE ?= 64M

APP_DIRS := $(patsubst %/Cargo.toml,%,$(wildcard */Cargo.toml))

ifeq ($(ARCH), aarch64)
  TARGET := aarch64-unknown-linux-gnu
  LINKER := aarch64-linux-gnu-gcc
else ifeq ($(ARCH), x86_64)
  TARGET := x86_64-unknown-linux-gnu
  LINKER := x86_64-linux-gnu-gcc
else
  $(error "ARCH" must be "aarch64" or "x86_64")
endif

APP_NAMES := $(foreach app,$(APP_DIRS),$(shell sed -n 's/^name *= *"\(.*\)"/\1/p' $(app)/Cargo.toml | head -n 1))
SH_APP := sh
TEE_APP_NAMES := $(filter-out $(SH_APP),$(APP_NAMES))
TEE_INIT_APPS := $(shell printf '/tee/%s,' $(TEE_APP_NAMES) | sed 's/,$$//')
APP_BINS := $(foreach name,$(APP_NAMES),$(TARGET_DIR)/$(TARGET)/release/$(name))

TARGET_UPPER := $(shell echo $(TARGET) | tr a-z- A-Z_)
LINKER_ENV := CARGO_TARGET_$(TARGET_UPPER)_LINKER
CARGO_ENV := RUSTFLAGS="-C target-feature=-crt-static" CC=$(LINKER) $(LINKER_ENV)=$(LINKER)
CARGO_BUILD := cargo build --release --target $(TARGET) --target-dir $(TARGET_DIR)

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
	echo "==> Creating glibc $(DISK_IMG)"; \
	rootfs=$$(../scripts/ci/prepare_rootfs_cache.sh "$(ARCH)" | tail -n 1); \
	cp --reflink=auto "$$rootfs" "$(DISK_IMG)"; \
	commands="$(TARGET_DIR)/tee-apps-$(ARCH).debugfs"; \
	{ \
		printf 'cd /usr/bin\nrm sh\nwrite %s sh\nset_inode_field sh mode 0100755\ncd /tee\n' "$(TARGET_DIR)/$(TARGET)/release/$(SH_APP)"; \
		for name in $(TEE_APP_NAMES); do \
			printf 'rm %s\nwrite %s %s\nset_inode_field %s mode 0100755\n' "$$name" "$(TARGET_DIR)/$(TARGET)/release/$$name" "$$name" "$$name"; \
		done; \
	} > "$$commands"; \
	debugfs -w -f "$$commands" "$(DISK_IMG)"
