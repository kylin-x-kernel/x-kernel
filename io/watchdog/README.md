# watchdog

Soft and hard lockup detection for system monitoring.

## Overview

watchdog combines timer-driven soft-lockup checks with optional NMI-based
hard-lockup detection. The soft-lockup heartbeat is a per-CPU watchdog task:
it is pinned to the owning CPU, runs with elevated scheduler priority, and
periodically updates the local CPU timestamp. Timer interrupt callbacks check
whether that timestamp is stale.

The NMI watchdog path also runs registered health checks, including the kwork
system workqueue backlog check. That check is implemented by kwork and only
registered through watchdog, so watchdog's own forward progress does not depend
on system workqueue execution.

## Usage

### Initialization

```rust
use watchdog::{init_primary, init_secondary};

// Initialize on primary core
init_primary();

// Initialize on secondary cores (call when each secondary core boots)
init_secondary();
```

## Hardware Requirements

### PMU NMI Source
- ARMv8-A architecture (AArch64)
- PMUv3-compatible processor
- Performance Monitoring Unit support

### SDEI NMI Source (Planned)
- ARM SDEI compatible firmware/hypervisor

## Notes

- Currently supports only AArch64 architecture
- PMU support requires `pmu` feature enabled
- Interrupt priority set to highest (0)
- Requires initialization on each core in multi-core systems

## Development Status

- ✅ PMU NMI Source: Implemented
- 🔄 SDEI NMI Source: In Development
- ✅ Multi-core Support: Implemented
