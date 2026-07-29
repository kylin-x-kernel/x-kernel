# Necessary dependencies for the build system

# Cargo binutils
ifeq ($(shell command -v cargo-objcopy),)
  $(info Installing cargo-binutils...)
  $(shell cargo install cargo-binutils)
endif
