# Rust Kbuild - API Documentation

## Module: `kconfig`

Core Kconfig parsing functionality.

### `Parser`

The main parser for Kconfig files.

#### Constructor

```rust
pub fn new(kconfig_path: impl AsRef<Path>, srctree: impl AsRef<Path>) -> Result<Self>
```

Creates a new parser for the given Kconfig file.

**Parameters:**
- `kconfig_path`: Path to the main Kconfig file
- `srctree`: Root directory for source tree (used to resolve relative paths)

**Returns:**
- `Result<Parser>`: Parser instance or error

**Example:**
```rust
use xconfig::kconfig::Parser;
use std::path::PathBuf;

let parser = Parser::new("Kconfig", ".")?;
```

#### Methods

##### `parse`

```rust
pub fn parse(&mut self) -> Result<KconfigFile>
```

Parses the Kconfig file and returns the AST.

**Returns:**
- `Result<KconfigFile>`: Parsed AST or error

**Example:**
```rust
let mut parser = Parser::new("Kconfig", ".")?;
let ast = parser.parse()?;
println!("Parsed {} entries", ast.entries.len());
```

### `Lexer`

Tokenizer for Kconfig files.

#### Constructor

```rust
pub fn new(input: String, file: PathBuf) -> Self
```

Creates a new lexer for the given input.

**Parameters:**
- `input`: Source code to tokenize
- `file`: File path for error messages

#### Methods

##### `next_token`

```rust
pub fn next_token(&mut self) -> Result<Token>
```

Returns the next token from the input.

##### `peek_token`

```rust
pub fn peek_token(&mut self) -> Result<Token>
```

Peeks at the next token without consuming it.

##### `skip_help_text`

```rust
pub fn skip_help_text(&mut self) -> String
```

Skips indented help text and returns it as a string.

### AST Types

#### `Entry`

```rust
pub enum Entry {
    Config(Config),
    MenuConfig(MenuConfig),
    Choice(Choice),
    Menu(Menu),
    If(If),
    Source(Source),
    Comment(Comment),
    MainMenu(String),
}
```

Represents a top-level entry in a Kconfig file.

#### `Config`

```rust
pub struct Config {
    pub name: String,
    pub symbol_type: SymbolType,
    pub properties: Property,
}
```

Configuration option.

**Fields:**
- `name`: Symbol name (e.g., "CONFIG_X86")
- `symbol_type`: Type of the option (Bool, Tristate, String, Int, Hex)
- `properties`: Option properties (prompt, default, dependencies, etc.)

#### `Property`

```rust
pub struct Property {
    pub prompt: Option<String>,
    pub default: Option<Expr>,
    pub depends: Option<Expr>,
    pub select: Vec<(String, Option<Expr>)>,
    pub imply: Vec<(String, Option<Expr>)>,
    pub range: Option<(Expr, Expr, Option<Expr>)>,
    pub help: Option<String>,
}
```

Properties of a configuration option.

#### `SymbolType`

```rust
pub enum SymbolType {
    Bool,
    Tristate,
    String,
    Int,
    Hex,
}
```

Type of a configuration symbol.

#### `Expr`

```rust
pub enum Expr {
    Symbol(String),
    Const(String),
    Not(Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Equal(Box<Expr>, Box<Expr>),
    NotEqual(Box<Expr>, Box<Expr>),
    Less(Box<Expr>, Box<Expr>),
    LessEqual(Box<Expr>, Box<Expr>),
    Greater(Box<Expr>, Box<Expr>),
    GreaterEqual(Box<Expr>, Box<Expr>),
}
```

Boolean/comparison expression.

### `SymbolTable`

Manages configuration symbols and their values.

#### Constructor

```rust
pub fn new() -> Self
```

Creates an empty symbol table.

#### Methods

##### `add_symbol`

```rust
pub fn add_symbol(&mut self, name: String, symbol_type: SymbolType)
```

Adds a new symbol to the table.

##### `set_value`

```rust
pub fn set_value(&mut self, name: &str, value: String)
```

Sets the value of a symbol.

##### `get_value`

```rust
pub fn get_value(&self, name: &str) -> Option<String>
```

Gets the value of a symbol.

##### `is_enabled`

```rust
pub fn is_enabled(&self, name: &str) -> bool
```

Checks if a symbol is enabled (value is "y" or "m").

##### `all_symbols`

```rust
pub fn all_symbols(&self) -> impl Iterator<Item = (&String, &Symbol)>
```

Returns an iterator over all symbols.

**Example:**
```rust
use xconfig::kconfig::{SymbolTable, SymbolType};

let mut symbols = SymbolTable::new();
symbols.add_symbol("CONFIG_X86".to_string(), SymbolType::Bool);
symbols.set_value("CONFIG_X86", "y".to_string());

assert!(symbols.is_enabled("CONFIG_X86"));
```

## Module: `config`

Configuration file I/O.

### `ConfigReader`

Reads .config files.

#### Methods

##### `read`

