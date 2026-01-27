# kspin

[![Crates.io](https://img.shields.io/crates/v/kspin)](https://crates.io/crates/kspin)
[![Docs.rs](https://docs.rs/kspin/badge.svg)](https://docs.rs/kspin)
[![CI](https://github.com/arceos-org/kspin/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/arceos-org/kspin/actions/workflows/ci.yml)

Kernel-space spinlocks with configurable critical section guards.

## Features

- 🔒 **Type-safe spinlocks** with compile-time guard selection
- 🚀 **Zero-cost abstractions** - guards optimized away when not needed
- 🎯 **Single-core optimization** - lock state removed without `smp` feature
- 📦 **No-std compatible** - works in bare-metal environments
- 🛡️ **RAII guards** - automatic lock release and state restoration

## Cargo Features

- `smp`: Multi-core support with atomic lock state (default: off)
- `preempt`: Preemption control support (default: off)

## Quick Start

```rust
use kspin::SpinNoIrq;

// Create a lock that disables IRQs and preemption
static COUNTER: SpinNoIrq<u32> = SpinNoIrq::new(0);

fn increment() {
    let mut count = COUNTER.lock();
    *count += 1;
    // Lock automatically released when guard is dropped
}
```

## Lock Types

kspin provides three main lock types with different safety guarantees:

### `SpinRaw<T>`
Raw spinlock with no guards. **Fastest but least safe** - must only be used in contexts where preemption and IRQs are already disabled.

```rust
use kspin::SpinRaw;

let lock = SpinRaw::new(42);
let guard = lock.lock();
assert_eq!(*guard, 42);
```

### `SpinNoPreempt<T>`
Disables preemption while holding the lock. Suitable for use in IRQ-disabled contexts or when IRQ handlers don't access the same data.

```rust
use kspin::SpinNoPreempt;

let lock = SpinNoPreempt::new(vec![1, 2, 3]);
let mut guard = lock.lock();
guard.push(4);
// Preemption is disabled here
```

### `SpinNoIrq<T>`
Disables both IRQs and preemption. **Safest option** - can be used from any context including interrupt handlers.

```rust
use kspin::SpinNoIrq;

let lock = SpinNoIrq::new(());
let guard = lock.lock();
// Both preemption and IRQs are disabled here
```

## Architecture

The crate is organized into three main components:

### Guards (`guard` module)
RAII guards that manage critical sections:
- `NoOp`: No protection (for IRQ-disabled contexts)
- `NoPreempt`: Disables kernel preemption
- `IrqSave`: Saves/restores IRQ state
- `NoPreemptIrqSave`: Disables both preemption and IRQs

### Locks (`lock` module)
Generic spinlock implementation `SpinLock<G, T>` parameterized by guard type.

### Type Aliases
Convenient aliases for common lock types:
- `SpinRaw<T>` = `SpinLock<NoOp, T>`
- `SpinNoPreempt<T>` = `SpinLock<NoPreempt, T>`
- `SpinNoIrq<T>` = `SpinLock<NoPreemptIrqSave, T>`

## Usage Patterns

### Static Locks
```rust
use kspin::SpinNoIrq;

static SHARED_DATA: SpinNoIrq<Vec<u8>> = SpinNoIrq::new(Vec::new());

fn add_data(value: u8) {
    SHARED_DATA.lock().push(value);
}
```

### Interrupt Handlers
```rust
use kspin::SpinNoIrq;

static DATA: SpinNoIrq<Option<u32>> = SpinNoIrq::new(None);

fn irq_handler() {
    // Safe to use in IRQ context
    *DATA.lock() = Some(42);
}
```

### Implementing KernelGuardIf
To use the `preempt` feature, implement the `KernelGuardIf` trait:

```rust
use kspin::KernelGuardIf;

struct MyKernelGuard;

#[crate_interface::impl_interface]
impl KernelGuardIf for MyKernelGuard {
    fn enable_preempt() {
        // Your implementation
    }

    fn disable_preempt() {
        // Your implementation
    }

    fn local_irq_save_and_disable() -> usize {
        0  // Your implementation
    }

    fn local_irq_restore(flags: usize) {
        // Your implementation
    }
}
```

## Performance

- **Single-core**: Lock state is completely optimized away
- **Multi-core**: Uses efficient atomic operations
- **No-allocation**: All operations are stack-based
- **Inline-friendly**: Hot paths marked with `#[inline]`

## Safety

- Type-safe API prevents misuse
- RAII ensures locks are always released
- Guard state automatically restored on drop
- Panic-safe: locks released during unwinding

## License

See [LICENSE](LICENSE) in the repository root.

