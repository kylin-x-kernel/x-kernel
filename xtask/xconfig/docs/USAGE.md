# Rust Kbuild - Usage Guide

## Installation

### From Source
```bash
git clone https://github.com/guoweikang/rust-kbuild.git
cd rust-kbuild
cargo build --release
cargo install --path .
```

## Command Line Interface

The `xconf` binary provides several commands for working with Kconfig files.

### Parse Command

Parse a Kconfig file and display its AST:

```bash
xconf parse --kconfig Kconfig --srctree .
```

Options:
- `-k, --kconfig <PATH>`: Path to Kconfig file (default: "Kconfig")
- `-s, --srctree <PATH>`: Source tree path (default: ".")

Example:
```bash
xconf parse --kconfig examples/sample_project/Kconfig --srctree examples/sample_project
```

### Defconfig Command

Apply a defconfig file (not yet implemented):

```bash
xconf defconfig <defconfig-path> --kconfig Kconfig --srctree .
```

### Menuconfig Command

Interactive TUI configuration (not yet implemented):

```bash
xconf menuconfig --kconfig Kconfig --srctree .
```

### Generate Command

Generate configuration files from .config:

```bash
xconf generate --config .config --kconfig Kconfig --srctree .
```

This command generates:
- `auto.conf`: Configuration file for makefiles
- `autoconf.h`: C header file with configuration macros

## Kconfig Syntax Support

Currently supported Kconfig syntax:

### Basic Configuration
```
config MY_OPTION
    bool "My option description"
    default y
    help
      Detailed help text for this option.
```

### Configuration Types
- `bool`: Boolean (y/n)
- `tristate`: Tristate (y/m/n)
- `string`: String value
- `int`: Integer value
- `hex`: Hexadecimal value

### Dependencies
```
config OPTION_A
    bool "Option A"
    depends on OPTION_B
```

### Select/Imply
```
config OPTION_A
    bool "Option A"
    select OPTION_B
    imply OPTION_C
```

### Menus
```
menu "Subsystem Configuration"
    depends on ENABLE_SUBSYSTEM

config SUB_OPTION
    bool "Sub option"

endmenu
```

### Source Directives
```
source "arch/x86/Kconfig"
source "drivers/Kconfig"
```

**Note**: Source directives support:
- Recursive file inclusion
- Circular dependency detection
- Relative paths from source tree root

### Choice
```
choice
    prompt "Select architecture"
    default X86

config X86
    bool "x86"

config ARM
    bool "ARM"

endchoice
```

### If Blocks
```
if ADVANCED
    config ADVANCED_OPTION
        bool "Advanced option"
endif
```

## Examples

See the `examples/sample_project` directory for a complete example with:
- Main Kconfig file
- Architecture-specific Kconfig files
- Kernel configuration
- Nested source directives

## Troubleshooting

### Circular Dependency Error
```
Error: Recursive source inclusion detected: Kconfig -> sub/Kconfig -> Kconfig
```

This error occurs when source directives create a circular dependency. Check your source directives to ensure no file includes itself directly or indirectly.

### File Not Found
```
Error: File not found: arch/x86/Kconfig
```

Ensure that:
1. The source tree path is correct
2. The referenced files exist relative to the source tree
3. File paths in source directives are relative to the source tree root