```rust
pub fn read(path: impl AsRef<Path>) -> Result<HashMap<String, String>>
```

Reads a .config file and returns a map of symbol names to values.

**Example:**
```rust
use xconfig::config::ConfigReader;

let config = ConfigReader::read(".config")?;
println!("CONFIG_X86 = {:?}", config.get("CONFIG_X86"));
```

### `ConfigWriter`

Writes .config files.

#### Methods

##### `write`

```rust
pub fn write(path: impl AsRef<Path>, symbols: &SymbolTable) -> Result<()>
```

Writes symbols to a .config file.

**Example:**
```rust
use xconfig::config::ConfigWriter;
use xconfig::kconfig::{SymbolTable, SymbolType};

let mut symbols = SymbolTable::new();
symbols.add_symbol("CONFIG_X86".to_string(), SymbolType::Bool);
symbols.set_value("CONFIG_X86", "y".to_string());

ConfigWriter::write(".config", &symbols)?;
```

### `ConfigGenerator`

Generates configuration output files.

#### Methods

##### `generate_auto_conf`

```rust
pub fn generate_auto_conf(path: impl AsRef<Path>, symbols: &SymbolTable) -> Result<()>
```

Generates an auto.conf file for makefiles.

**Output Format:**
```
CONFIG_X86=y
CONFIG_VERSION="1.0.0"
```

##### `generate_autoconf_h`

```rust
pub fn generate_autoconf_h(path: impl AsRef<Path>, symbols: &SymbolTable) -> Result<()>
```

Generates an autoconf.h header file for C code.

**Output Format:**
```c
#define CONFIG_X86 1
#define CONFIG_VERSION "1.0.0"
```

**Example:**
```rust
use xconfig::config::ConfigGenerator;

ConfigGenerator::generate_auto_conf("auto.conf", &symbols)?;
ConfigGenerator::generate_autoconf_h("autoconf.h", &symbols)?;
```

## Module: `error`

Error types.

### `KconfigError`

```rust
pub enum KconfigError {
    Io(std::io::Error),
    Syntax { file: PathBuf, line: usize, message: String },
    CircularDependency { chain: String },
    FileNotFound(PathBuf),
    UndefinedSymbol(String),
    TypeMismatch { expected: String, actual: String },
    InvalidExpression(String),
    Parse(String),
    Config(String),
    RecursiveSource { chain: String },
}
```

Error type for Kconfig operations.

**Variants:**
- `Io`: I/O error
- `Syntax`: Syntax error with location
- `CircularDependency`: Circular dependency in symbol definitions
- `FileNotFound`: Referenced file not found
- `UndefinedSymbol`: Reference to undefined symbol
- `TypeMismatch`: Type mismatch in expression
- `InvalidExpression`: Invalid expression
- `Parse`: Parse error
- `Config`: Configuration error
- `RecursiveSource`: Circular source inclusion

## CLI Module

### Running Commands

The CLI is accessed through the `xconf` binary:

```bash
xconf <command> [options]
```

Available commands:
- `parse`: Parse and display Kconfig AST
- `defconfig`: Apply defconfig (not yet implemented)
- `menuconfig`: Interactive TUI (not yet implemented)
- `generate`: Generate configuration files

See [USAGE.md](USAGE.md) for detailed CLI documentation.

## Examples

### Complete Example

```rust
use xconfig::kconfig::{Parser, SymbolTable, SymbolType};
use xconfig::config::{ConfigWriter, ConfigGenerator};
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Parse Kconfig
    let mut parser = Parser::new("Kconfig", ".")?;
    let ast = parser.parse()?;
    
    // Build symbol table from AST
    let mut symbols = SymbolTable::new();
    
    // Process config entries (simplified)
    for entry in &ast.entries {
        match entry {
            xconfig::kconfig::Entry::Config(config) => {
                symbols.add_symbol(
                    config.name.clone(),
                    config.symbol_type.clone()
                );
                
                // Set default value if present
                if let Some(default) = &config.properties.default {
                    // Evaluate default expression
                    // symbols.set_value(&config.name, evaluated_value);
                }
            }
            _ => {}
        }
    }
    
    // Write configuration
    ConfigWriter::write(".config", &symbols)?;
    ConfigGenerator::generate_auto_conf("auto.conf", &symbols)?;
    ConfigGenerator::generate_autoconf_h("autoconf.h", &symbols)?;
    
    Ok(())
}
```

## Thread Safety

All types in this library are `Send` but not `Sync`. Parser instances should not be shared across threads without synchronization.

## Performance

- **Lexer**: O(n) where n is input size
- **Parser**: O(n) with small constant factor
- **Symbol table**: O(1) lookups via HashMap
- **Memory**: Proportional to AST size (typically < 10MB for large projects)

## Error Handling

All fallible operations return `Result<T, KconfigError>`. Use the `?` operator for error propagation:

```rust
fn parse_config() -> Result<(), KconfigError> {
    let mut parser = Parser::new("Kconfig", ".")?;
    let ast = parser.parse()?;
    Ok(())
}
```
