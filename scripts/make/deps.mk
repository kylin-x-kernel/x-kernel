# Necessary dependencies for the build system

# Tool to generate xconfig
ifeq ($(shell xconfig --version 2>/dev/null),)
  $(info Installing xconfig...)
  $(shell cargo install --path xtask/xconfig)
endif

# Cargo binutils
ifeq ($(shell cargo install --list | grep cargo-binutils),)
  $(info Installing cargo-binutils...)
  $(shell cargo install cargo-binutils)
endif
