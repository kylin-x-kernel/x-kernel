# device-res — Design

Kernel unit tests live in `ut-tests/` and are mounted from their original
source modules with `unittest::path_mod_test!` under `cfg(unittest)`.

## Purpose

`device-res` provides the OS-neutral device resource description model and
provider contract. It is the single entry point through which the driver
subsystem expresses "which kernel capabilities a driver needs", covering
MMIO mapping, I/O ports, interrupts, DMA buffers, ordinary aligned CPU storage,
and the monotonic clock,
and it decouples those semantics from concrete kernel implementations.

Upstream subsystems that depend on this module include:

- platform drivers (acquiring resources via RAII handles or `devm_*`
  functions);
- the kernel bus/device model (implementing the [`DeviceResource`] trait to
  support device-managed resources).

## Non-Responsibilities

- No concrete kernel behavior behind the provider traits: `MmioOp` /
  `IrqOp` / `DmaOp` / `CpuMemoryOp` / `TimeOp` implementations belong to the host kernel's
  provider crate (in x-kernel, `device-res-xkernel`, backed by `memspace`,
  `kirq`, and `kdma`).
- No global provider state or provider selection: the driver framework
  (for example `kdriver::resource`) holds the provider instance and passes
  it explicitly; this crate stores none.
- No MMIO region overlap detection: rejecting overlapping maps (`Busy`) is
  owned by the `MmioOp` implementation.
- No validation of `ResourceDesc` contents: firmware-derived addresses,
  sizes, and IRQ numbers are trusted to the provider implementations,
  which must check them at the `map_mmio` / `request_irq` boundary.
- No interrupt vector, APIC id, or CPU vector allocation: those are
  internal state of the host IRQ core; this crate only hands out the
  provider-generated `MsiResource` (see Threat T-09).
- No DMA cache-coherency protocol of its own: coherency strategy
  (uncached mappings, fences, cache maintenance) is decided by the
  `DmaOp` backend.
- No devres lifecycle engine: cleanup callbacks are registered through
  the caller's `DeviceResource` implementation, and the device model
  drives their LIFO execution.

## Background

Different kernels expose different interfaces for the same class of
capability: mapping MMIO, registering interrupts, allocating DMA, reading
monotonic time. Driver code that calls kernel APIs directly must be edited
function by function when ported. This module separates resource discovery
from resource use: the driver describes "what it needs", and the host
kernel supplies "how it is provided" through the MMIO / IRQ / DMA provider
traits, which is what makes drivers portable across kernels.

## Scope

```text
drivers/contracts/device-res/
├── src/
│   ├── lib.rs
│   ├── dma.rs
│   ├── irq.rs
│   ├── mmio.rs
│   ├── memory.rs
│   ├── provider.rs
│   └── time.rs
├── Cargo.toml
└── docs/
    ├── design.md
    └── security.md
```

## Architecture

```text
                    ┌─────────────────────────────────────────────┐
                    │             Host Kernel                      │
                    │  implements provider traits                  │
                    │  passes provider to driver framework         │
                    └───────────────┬─────────────────────────────┘
                                    │
            ┌───────────────────────┼───────────────────────┐
            │                       │                       │
            v                       v                       v
     ┌──────────────┐      ┌──────────────┐      ┌──────────────────┐
     │  Io (MMIO)   │      │  Irq         │      │  DmaCoherent     │
     │  RAII handle │      │  RAII handle │      │  RAII handle     │
     │  map on new  │      │  request     │      │  alloc on new    │
     │  unmap drop  │      │  release drop│      │  free on drop    │
     └──────────────┘      └──────────────┘      └──────────────────┘
            ^                       ^                       ^
            │                       │                       │
     ┌─────────────────────────────────────────────────────────────┐
     │              devm_* helpers (device-managed)                │
     │  devm_iomap / devm_request_irq* / devm_alloc_coherent      │
     │  register cleanup via DeviceResource trait                 │
     └─────────────────────────────────────────────────────────────┘
                        ^
                        │
               ┌────────────────┐
               │  Driver Code   │
               │  reads/writes  │
               │  registers via │
               │  RAII or devm  │
               └────────────────┘
```

### Core Components

