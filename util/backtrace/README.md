# Backtrace - Stack Unwinding for x-kernel

Stack unwinding and symbolication library for bare-metal and kernel environments.

## Features

- 🏗️ **Multi-architecture** - x86_64, aarch64, riscv32/64, loongarch64
- 🔍 **DWARF symbolication** - Function names, file paths, line numbers
- ⚙️ **Configurable** - Depth limits, memory ranges, filtering
- 🛡️ **Safe** - Comprehensive validation and error handling

## Quick Start

```rust,no_run
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

## Testing

### Running Tests

```bash
cd util/backtrace
cargo test --all-features
```

### Test Environment Limitations

**Important**: DWARF symbolication is automatically disabled during tests because:

1. Debug symbols (`__start_debug_*`, `__stop_debug_*`) are defined by the kernel linker script
2. These symbols are not available in user-space test environments
3. Attempting to reference them causes linker errors

### What Is Tested

✅ **Tested in `cargo test`:**
- Stack frame capture
- Frame pointer validation
- Configuration management (max depth, ranges)
- API surface and error handling
- Display formatting (raw frames)

❌ **Not tested in `cargo test`:**
- DWARF symbolication (function names, file paths, line numbers)
- Architecture-specific symbol resolution in kernel context

### Testing DWARF Symbolication

To test full symbolication support, run in the actual kernel:

```bash
# Build x-kernel with backtrace support
cd /path/to/x-kernel
make A=apps/exception ARCH=aarch64 qemu

# Trigger an exception to see backtrace with symbols
```

### Test Output Example

When running `cargo test --all-features`, you'll see:

```text
running 11 tests
test test_backtrace_display ... ok
test test_capture_trap ... ok
test test_frame_adjusted_ip ... ok
test test_frame_count ... ok
test test_frame_creation ... ok
test test_frame_display ... ok
test test_initialization ... ok
test test_invalid_frame ... ok
test test_max_depth_configuration ... ok
test test_raw_frames_access ... ok
test test_recursive_capture ... ok

test result: ok. 11 passed; 0 failed; 0 ignored
```

Backtrace display in tests will show:
```text
Symbolication not available in test mode.
Raw frames:
   0: fp=0x00007ffd12340000, ip=0x0000555555555000
   1: fp=0x00007ffd12341000, ip=0x0000555555556000
   ...
```

## License

See LICENSE file in the repository root.
