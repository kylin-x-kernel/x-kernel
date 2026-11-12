# char_driver — Design

## Purpose

The package name is `char_driver` (directory `drivers/contracts/char/`).

`char_driver` defines the minimal contract for character device drivers that join
the unified discover → bind → activate driver pipeline: the `CharDevice`
trait, a byte-stream read/write/flush surface implemented on top of the
shared `driver_base::Device` base. Typical implementers are `hwrng`
sources, secondary serial/UART expansion cards, and runtime wrappers that
hand a boot-initialized device to the generic pipeline.

## Responsibilities

- Define `CharDevice: Device` with three operations: `read` (bounded byte
  read, `Ok(0)` meaning end-of-stream for finite sources),
  `write` (bounded byte write), and `flush` (no-op default for stateless
  devices).
- Re-export the shared driver vocabulary — `Device`, `DeviceKind`,
  `DriverError`, `DriverResult` — from `driver_base` (`#[doc(no_inline)]`)
  so char drivers import one contract root.

## Non-Responsibilities

- No device nodes, no `/dev` filesystem, no cdev registry: naming and
  exposure to user space belong to the fs/device layers.
- No early-boot console: the boot console
  (`drivers/platform/console`) initializes on its dedicated path before
  the bus manager and driver registry exist. This crate only enables
  *reusing* an already-initialized console through a thin `CharDevice`
  wrapper later at runtime.
- No buffering, line discipline, or termios policy: drivers move bytes;
  policy belongs to higher layers.
- No blocking semantics: the trait is non-blocking by contract (see
  Error Model); sleeping decisions belong to callers.

## Scope

```text
drivers/contracts/char/
├── src/
│   └── lib.rs        # CharDevice trait, driver_base re-exports
└── Cargo.toml
```

## Architecture

```text
driver_base (Device, DriverError/Result, discovery pipeline)
      ^
      |  impl CharDevice for <driver>
      |
char_driver (this crate: byte-stream trait only)
      ^
      |  consumes &self read/write
      |
char-device consumers (higher-layer callers of CharDevice implementations)
```

The crate is a single trait plus re-exports; there is no state, no
registry, and no object type.

## Execution Context

- Trait methods run in whatever context the caller drives the device from;
  implementations must be safe to call from task context and must not
  require sleeping (they return `WouldBlock` instead).
- `read`/`write` take `&self`, so implementations must be internally
  synchronized if the device is shared.
- No early-boot restriction: implementers may exist only after the driver
  pipeline is up, per the pipeline contract in `driver_base`.

## Error Model

All results are `driver_base::DriverResult`:

- `read` — `Ok(n)` with `n <= buf.len()`; `Ok(0)` is end-of-stream for
  finite sources; open-ended sources return `Err(DriverError::WouldBlock)`
  so the caller decides retry versus sleep.
- `write` — `Ok(n)` bytes accepted; `Err(DriverError::WouldBlock)` when
  progress requires blocking; drivers must not spin internally.
- `flush` — defaults to `Ok(())`.

## Design Decisions

- Deliberately minimal trait surface (read/write/flush): a wide range of
  byte devices can implement it without inheriting tty or storage policy.
- Non-blocking contract with `WouldBlock`: kernel callers need explicit
  control over waiting; hidden spinning inside drivers would couple driver
  implementations to scheduler state.
- Re-export `driver_base` types instead of redefining errors: one error
  vocabulary across all driver contract crates, one import root for
  implementers.
- Boot console stays outside this contract: the console must work before
  any registry exists; wrapping it for runtime reuse keeps both paths
  honest about their execution context.
