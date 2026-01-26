# platconfig

Platform-specific constants and parameters for X-Kernel.

## Overview

This crate provides compile-time configuration constants for the X-Kernel operating system. It reads platform-specific configuration from TOML files and generates Rust constants that can be used throughout the kernel.

## Usage

The configuration is loaded at compile time through the `PLAT_CONFIG_PATH` environment variable. If not set, it falls back to `configs/dummy.toml`.

```rust
use platconfig::TASK_STACK_SIZE;
use platconfig::TICKS_PER_SEC;

// Use the configuration constants
let stack_size = TASK_STACK_SIZE;
let tick_rate = TICKS_PER_SEC;
```

## Configuration Files

Configuration files are located in the `configs/` directory at the project root:

- `defconfig.toml` - Default configuration
- `dummy.toml` - Dummy configuration for testing
- `custom/*.toml` - Platform-specific configurations

## Environment Variables

- `PLAT_CONFIG_PATH`: Path to the configuration file to use

## Build System Integration

The build system automatically sets `PLAT_CONFIG_PATH` based on the target platform. See the main `Makefile` for details.

## Implementation

This crate uses the `axconfig-macros` procedural macro to parse TOML configuration files and generate Rust constants at compile time.

## License

Apache License 2.0
