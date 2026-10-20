# net — Design

## Purpose

`net` defines the core contract between NIC drivers and the network subsystem:
the `NetDevice` trait that a NIC driver implements, the `NetRxScheduler` hook
the network stack attaches to interrupt-driven drivers, and the pooled
`NetBuf` buffer family exchanged across that boundary. The crate also
re-exports the shared driver vocabulary (`Device`, `DeviceKind`,
`DriverError`, `DriverResult`) from `driver_base` so that NIC drivers depend
on a single contract crate.

## Responsibilities

- Define `NetDevice`, the operations a NIC driver must provide: address and
  queue capabilities, non-blocking `send`, `recv`, TX/RX recycling, and TX
  buffer allocation.
- Define `NetRxScheduler`, the IRQ-safe receive-scheduling hook stored by
  drivers and invoked after RX interrupt acknowledgement.
- Provide the `NetBuf` / `NetBufHandle` / `NetBufBox` buffer family and the
  fixed-slot `NetBufPool` allocator used for zero-copy packet exchange.
- Provide `MacAddress` as the common NIC hardware address type.

## Non-Responsibilities

- No protocol processing (L3/L4), sockets, or routing: the network stack owns
  all of that. The crate only moves framed byte buffers across the boundary.
- No device discovery, enumeration, or binding: platform code and
  `kdriver` own driver registration and probe.
- No concrete NIC drivers. The `fxmac` and `ixgbe` adapter sources are kept
  in-tree (`src/fxmac.rs`, `src/ixgbe.rs`) but their module declarations and
  features are disabled; they are not part of the built crate.
- No global device registry: a driver backend is reached through the
  `Device` object its implementation lives in.

## Scope

```text
drivers/contracts/net/
├── src/
│   ├── lib.rs        # NetDevice, NetRxScheduler, MacAddress, re-exports
│   ├── net_buf.rs    # NetBuf, NetBufHandle, NetBufBox, NetBufPool
│   ├── fxmac.rs      # dormant Phytium FXMAC adapter (not compiled in)
│   └── ixgbe.rs      # dormant ixgbe adapter (not compiled in)
├── ut-tests/
│   ├── net_buf.rs    # 2 reachable guest unit tests
│   ├── fxmac.rs      # 2 dormant historical mock tests
│   └── ixgbe.rs      # 2 dormant historical mock tests
└── Cargo.toml
```

## Guest Unit Tests

Each source module mounts its file under `ut-tests/` with
`unittest::path_mod_test!`. The module names and visibility remain
`pub(crate) net_buf::tests_netbuf`, `pub fxmac::tests_fxmac`, and
`pub ixgbe::tests_ixgbe`. The macro preserves the former `mod_test`
`cfg(unittest)` gate and test lint exemptions; registration attributes,
assertions, fixtures, and private access are unchanged. No dependency or
public production API is added.

Only the two netbuf cases are in the current compilation graph. They cover
handle boundaries and local buffer-pool operations, require an initialized
allocator and the kernel synchronization environment, and use no real NIC.
They have no additional feature or architecture gate. A network-enabled
kernel that links this contract can select them with
`make unittest UNITTEST_CRATE=net`; the crate filter does not itself enable
networking or add the crate to the kernel dependency graph.

The other four cases remain uncompiled: both adapter module declarations,
their feature declarations, and their SDK dependencies are commented out.
Moving their tests does not enable them. They are historical mock/queue/frame
tests rather than hardware integration tests, but still reference their
parent adapter's types and imports. Re-enabling them would require separate
SDK integration and test correctness work; six relocated cases must not be
reported as six executed or validated cases.

## Architecture

```text
network stack (caller)                NIC driver (implementer)
        |                                     |
        |  uses `NetDevice` (send/recv/...)    |
        +------------> contract <-------------+
        |  provides `NetRxScheduler`          |
        +-- via `set_rx_scheduler` ---------->'
        |                                     |
        +--- `NetBuf`/`NetBufHandle` --- shared buffer currency ---+
                                              |
                                   `NetBufPool` (fixed slots, RAII)
```

