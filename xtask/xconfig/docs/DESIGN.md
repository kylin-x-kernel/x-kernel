# Rust Kbuild - Design Document

## Overview

Rust Kbuild is a complete reimplementation of the kbuild-standalone configuration system in Rust, providing Kconfig parsing, configuration management, and build system support.

## Architecture

### Module Structure

```
rust-kbuild/
├── src/
│   ├── kconfig/      # Kconfig parsing
│   ├── config/       # Configuration management
│   ├── cli/          # Command-line interface
│   └── error.rs      # Error types
```

### Core Components

#### 1. Lexer (kconfig/lexer.rs)

The lexer tokenizes Kconfig files into a stream of tokens.

**Key Features:**
- Handles keywords (config, menu, choice, etc.)
- Operators (=, !=, <, <=, >, >=, &&, ||, !)
- String literals with escape sequences
- Numeric literals (decimal and hexadecimal)
- Comments (line and block)
- Special handling for help text (raw, non-tokenized)

**Token Types:**
- Keywords: `config`, `menuconfig`, `choice`, `menu`, `source`, etc.
- Operators: Comparison and logical operators
- Literals: Identifiers, strings, numbers
- Structural: Newlines, parentheses

#### 2. Parser (kconfig/parser.rs)

The parser builds an Abstract Syntax Tree (AST) from the token stream.

**Key Features:**
- Recursive descent parser
- Source directive handling with circular dependency detection
- File stack for nested includes
- Expression parsing with operator precedence
- Context-aware parsing (menu, choice, if blocks)

**Source Recursion:**
The parser implements a file stack to handle nested source directives:

```rust
pub struct Parser {
    current_file: PathBuf,
    srctree: PathBuf,
    file_stack: Vec<FileContext>,      // Stack of open files
    parsed_files: HashSet<PathBuf>,    // Already parsed files
    inclusion_chain: Vec<PathBuf>,     // Current inclusion chain
}
```

When a source directive is encountered:
1. Resolve path relative to source tree
2. Check for circular dependencies
3. Check if already parsed
4. Push new file context onto stack
5. Parse the file
6. Pop file context

#### 3. AST (kconfig/ast.rs)

Defines the Abstract Syntax Tree structure.

**Node Types:**
- `Config`: Configuration option
- `MenuConfig`: Menu configuration option
- `Choice`: Choice group
- `Menu`: Menu container
- `If`: Conditional block
- `Source`: File inclusion
- `Comment`: User comment
- `MainMenu`: Main menu title

**Expression Types:**
- Symbol references
- Constants
- Logical operators (AND, OR, NOT)
- Comparison operators (=, !=, <, <=, >, >=)

#### 4. Expression Evaluator (kconfig/expr.rs)

Evaluates boolean expressions in depends/select/if conditions.

**Features:**
- Symbol lookup from symbol table
- Type-aware comparisons
- Short-circuit evaluation for AND/OR

#### 5. Symbol Table (kconfig/symbol.rs)

Manages configuration symbols and their values.

**Features:**
- Symbol registration
- Value storage (bool, tristate, string, int, hex)
- Enabled state checking
- Value retrieval

#### 6. Configuration I/O (config/)

##### Reader (config/reader.rs)
Parses .config files:
```
CONFIG_OPTION=y
# CONFIG_DISABLED is not set
CONFIG_STRING="value"
```

##### Writer (config/writer.rs)
Writes .config files with proper formatting.

##### Generator (config/generator.rs)
Generates:
- `auto.conf`: Makefile-compatible configuration
- `autoconf.h`: C header with preprocessor macros

## Design Decisions

### 1. Error Handling

Uses `thiserror` for custom error types with context:
```rust
pub enum KconfigError {
    Syntax { file, line, message },
    RecursiveSource { chain },
    FileNotFound(PathBuf),
    // ...
}
```

### 2. Parsing Strategy

**Recursive Descent Parser:**
- Easy to understand and maintain
- Natural mapping to grammar rules
- Good error messages with line numbers

**Advantages:**
- Clear code structure
- Easy to extend
- Predictable performance

**Trade-offs:**
- Not the most efficient parser
- Some grammar ambiguities resolved by lookahead

### 3. Source Recursion

**Circular Dependency Detection:**
Maintains an inclusion chain to detect cycles:
```rust
inclusion_chain: Vec<PathBuf>
```

Before including a file, checks if it's already in the chain.

**Already-Parsed Tracking:**
```rust
parsed_files: HashSet<PathBuf>
```

Prevents re-parsing the same file multiple times (but not circular - that's an error).

### 4. Help Text Handling

Help text is special - it's not tokenized like regular content:
- After `help` keyword, lexer switches to raw text mode
- Collects all indented lines
- Returns to normal tokenization at first non-indented line

This avoids tokenization errors on arbitrary help text content.

### 5. Memory Management

**File Stack:**
Each included file gets its own lexer and token state. The stack grows with include depth but is typically shallow (< 10 levels).

**Parsed Files Set:**
Uses a HashSet to track parsed files - O(1) lookup, minimal memory overhead.

## Performance Considerations

### Lexer Performance
- Single-pass tokenization
- Minimal backtracking (only for peek operations)
- Efficient string operations

### Parser Performance
- Single-pass parsing
- Minimal allocations
- Stack-based file context management

### Memory Usage
- AST nodes allocated on demand
- Symbol table uses HashMap for O(1) lookups
- File contents loaded entirely into memory (acceptable for Kconfig files which are typically small)

## Future Enhancements

### Planned Features
1. **Full menuconfig TUI**: Interactive terminal UI for configuration
2. **Defconfig support**: Apply and save defconfig files
3. **Constraint validation**: Check select/imply constraints
4. **Dependency resolution**: Auto-enable dependencies
5. **Config merging**: Merge multiple configuration fragments
6. **Export formats**: JSON, YAML export of configuration

### Performance Optimizations
1. **Lazy parsing**: Only parse files when needed
2. **Caching**: Cache parsed ASTs for repeated builds
3. **Parallel parsing**: Parse independent files in parallel

### Language Features
1. **Full expression support**: All expression operators
2. **Advanced select/imply**: Complex select conditions
3. **Range validation**: Validate int/hex ranges
4. **Option attributes**: allnoconfig_y, etc.

## Testing Strategy

### Unit Tests
- Lexer: Token generation
- Parser: AST construction
- Expression: Evaluation logic

### Integration Tests
- Source recursion
- Circular dependency detection
- Configuration I/O
- Complete parsing of real Kconfig files

### Test Fixtures
Realistic Kconfig files covering:
- Basic options
- Source directives
- Choices and menus
- Dependencies and selections

## References

- **kbuild-standalone**: https://github.com/WangNan0/kbuild-standalone
- **Linux Kconfig**: Linux kernel Kconfig documentation
- **Kconfig Language**: Kconfig language specification