| Component | Responsibility |
|------|------|
| `ResourceDesc` / `ResourceSet` | Describes one device's hardware resources (MMIO, I/O ports, interrupts, DMA) |
| `MmioOp` / `IrqOp` / `DmaOp` / `TimeOp` traits | The capability backends a host kernel implements (map/unmap, request/release IRQ, alloc/free DMA, alloc/free MSI-X, monotonic time) |
| `ResourceProvider` trait | Combined `MmioOp + IrqOp + DmaOp + TimeOp` trait; CPU storage is requested separately |
| `CpuMemoryOp` / `CpuBuffer` | Fallible ordinary aligned memory with an initialized exclusive byte view and paired release; no DMA mapping |
| `Io` | RAII handle over an MMIO mapping, with register read/write methods using acquire/release fences |
| `Irq` | RAII handle over an interrupt registration, released automatically on drop |
| `DmaCoherent` | RAII handle over a coherent DMA buffer, freed automatically on drop |
| `DeviceResource` trait | OS-neutral device abstraction through which drivers read resources and register cleanup callbacks |
| `devm_*_with_provider` functions | Bind resource lifetime to a device, cleaning up automatically on probe failure or removal |

### Provider Selection

`device-res` keeps no global provider state. The driver framework holds the
provider instance supplied by the host kernel and calls
`Io::map_with()`, `Irq::request_with()`, `DmaCoherent::alloc_with()`, or
`devm_*_with_provider()`. RAII handles remember the provider that created
them and return to that same provider for release on drop.

## Calling Constraints / Execution Context

- **Callable in early boot**: the module depends on neither the scheduler
  nor a process-thread context; provider lifetime is guaranteed by the
  caller.
- **No resource acquisition or release from interrupt context**: the
  provider trait method docs declare that they run in normal (non-IRQ)
  context. MMIO register reads/writes themselves may run in any context,
  but acquisition/release (`map`, `request`, `alloc`, and the
  corresponding drops) must not run in interrupt context.
  `TimeOp::monotonic_time()` may be used for short polling and timeout
  checks; each provider implementation must state whether it is callable
  from IRQ-like contexts.
- **No sleeping or blocking**: `device-res` itself takes no global
  provider lock; provider methods must still respect each host kernel's
  probe/remove context constraints.
- **No current process thread required**: the API depends only on the
  current execution path.
- **Reentrancy**: `device-res` never calls into a provider while holding a
  global lock; provider implementations should still avoid their own lock
  recursion inside resource-release callbacks.

## Flows

### Resource acquisition (`Io::map_with` as the example)

```text
Io::map_with(provider, region, name)
  │
  ├─ provider.map_mmio(region, name)?
  │    └─ host kernel performs the actual mapping
  │
  └─ Ok(Io { provider, mapping: Some(mapping) })
```

### RAII resource release (`Io::drop` as the example)

```text
Io::drop()
  │
  ├─ mapping.take()
  ├─ provider.take()
  │
  └─ if both Some:
       provider.unmap_mmio(mapping)
```

An RAII handle keeps the provider that created it. Drop does not re-query
external state, so a release request can never be routed to a different
provider, and cleanup does not depend on the framework re-selecting one.

### Device-managed resources (`devm_iomap` as the example)

```text
devm_iomap_with_provider(provider, device, region, name)
  │
  ├─ Io::map_with(provider, region, name)?  → io
  ├─ io.as_ptr()                           → ptr
  ├─ device.register_cleanup(move || drop(io))
  │    └─ callback runs LIFO when the device is removed
  │
  └─ Ok(ptr)
```

## Concurrency Model

- **Explicit provider**: the driver framework holds the provider; resource
  handles remember their creating provider and add no shared mutable state.
  `CpuBuffer<'a>` borrows its provider for the local allocation lifetime; DMA,
  MMIO and IRQ handles retain their existing static-provider requirement.
- **RAII handles**: `Io`, `Irq`, and `DmaCoherent` are not `Sync` (they
  hold `NonNull` internally) and cannot be shared across threads. They may
  move between threads (`Send`), but only one thread holds a handle at a
  time.
