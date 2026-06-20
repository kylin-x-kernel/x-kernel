# Cargo features and build args

ifeq ($(V),1)
  verbose := -v
else ifeq ($(V),2)
  verbose := -vv
else
  verbose :=
endif

build_args-release := --release

build_args := \
  --target $(TARGET) \
  --target-dir $(TARGET_DIR) \
  $(build_args-$(MODE)) \
  $(verbose)

RUSTDOCFLAGS := -Z unstable-options --enable-index-page -D rustdoc::broken_intra_doc_links --check-cfg cfg(unittest)

ifeq ($(MAKECMDGOALS), doc_check_missing)
  RUSTDOCFLAGS += -D missing-docs
endif

define cargo_build
  $(call run_cmd,cargo build --manifest-path $(1)/Cargo.toml,$(build_args) --features "$(strip $(2))")
endef

clippy_args := -A unsafe_op_in_unsafe_fn -D clippy::undocumented_unsafe_blocks -D warnings

define cargo_clippy
  $(call run_cmd,cargo clippy --manifest-path $(APP)/Cargo.toml,-p $(rust_package) --target $(TARGET) --target-dir $(TARGET_DIR) --features "$(strip $(KFEAT) $(APP_FEAT))" $(1) $(verbose) -- $(clippy_args))
endef

package_roots := api arch boot core drivers fs io mm net process tee util
crate_dirs := $(sort $(foreach root,$(package_roots),$(dir $(wildcard $(CURDIR)/$(root)/*/Cargo.toml))))
all_packages := $(notdir $(patsubst %/,%,$(crate_dirs)))

define unit_test
  $(call run_cmd,cargo test,-p kfs $(1) $(verbose) -- --nocapture)
  $(call run_cmd,cargo test,-p kfs $(1) --features "myfs" $(verbose) -- --nocapture)
  $(call run_cmd,cargo test,--workspace --exclude kfs $(1) $(verbose) -- --nocapture)
endef
