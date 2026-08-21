# Backtrace - Stack Unwinding for x-kernel

Frame-pointer based stack unwinding for bare-metal and kernel environments.

## Design

Stack unwinding and symbolication are **decoupled**:

| Layer | Where | Always on? |
|-------|-------|-----------|
| Unwinding (raw addresses) | Kernel (`Backtrace::capture`) | **Yes** — no debug data needed |
| In-kernel DWARF symbolication | Kernel (`dwarf` feature / KFEAT_DWARF) | Optional, off by default |
| Compact symbol table (`func+0xoff/0xsize`) | Kernel (`symtab` feature / KFEAT_SYMTAB) | Optional, off by default |
| Full file/line symbolication | Host (`xkmake symbolize` / `make symbolize`) | Offline tool |

The kernel prints a stable raw format that the host resolves against the
unstripped `kernel.debug.elf`:

```text
Backtrace:
0: 0xffff8000004a1234
1: 0xffff8000003f2abc
```

```bash
# host-side resolution
make symbolize LOG=panic.log
```

## Features

- 🏗️ **Multi-architecture** - x86_64, aarch64, riscv32/64, loongarch64
- ⚡ **Always-on frame-pointer unwinding** - raw addresses without DWARF
- 🔍 **Optional DWARF symbolication** - in-kernel gimli/addr2line (`dwarf`)
- 🏷️ **Optional compact symbol table** - kallsyms-style annotations (`symtab`)
- ⚙️ **Configurable** - depth limits, memory ranges, validation

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

// 3. Display (raw addresses, optionally annotated)
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

## Output Formats

Default (no symbolication features):

```text
Backtrace:
0: 0xffff8000004a1234
1: 0xffff8000003f2abc
```

With `symtab` (KFEAT_SYMTAB):

```text
Backtrace:
0: 0xffff8000004a1234  panic+0x2f9/0x330
1: 0xffff8000003f2abc  ksyscall_entry+0x1a/0x2b
```

With `dwarf` (KFEAT_DWARF): full function names, files and line numbers,
resolved in kernel (legacy mode).

The raw address is always the first token of each frame line, so the host
parser works with every format.

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

## Safety Considerations

⚠️ **Important**: Always call `init()` before capturing backtraces!

- Ensure IP and FP ranges cover valid memory
- Stack unwinding allocates a `Vec` bounded by the configured max depth
- Raw unwinding is designed for panic/NMI/lockup paths: no DWARF parsing,
  no demangling, no allocation beyond the frame vector

## Testing

### Running Tests

```bash
cargo test --all-features --manifest-path util/backtrace/Cargo.toml
```

Note: `util/backtrace` is not a workspace member; run tests through the
kernel build (`make build`) or the unittest image (`make UNITTEST=y run`)
for kernel-context coverage.

### What Is Tested

✅ **Tested in `cargo test`:**
- Stack frame capture (always-on unwinding)
- Frame pointer validation
- Configuration management (max depth, ranges)
- Display formatting (raw addresses)

❌ **Not tested in `cargo test`:**
- DWARF symbolication (requires kernel linker sections; covered by the
  kernel unittest image with KFEAT_DWARF)
- Symbol table blob parsing (`symtab` feature; covered by kernel
  `#[cfg(unittest)]` tests, run via `make UNITTEST=y run`)
- Kernel symbol table embedding (requires the `xkmake` bootstrap)

## Host-side Symbolication Workflow

1. Build the kernel (KFEAT_DWARF=n by default):

   ```bash
   make defconfig && make build
   ```

2. Run and capture a panic/exception log:

   ```bash
   make run > panic.log
   ```

3. Resolve addresses against `kernel.debug.elf`:

   ```bash
   make symbolize LOG=panic.log
   # or: xkmake symbolize --elf kernel.debug.elf --log panic.log
   ```

`kernel.debug.elf` is always produced by the build (unstripped, with DWARF)
and is the symbolication input; the boot image never contains DWARF in the
default configuration.

`make run` / `make justrun` also symbolicate automatically after QEMU
exits: the guest output is mirrored to `bundle/qemu.log` (while staying
live on the terminal), and any backtrace frames in it are resolved against
`kernel.debug.elf` right away. Disable with `--no-symbolize`.

## License

See LICENSE file in the repository root.