- `NetDevice` extends `driver_base::Device`; every NIC driver implements it
  and the network stack is its consumer.
- `NetRxScheduler::schedule_rx` is provided by the network stack, attached
  through `NetDevice::set_rx_scheduler`, and called by the driver from its
  IRQ handler. The hook only records RX work; packet progress stays owned by
  the network stack.
- `NetBuf` is the buffer type; `NetBufBox` (`Box<NetBuf>`) is its RAII form
  and `NetBufHandle` its raw-pointer form for crossing driver interfaces
  without lifetime plumbing.

## Buffer Pool And Lifecycle

`NetBufPool::new(slot_count, buf_len)` allocates one contiguous
`slot_count * buf_len` storage and a free-offset list. Creation rejects a
zero `slot_count` and any `buf_len` outside `1526..=65535` with
`DriverError::InvalidInput`.

- Construction: `alloc_buf` / `alloc_boxed` pop one slot offset from the
  free list and wrap it in a `NetBuf` that keeps an `Arc<NetBufPool>`.
- Use: header and payload regions are addressed by `hdr_len` / `payload_len`
  inside the slot; `set_hdr_len` / `set_payload_len` validate the combined
  frame length against the slot and return `DriverError::InvalidInput` on
  overflow or out-of-bounds sizes.
- Release: `Drop for NetBuf` returns the slot offset to the pool; no manual
  free call exists. `into_handle` / `from_handle` convert between the owned
  and raw-handle forms for drivers that stage buffers in descriptor rings;
  a handle must be consumed by `from_handle` exactly once.

## Execution Context

- `NetRxScheduler` implementations must be safe from hardirq context: no
  sleeping, no hot-path allocation, no callbacks into driver receive paths.
- `NetDevice::send` is non-blocking by contract; `recv` returns
  `DriverError::WouldBlock` when no packet is pending.
- `NetBufPool` allocation takes an internal spin lock and is safe to call
  from any context that may not sleep; the network stack decides where
  buffers are allocated.
- No requirement for a current userspace process, scheduler services, or
  late-boot state; the contract is usable from early driver bring-up.

## Concurrency Model

- `NetBufPool` guards `free_offsets` with `ksync::Mutex`; alloc and release
  are short critical sections. Buffer slots themselves are exclusively owned
  by one `NetBuf` at a time, and mutable access goes through `&mut self`, so
  no additional synchronization is needed on buffer contents.
- `NetDevice` implementations are shared (`&self`) and must make
  queue operations internally synchronized; the contract layer adds none.
- `NetRxScheduler` is `Send + Sync` because it is stored in drivers and
  invoked from interrupt context.

## Error Model

All fallible operations return `driver_base::DriverResult`; no crate-local
error type is defined. Documented failure results:

- `NetBufPool::new` — `DriverError::InvalidInput` for a zero slot count or
  an out-of-range buffer length.
- `NetBuf::set_hdr_len` / `set_payload_len` — `DriverError::InvalidInput`
  when header plus payload overflows or exceeds the slot length.
- `NetDevice::recv` — `DriverError::WouldBlock` when no packet is available.
- `NetDevice::set_rx_scheduler` — default `DriverError::Unsupported` for
  poll-only drivers.

## Design Decisions

- Pooled fixed-slot buffers instead of per-packet heap allocation: kernel
  network paths must not allocate on the hot path; a slot is reused through
  RAII drop, and one contiguous storage keeps DMA-friendly layouts simple.
- Raw `NetBufHandle` instead of `Arc<NetBuf>` on driver interfaces: descriptor
  rings need plain pointers; the single-consumption contract is documented on
  `NetBuf::from_handle` (`# Safety`) rather than paid for on every packet.
- Header/payload split inside one buffer: protocols prepend or strip headers
  without copying the payload, and `frame()` exposes the contiguous view.
- Contract-only crate: hardware knowledge stays in driver crates; keeping the
  dormant `fxmac`/`ixgbe` sources here is historical and they compile only if
  their module declarations and features are re-enabled.
- `driver_base` types are re-exported (not redefined) so the NIC driver
  surface has one import root and error semantics stay defined in one place.
