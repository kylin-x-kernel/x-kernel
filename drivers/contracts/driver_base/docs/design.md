# driver_base — Design

## Purpose

This module provides the common base interfaces for all x-kernel device
drivers: the device-category enum `DeviceKind`, the driver error type
`DriverError` / `DriverResult`, and the `Device` trait every device must
implement. It is the shared dependency of the driver sub-crates under
`drivers/` (block, net, display, input, vsock, virtio, kdriver) and gives
the kernel driver framework one unified type contract.

## Background

x-kernel runs in a bare-metal `no_std` environment and cannot use standard
library error types such as `std::io::Error`. At the same time the kernel
must manage heterogeneous devices (block, network, display, ...) under one
framework, so a lightweight, dependency-free common interface layer is
needed to keep driver crates consistent in error handling and device
identity.

## Scope

```text
drivers/contracts/driver_base/
├── src/
│   └── lib.rs
└── Cargo.toml
```

## Architecture

```text
┌──────────────────────────────────────────────────────────────┐
│                        driver_base                           │
│                                                              │
│  DeviceKind ──classification──> Device                       │
│       │                │                                     │
│       │                ├── name()                            │
│       │                ├── device_kind()                     │
│       │                └── irq()                             │
│       │                                                      │
│  DriverError ──errors──> DriverResult<T>                     │
│       │                │                                     │
│       ├── should_retry()                                     │
│       └── message()                                          │
└──────────────────────────────────────────────────────────────┘
        ▲            ▲             ▲            ▲
        │            │             │            │
   block crate   net crate   display crate   virtio crate
   (and kdriver, input, vsock, and all other driver sub-crates)
```

| Component | Responsibility |
|------|------|
| `DeviceKind` | Enumerates all supported device categories, provides the stable short names of `as_str()` |
| `DriverError` | Unified driver error codes, with retry classification and log messages |
| `DriverResult<T>` | The dedicated `Result` alias for driver operations |
| `Device` | The trait every device implements: device identity metadata |

## State Machine

None. This module is pure type definitions with no state management.

## Flows

The module has no complex algorithms; the core logic is enum matching:

### Error retry classification

1. The caller receives `DriverResult::Err(e)`.
2. `e.should_retry()` reports whether the error is retryable
   (`WouldBlock` / `ResourceBusy`).
3. If retryable, the caller backs off and retries per its own policy.

### Device classification dispatch

1. `Device::device_kind()` yields the `DeviceKind`.
2. The caller dispatches on the `DeviceKind` variant to the matching
   subsystem.

## Concurrency Model

The module defines only types and traits: no interior mutability, no
concurrency concerns of its own.

- `Device` requires implementers to be `Send + Sync`, so trait objects can
  be shared across threads.
- `DeviceKind` and `DriverError` are `Copy` types and trivially thread-safe.

## Design Decisions

### Why an enum instead of strings for device categories

A `#[repr(u8)]` enum rather than strings:

- exhaustive matching at compile time; a missing variant is a compiler
  error;
- zero heap allocation and `Copy` semantics, suitable for hot paths;
- `as_str()` supplies human-readable names for logging/debugging only.

### Why DriverError does not implement std::error::Error

x-kernel is `no_std` and cannot rely on the `std::error::Error` trait.
Basic error description is provided through `core::fmt::Display` instead.

### Why the Device trait has only three methods

`Device` intentionally stays a minimal interface describing device
identity metadata:

- `name()` / `device_kind()` / `irq()` are the identity information every
  device needs;
- concrete operations (read/write, configuration, ...) are defined by the
  dedicated traits of each sub-crate, with `Device` as their super-trait;
- this avoids unnecessary abstraction in the base layer and keeps the
  layers orthogonal.

### Why irq() returns Option<usize> instead of Result

Some devices (such as ramdisks) use no interrupts, and `Option` is the
more accurate shape:

- `None` means the device uses no interrupt (a normal condition);
- `Some(irq)` names the interrupt the device uses;
- `Result::Err` would imply a failed operation, which "no interrupt" is
  not.

### Why DeviceKind includes a Bus variant

Bus controllers (such as a PCI host bridge) are devices the driver
framework must manage too:

- bus drivers enumerate and configure their child devices;
- folding them into `DeviceKind` reuses the device registration and
  discovery machinery;
- bus devices get the same identity-query interface as every other
  category.
