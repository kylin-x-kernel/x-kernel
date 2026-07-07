# ksync

Kernel synchronization primitives for x-kernel.

## Features

- **Mutex**: Mutual exclusion lock with configurable spinning
- **RwLock**: Reader-writer lock allowing multiple readers or one writer
- **Semaphore**: Counting semaphore for resource management
- **Integration**: Works seamlessly with `kspin` for spinlocks

## Usage

### Mutex

Basic mutex usage:

```rust
use ksync::Mutex;

static DATA: Mutex<Vec<u8>> = Mutex::new(Vec::new());

fn task() {
    let mut data = DATA.lock();
    data.push(42);
}
```

Mutex with custom spin configuration:

```rust
use ksync::{Mutex, SpinConfig};

static DATA: Mutex<u32> = Mutex::const_new(
    ksync::RawMutex::with_config(SpinConfig {
        max_spins: 20,
        spin_before_yield: 5,
    }),
    0
);
```

### RwLock

Reader-writer lock allows multiple concurrent readers but only one writer:

```rust
use ksync::RwLock;

static CONFIG: RwLock<u32> = RwLock::new(0);

fn reader() {
    let config = CONFIG.read();
    // multiple readers can access simultaneously
}

fn writer() {
    let mut config = CONFIG.write();
    // exclusive writer access
}
```

### Semaphore

Counting semaphore for resource management:

```rust
use ksync::Semaphore;

static SEM: Semaphore = Semaphore::new(3);

fn task() {
    let _guard = SEM.acquire_guard();
    // do work with permit
    // permit automatically released when guard is dropped
}
```

Manual acquire/release:

```rust
SEM.acquire();
// do work
SEM.release();
```

## Features

### `stats` (Kconfig `KFEAT_LOCK_STAT`)

Enable lock contention statistics through `klockstat`:

```toml
ksync = { version = "0.1", features = ["stats"] }
```

Runtime locks created with `Mutex::new` / `RwLock::new` share one lock class per
init site (`file:line`). Static locks register through `static_lock!`:

```rust
use ksync::{Mutex, static_lock};

static_lock! {
    static TASK_TABLE: Mutex<TaskTable> = Mutex::new(TaskTable::new());
}
```

Read aggregated statistics from `/proc/lock_stat` or through
`klockstat::dump_lock_stat()`.

### `watchdog`

Enable watchdog support for deadlock detection (requires `axhal`).

## Architecture

- **Mutex**: Adaptive spinning followed by blocking on `event_listener::Event`
- **RwLock**: Atomic state management with separate events for readers/writers
- **Semaphore**: Atomic counter with blocking on `event_listener::Event`

All primitives use the `lock_api` trait for consistent API surface.
