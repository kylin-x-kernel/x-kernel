# rust-kbuild

> A modern Rust implementation of Linux Kconfig for large-scale Rust project configuration

[![Crates.io](https://img.shields.io/crates/v/rust-kbuild.svg)](https://crates.io/crates/rust-kbuild)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)
[![Build Status](https://github.com/guoweikang/rust-kbuild/workflows/CI/badge.svg)](https://github.com/guoweikang/rust-kbuild/actions)

## What is rust-kbuild?

rust-kbuild brings the powerful Linux kernel Kconfig system to Rust projects. Instead of managing dozens of Cargo features, use hierarchical, dependency-aware configuration that scales.

### Why rust-kbuild?

**Problem**: Large Rust projects with many compile-time options become unmaintainable with Cargo features:

```toml
# Cargo.toml becomes a mess
[features]
logging = []
logging_json = ["logging"]
logging_pretty = ["logging"]
network = []
network_http = ["network"]
network_grpc = ["network"]
# ... hundreds more
```

**Solution**: Use Kconfig for clean, hierarchical configuration:

```
menu "Logging Configuration"
    config ENABLE_LOGGING
        bool "Enable logging system"
        default y

    choice
        prompt "Log format"
        depends on ENABLE_LOGGING

    config LOG_FORMAT_JSON
        bool "JSON format"

    config LOG_FORMAT_PRETTY
        bool "Pretty format"
    endchoice
endmenu
```

### Key Features

- ✅ **Linux-Compatible Syntax**: Full Kconfig language support (bool, tristate, string, int, hex)
- ✅ **Dependency Management**: `depends on`, `select`, `imply` directives
- ✅ **Fast & Safe**: Written in Rust for performance and memory safety

## 🚀 Quick Start

### Installation

cargo install --path .

### Your First Configuration (3 Steps)

**Step 1: Create a Kconfig file**

```bash
cat > Kconfig <<'EOF'
mainmenu "My Project Configuration"

config ENABLE_LOGGING
    bool "Enable logging"
    default y
    help
      Enable the logging system.

config LOG_LEVEL
    string "Log level"
    depends on ENABLE_LOGGING
    default "info"
    help
      Set the default log level (debug, info, warn, error).

config MAX_CONNECTIONS
    int "Maximum concurrent connections"
    range 1 1000
    default 100
EOF
```

**Step 2: Generate configuration with defaults**

```bash
xconf saveconfig
# Creates .config, auto.conf, and autoconf.h
```

**Step 3: View your configuration**

```bash
cat .config
# Output:
# ENABLE_LOGGING=y
# LOG_LEVEL="info"
# MAX_CONNECTIONS=100
```

### Update Configuration When Kconfig Changes

```bash
# You added new options to Kconfig...
xconf oldconfig

# Output:
# 🆕 New configuration options detected:
#   + NEW_FEATURE
# 
# 💡 Use 'menuconfig' to configure new options (coming soon)
```

With auto-defaults:

```bash
xconf oldconfig --auto-defaults
# Automatically applies default values to new options
```

## 🔌 Integration with Rust Projects

### Method 1: Use in build.rs (Recommended)

```rust
// build.rs
use std::fs;

fn main() {
    // Read the generated configuration
    let config = fs::read_to_string(".config")
        .expect("Run 'xconf saveconfig' first");

    for line in config.lines() {
        let line = line.trim();

        // Skip comments and empty lines
        if line.starts_with('#') || line.is_empty() {
            continue;
        }

        // Parse "KEY=value" format
        if let Some((key, value)) = line.split_once('=') {
            let value = value.trim_matches('"');

            // Enable cfg attribute
            println!("cargo:rustc-cfg={}", key);

            // Also set as environment variable
            println!("cargo:rustc-env={}={}", key, value);
        }
    }

    // Rebuild if config changes
    println!("cargo:rerun-if-changed=.config");
}
```

### Method 2: Use auto.conf

The `auto.conf` file contains only enabled options, ideal for makefiles or scripts:

```bash
# auto.conf (generated)
ENABLE_LOGGING=y
LOG_LEVEL="info"
MAX_CONNECTIONS=100
```

### In Your Rust Code

```rust
// Use conditional compilation
#[cfg(ENABLE_LOGGING)]
fn log(msg: &str) {
    let level = env!("LOG_LEVEL");
    println!("[{}] {}", level, msg);
}

#[cfg(not(ENABLE_LOGGING))]
fn log(_msg: &str) {
    // No-op when logging is disabled
}

fn main() {
    log("Application started");

    #[cfg(ENABLE_LOGGING)]
    {
        let max_conn: usize = env!("MAX_CONNECTIONS").parse().unwrap();
        println!("Max connections: {}", max_conn);
    }
}
```

## 📚 Commands Reference

### `xconf saveconfig`

Generate configuration files with default values from Kconfig.

```bash
xconf saveconfig [OPTIONS]

Options:
  -o, --output <FILE>    Output .config path [default: .config]
  -k, --kconfig <FILE>   Kconfig file path [default: Kconfig]
  -s, --srctree <DIR>    Source tree root [default: .]
```

**Example:**

```bash
xconf saveconfig --output my.config --kconfig MyKconfig
```

### `xconf oldconfig`

Update existing configuration when Kconfig changes.

```bash
xconf oldconfig [OPTIONS]

Options:
  -c, --config <FILE>      Input .config file [default: .config]
  -k, --kconfig <FILE>     Kconfig file path [default: Kconfig]
  -s, --srctree <DIR>      Source tree root [default: .]
      --auto-defaults      Apply defaults to new options automatically
```

**What it does:**

- Detects new symbols added to Kconfig
- Detects removed symbols (no longer in Kconfig)
- Preserves existing values
- Shows summary of changes

**Example:**

```bash
# Interactive mode (prompts for new options)
xconf oldconfig

# Automatic mode (uses defaults for new options)
xconf oldconfig --auto-defaults
```

### `xconf generate`

Generate auto.conf and autoconf.h from existing .config.

```bash
xconf generate [OPTIONS]

Options:
  -c, --config <FILE>    Input .config file [default: .config]
  -k, --kconfig <FILE>   Kconfig file path [default: Kconfig]
  -s, --srctree <DIR>    Source tree root [default: .]
```

**Example:**

```bash
xconf generate --config production.config
```

### `xconf parse`

Parse and validate Kconfig syntax (debugging tool).

```bash
xconf parse [OPTIONS]

Options:
  -k, --kconfig <FILE>   Kconfig file path [default: Kconfig]
  -s, --srctree <DIR>    Source tree root [default: .]
```

**Example:**

```bash
xconf parse --kconfig Kconfig
```

### `xconf menuconfig`

Interactive terminal UI for configuration.

```bash
xconf menuconfig [OPTIONS]

Options:
  -k, --kconfig <FILE>   Kconfig file path [default: Kconfig]
  -s, --srctree <DIR>    Source tree root [default: .]
```

**Features:**

- Modern three-panel layout (menu tree, details, status bar)
- Intuitive navigation with arrow keys or vim-style (hjkl) keys
- Live search with fuzzy matching (press `/`)
- Visual indicators for enabled/disabled options
- Real-time value toggling with Space key
- Save/load with modification tracking
- Built-in help system (press `?`)

See [MENUCONFIG_GUIDE.md](MENUCONFIG_GUIDE.md) for detailed usage.

### `xconf defconfig` *(Coming Soon)*

Apply a defconfig file.

```bash
xconf defconfig <DEFCONFIG_FILE> [OPTIONS]

Options:
  -k, --kconfig <FILE>   Kconfig file path [default: Kconfig]
  -s, --srctree <DIR>    Source tree root [default: .]
```

## 📖 Configuration File Formats

### .config Format

The primary configuration file with all options:

```bash
#
# Automatically generated file; DO NOT EDIT.
# Rust Kbuild Configuration
#
ENABLE_LOGGING=y
LOG_LEVEL="info"
MAX_CONNECTIONS=100
# EXPERIMENTAL is not set
```

**Key differences from Linux Kconfig:**

- ✅ No `CONFIG_` prefix (cleaner for Rust)
- ✅ Backward compatible reader (accepts both formats)
- ✅ Clean, minimalist output

### auto.conf Format

Contains only enabled options (ideal for scripts):

```bash
ENABLE_LOGGING=y
LOG_LEVEL="info"
MAX_CONNECTIONS=100
```

### autoconf.h Format

C-style header for compatibility:

```c
#define ENABLE_LOGGING 1
#define LOG_LEVEL "info"
#define MAX_CONNECTIONS 100
```

## 🧪 Example Project

See `examples/sample_project` for a complete working example:

```
examples/sample_project/
├── Kconfig              # Main configuration
├── arch/
│   ├── x86/Kconfig      # x86 architecture options
│   └── arm/Kconfig      # ARM architecture options
└── kernel/Kconfig       # Kernel options
```

**Try it:**

```bash
cd examples/sample_project
xconf saveconfig
cat .config
```

## 📝 Supported Kconfig Syntax

### Configuration Types

- `bool` - Boolean (y/n)
- `tristate` - Three-state (y/m/n)
- `string` - String values
- `int` - Integer values
- `hex` - Hexadecimal values

### Directives

- `config` - Define a configuration option
- `menuconfig` - Config option with sub-menu
- `choice` / `endchoice` - Mutually exclusive options
- `menu` / `endmenu` - Grouping
- `source` - Include another Kconfig file
- `comment` - Display text
- `mainmenu` - Top-level menu title

### Dependencies

- `depends on` - Conditional visibility
- `select` - Force enable another option
- `imply` - Weak dependency
- `range` - Value constraints (for int/hex)
- `default` - Default value

### Expressions

- Logical: `&&`, `||`, `!`
- Comparison: `=`, `!=`, `<`, `<=`, `>`, `>=`
- Grouping: `( )`

### Example Kconfig

```
config NET_SUPPORT
    bool "Networking support"
    default y

if NET_SUPPORT

config NET_TCP
    bool "TCP protocol"
    default y
    select NET_IPV4

config NET_IPV4
    bool "IPv4 support"

config NET_PORT
    int "Default port"
    range 1 65535
    default 8080

endif # NET_SUPPORT
```

## 🐛 Debugging

### Enable Debug Logging

Set the `XCONFIG_DEBUG` environment variable to enable detailed debug logging for menu tree operations:

```bash
XCONFIG_DEBUG=1 xconf menuconfig
```

**Debug output includes:**

- `process_entries` calls with parent_id, depth, and entry count
- Menu tree insertions with all items being inserted
- Key overwrite warnings (if any duplicate keys are detected)
- `get_items_for_path` lookups showing which items are returned for each navigation

**Example debug output:**

```
🔹 process_entries: parent_id='menu_Platform Selection', depth=1, entries_count=5
🔹 Inserting into menu_tree: key='menu_Platform Selection', items_count=12
    [0] id='choice_PLATFORM_KPLAT_AARCH64', label='AArch64 Platform', kind="Choice"
    [1] id='PLATFORM_KPLAT_AARCH64', label='QEMU ARM64 Virtual Machine', kind="Config"
    ...
🔍 get_items_for_path: key='menu_Platform Selection', path=["menu_Platform Selection"]
📋 Returning 12 items for key 'menu_Platform Selection'
```

**Use cases:**

- Debugging menu navigation issues
- Verifying correct menu tree structure
- Identifying configuration parsing problems
- Troubleshooting menu content misalignment

## 🔧 Development

### Building

```bash
cargo build --release
```

### Running Tests

```bash
cargo test
```

### Benchmarks

```bash
cargo bench
```

### Documentation

```bash
cargo doc --open
```

## 🗺️ Roadmap

### ✅ Implemented

- Complete Kconfig lexer and parser
- Full syntax support (bool, tristate, string, int, hex)
- Source directive recursion with cycle detection
- Expression evaluation
- Configuration file I/O (without CONFIG_ prefix)
- Backward compatible reader
- Configuration generators (auto.conf, autoconf.h)
- Oldconfig with change detection
- Saveconfig command
- **Interactive menuconfig TUI** ✨
- Command-line interface
- Comprehensive test suite

### 🚧 In Progress

- Defconfig support

### 📋 Planned

- Dependency resolution and validation
- Export to JSON/YAML
- VS Code extension
- Language server protocol (LSP) support

## 📚 Additional Documentation

- [Usage Guide](docs/USAGE.md) - Detailed usage instructions
- [Design Document](docs/DESIGN.md) - Architecture and design decisions
- [API Documentation](docs/API.md) - Complete API reference

## 🤝 Contributing

Contributions are welcome! Please feel free to submit a Pull Request.

## 📄 License

Licensed under the [Apache License, Version 2.0](LICENSE)

## 🙏 Acknowledgments

This project is inspired by:

- [kbuild-standalone](https://github.com/WangNan0/kbuild-standalone) - Original C implementation
- [Linux Kconfig](https://www.kernel.org/doc/html/latest/kbuild/kconfig-language.html) - Official Kconfig documentation

## 📧 Contact

- GitHub Issues: [guoweikang/rust-kbuild/issues](https://github.com/guoweikang/rust-kbuild/issues)
- Repository: [guoweikang/rust-kbuild](https://github.com/guoweikang/rust-kbuild)