- **MMIO access**: the `read*`/`write*` methods of `Io` use acquire/release
  fences to order register accesses. Multi-byte accesses carry
  `debug_assert` alignment checks.

## Design Decisions

### Why the provider API uses trait objects

RAII handles store `&'static dyn MmioOp` / `IrqOp` / `DmaOp` internally:

- the driver framework can hold a concrete provider type and manage
  capabilities through trait bounds;
- handles must store a provider reference inside devres cleanup closures;
  trait objects keep provider generics from leaking into ordinary driver
  code and cleanup containers;
- provider selection happens explicitly at the framework boundary; driver
  code still sees only device-resource methods.

### Why devm_* returns raw pointers instead of RAII handles

`devm_iomap` returns `NonNull<u8>` rather than an `Io`, because `Io`'s
drop behavior conflicts with device-managed cleanup: if `Io` were
returned, the driver dropping the `Io` would release the mapping while the
device cleanup callback would also try to release the same mapping.

Resolution: `devm_iomap` creates the `Io` internally, extracts the
pointer, then registers the drop closure via `register_cleanup`. The
`Io`'s lifetime is managed by the callback; the driver keeps only the raw
pointer.

### Why Io::read*/write* use fences instead of volatile Ordering parameters

`core::ptr::read_volatile` / `write_volatile` guarantee the compiler will
not elide or reorder volatile accesses, but they give no CPU-side memory
ordering. The extra `fence(Acquire)` / `fence(Release)` ensure that on
weakly ordered architectures (AArch64) register reads and writes are not
reordered by the CPU across the fence. On strongly ordered architectures
(x86) the fences compile to no-ops.

### Why drop keeps the creating provider

Resource release must return to the same provider that allocated the
resource. Otherwise, once per-framework, per-bus, or test-mock providers
exist, re-selecting a provider at drop time could release into the wrong
backend. Keeping the creating provider encodes the acquire/release pairing
in the RAII handle itself.

## Drop / Resource Release

| Type | Drop behavior |
|------|----------|
| `Io` | If both mapping and provider are `Some`, calls `provider.unmap_mmio(mapping)` |
| `Irq` | If `armed` and provider is `Some`, calls `provider.release_irq(resource, token)` |
| `DmaCoherent` | If both allocation and provider are `Some`, calls `provider.free_coherent(allocation)` |

`Irq` uses an `armed` flag so a failed `request_irq` does not trigger a
spurious `release_irq` on drop, and it stores the provider-returned token
so releasing a shared IRQ removes only the handler registered by this
handle.

### MSI-X Resources

`IrqOp::alloc_msix()` returns an `MsiResource`:

- `MsiResource::irq` is the OS-visible IRQ, used by the driver or upper
  framework to register the handler;
- `MsiResource::message` is the device-visible MSI message, used by the
  PCI/MSI-X code to program the device table or MSI register.

`MsiResource` is an explicitly owned resource and does not implement
`Copy`. Callers may move it along one ownership chain and must call
`free_msix()` to release it after the IRQ handler is unregistered; implicit
copies cannot create multiple releasers of one MSI allocation.

`device-res` exposes no APIC ids, CPU vectors, or irqchip-private
allocation cookies. Those are internal state of the host IRQ core and its
backends. In x-kernel this provider is implemented by
`device-res-xkernel`, which forwards to `kirq::alloc_msix()`; `device-res`
itself stays OS-neutral and does not depend on `kirq`.

### Ordinary CPU storage is separate from DMA

`CpuMemoryOp` supplies a fallible allocation for a nonempty `Layout` and
releases its original pointer/layout. It is an unsafe implementation contract:
providers must return unique live storage with the requested alignment.
`CpuBuffer<'a>` retains that provider borrow, initializes all bytes, lends one
exclusive byte slice, and releases exactly once. It adds no test-only storage
variants or accounting fields. Tests inject providers through the same API.

CPU staging neither needs nor creates a device bus address, DMA mapping,
uncached mapping, or platform-sharing operation. Backends still own any actual
DMA/cache protocol. `ResourceProvider` keeps its existing four capabilities;
drivers that need CPU staging request `CpuMemoryOp` explicitly. Host adapters
may use their ordinary fallible allocator without coupling portable drivers to
that allocator.
