# Backtrace - Stack Unwinding for x-kernel

Stack unwinding and symbolication library for bare-metal and kernel environments.

## Features

- 🏗️ **Multi-architecture** - x86_64, aarch64, riscv32/64, loongarch64
- 🔍 **DWARF symbolication** - Function names, file paths, line numbers
- ⚙️ **Configurable** - Depth limits, memory ranges, filtering
- 🛡️ **Safe** - Comprehensive validation and error handling

## Quick Start

```rust
use backtrace::{init, Backtrace};

// 1. Initialize (once, at startup)
init(
    0x8000_0000..0x9000_0000,  // code range
    0x7000_0000..0x8000_0000,  // stack range
);

// 2. Capture backtrace
let bt = Backtrace::capture();

// 3. Display
println!("{}", bt);
```

## Architecture Support

| Architecture | Status | Frame Offset | Alignment | Notes |
|--------------|--------|--------------|-----------|-------|
| x86_64       | ✅     | 0            | 16 bytes  | Fully tested |
| aarch64      | ✅     | 0            | 16 bytes  | Fully tested |
| riscv32      | ✅     | 1            | 8 bytes   | Tested |
| riscv64      | ✅     | 1            | 8 bytes   | Fully tested |
| loongarch64  | ⚠️     | 1            | 8 bytes   | Limited testing |

## Examples

### Capture from Exception Handler

```rust,ignore
fn exception_handler(trap_frame: &TrapFrame) {
    let bt = Backtrace::capture_trap(
        trap_frame.fp,
        trap_frame.pc,
        trap_frame.ra,
    );

    eprintln!("Exception occurred!");
    eprintln!("{}", bt);
}
```

### Configure Maximum Depth

```rust,no_run
use backtrace::set_max_depth;

// Limit unwinding to 20 frames
set_max_depth(20);
```

### Iterate Frames with Symbolication

```rust,ignore
if let Some(frames) = bt.frames() {
    for (i, (raw, frame)) in frames.enumerate() {
        println!("{:>4}: {:?} at {}:{}",
            i,
            frame.function,
            frame.file.unwrap_or("??"),
            frame.line.unwrap_or(0),
        );
    }
}
```

## Safety Considerations

⚠️ **Important**: Always call `init()` before capturing backtraces!

- Ensure IP and FP ranges cover valid memory
- Be aware of performance impact in hot paths
- Stack unwinding can be expensive (allocates Vec)

## License

See LICENSE file in the repository root.
