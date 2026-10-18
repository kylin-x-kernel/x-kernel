# device-res — Security And Reliability

## Scope

This analysis covers the entire crate: `src/lib.rs` (handles, the MMIO
access surface, `unsafe impl Send`), `src/dma.rs` (coherent handles),
`src/irq.rs` (IRQ/MSI resources), `src/memory.rs` (CPU-only storage),
`src/provider.rs`, and `src/time.rs`.
No modules are excluded; every unsafe item is enumerated in the
inventory below. The host-side implementation lives in
`device-res-xkernel` and is audited in its own security document.


## Overview

`device-res` is x-kernel's unified abstraction layer through which drivers
acquire hardware resources. The module operates directly on MMIO registers
and DMA buffers, and contains `unsafe impl Send`, raw-pointer
dereferences, and volatile accesses. Incorrect use or broken invariants
can lead to undefined behavior or device malfunction.

## Trust Model

```
Driver code (trusted)
   │
   │ safe API: Io::map_with, Irq::request_with,
   │           DmaCoherent::alloc_with, devm_*_with_provider
   │
   v
┌──────────────────────────────────────────────────────────────┐
│  device-res                                                  │
│                                                              │
│  ┌── unsafe boundaries ───────────────────────────────────┐ │
│  │ unsafe impl Send for MmioMapping, DmaAllocation        │ │
│  │ Io::access_ptr → ptr.add(offset)                       │ │
│  │ Io read*/write* → ptr.read_volatile / write_volatile   │ │
│  └─────────────────────────────────────────────────────────┘ │
│                                                              │
│  MmioOp / IrqOp / DmaOp / TimeOp provider traits            │
└──────────────────┬───────────────────────────────────────────┘
                   │
                   v
         Host Kernel implementation (trusted)
```

- **Driver code**: trusts `device-res` to manage RAII handle lifetimes
  and bounds checks correctly.
- **Host kernel providers**: trusted to implement `MmioOp` / `IrqOp` /
  `DmaOp` / `TimeOp` mapping, registration, and release correctly.
- **Driver framework**: owns provider custody and passes it explicitly;
  `device-res` keeps no global provider state.
- **Device firmware/hardware**: untrusted input source — MMIO read values
  and interrupt events come from outside.

## External Boundaries / Attack Surface

| Boundary | Type | Description |
|------|------|------|
| MMIO registers | Device input | Values read back by `read_volatile` come from hardware and may be arbitrary |
| MMIO registers | Device output | `write_volatile` programs device-visible registers |
| DMA buffers | Device-writable memory | Coherent DMA buffers can be modified concurrently by the device |
| Interrupts | Device signals | Interrupt handlers run in interrupt context, at device-chosen frequency |
| MSI message | Device output | `MsiResource::message` is written into device MSI/MSI-X registers or tables |
| Firmware/ACPI resource description | Boot metadata | `ResourceDesc` (addresses, sizes, IRQ numbers) comes from firmware discovery |
| Monotonic clock | Host input | `TimeOp::monotonic_time()` comes from the host kernel and drives driver timeouts |

This module:

- **never accesses user memory directly**;
- **never parses bootloader/firmware input directly** — resource
  descriptions are constructed by callers;
- **depends on no FFI or inline assembly** — it uses `core::ptr` volatile
  operations;
- **processes no filesystem, network, or IPC input**.

## Unsafe Code Inventory

### 1. `unsafe impl Send for MmioMapping` (`src/lib.rs`)

```rust
unsafe impl Send for MmioMapping {}
```

**Invariant**: `MmioMapping` carries only address values and plain
descriptors. Ownership of the underlying device mapping is exclusive to a
single `Io` handle, the address stays valid for the mapping lifetime, and
release returns to the provider that created the handle.

**Why sound**: `Io` is not `Sync` (`NonNull` is `!Sync`), so one `Io` is
never shared across threads. A `MmioMapping` is created when the `Io` is
constructed and consumed in `Io::drop`; ownership is singular.

### 2. `unsafe impl Send for DmaAllocation` (`src/lib.rs`)

```rust
unsafe impl Send for DmaAllocation {}
```

**Invariant**: `DmaAllocation` carries the coherent buffer's address
values; ownership is exclusive to the `DmaCoherent` handle, and release
returns to the provider that created the handle.

**Why sound**: same argument — `DmaCoherent` is not `Sync`; when the
handle moves between threads, buffer ownership moves with it.

### 3. `Io::access_ptr` raw-pointer offset (`src/lib.rs`)

```rust
unsafe { self.as_ptr().as_ptr().add(offset) }
```

**Invariants**:
- `offset + size <= region.size` (enforced by `checked_add` + `assert!`).
- The base address `vaddr` is guaranteed valid for the mapping lifetime by
  the provider.

**Why sound**: the offset is overflow-checked and bounds-checked, so the
resulting pointer never leaves the mapped region.

### 4. `Io::read*` — `ptr.read_volatile()` (`src/lib.rs`)

```rust
let value = unsafe { ptr.read_volatile() };
```

**Invariants**:
- The pointer is guaranteed by `access_ptr` to lie inside the mapping.
- Multi-byte reads carry a `debug_assert` natural-alignment check.

**Why sound**: `read_volatile` causes no UB (the pointer is valid and
aligned); the value read is interpreted by the caller.

### 5. `Io::write*` — `ptr.write_volatile(value)` (`src/lib.rs`)

```rust
unsafe { ptr.write_volatile(value) };
```

**Invariants**: same as `read*` — the pointer is inside the mapping and
naturally aligned.

**Why sound**: `write_volatile` writes to a valid MMIO-mapped address.

## Memory-Safety Invariants

The following must hold at all times:

1. **Mapping validity**: `MmioMapping.vaddr` held by an `Io` handle stays
   valid from handle construction to drop. Guaranteed by the `MmioOp`
   implementation.

2. **Exclusive ownership**: one mapped region is not referenced by
   multiple `Io` handles. Callers must not double-map a region.

3. **Bounds checking**: every `read*`/`write*` call satisfies
   `offset + size <= region.size`. Runtime-enforced by the `assert!` in
   `access_ptr`.

4. **Alignment checking**: multi-byte MMIO accesses are naturally
   aligned. Checked by `debug_assert!` in debug builds.

5. **DMA buffer alignment**: `DmaAllocation.cpu_addr` satisfies
   `DmaSpec.align`. Guaranteed by the `DmaOp` implementation.

6. **Provider lifetime**: the provider passed by the caller stays valid
   longer than every RAII handle and devres cleanup callback it created.
   The API requires `&'static dyn ...` providers, encoding the constraint
   in the type.

7. **MSI message origin**: `MsiResource::message` is produced by the host
   IRQ core or its backend provider. Drivers must not build messages from
   APIC ids, CPU vectors, or other controller-private state.

## Thread Safety

| Type | `Send` | `Sync` | Notes |
|------|--------|--------|------|
| `MmioMapping` | manual `unsafe impl` | auto `!Sync` (`NonNull`) | Address values may move between threads |
| `DmaAllocation` | manual `unsafe impl` | auto `!Sync` (`NonNull`) | Address values may move between threads |
| `Io` | auto (all fields `Send`) | `!Sync` (`NonNull`) | One handle is never shared across threads |
| `Irq` | auto | auto | `IrqResource` is `Copy`; `bool` is `Send + Sync` |
| `DmaCoherent` | auto (all fields `Send`) | `!Sync` (`NonNull`) | One handle is never shared across threads |

## Threat Analysis

| ID | Threat | Impact | Trigger | Existing control |
|------|----------|----------|----------|----------|
| T-01 | Out-of-bounds MMIO read/write corrupts kernel memory | High | Driver-supplied `offset + size` exceeds the mapped region | Runtime bounds check in `access_ptr` (`checked_add` + `assert!`) |
| T-02 | Unaligned MMIO access raises an architecture fault | High | Driver supplies an unaligned offset | `debug_assert!` in debug builds; release builds rely on driver correctness |
| T-03 | Device modifies kernel memory via DMA | High | Malicious or faulty device writes into kernel-visible memory through a DMA buffer | Coherent DMA buffers are explicitly allocated by the driver and device-writable range is limited to `DmaSpec.len`; an IOMMU can isolate further |
| T-04 | Interrupt storm exhausts CPU resources | Medium | Device fires interrupts continuously or interrupts are not acknowledged correctly | Throttling and masking are owned by the `IrqOp` implementation; `Irq::set_enabled(false)` can disable temporarily |
| T-05 | Drop releases into the wrong provider | High | Acquisition used one provider; drop re-resolves a different one | RAII handles remember the creating provider and drop through it |
| T-06 | Double-mapping the same MMIO region | Medium | Two drivers request overlapping MMIO regions | Overlap detection (`Busy`) is owned by the `MmioOp` implementation; this module does not check overlaps |
| T-07 | Firmware supplies malicious or wrong resource descriptions | High | Malicious or buggy firmware reports wrong MMIO bases, zero-size regions, or invalid IRQ numbers | Provider implementations should validate arguments in `map_mmio` / `request_irq`; this module does not validate `ResourceDesc` contents |
| T-09 | Driver builds its own MSI message | High | Driver writes an MSI-X table using the current APIC id or a raw vector | Wrong delivery target or vector conflict under SMP/affinity/x2APIC | The API only hands out provider-generated `MsiResource::message`; no APIC id query exists |
| T-10 | MSI resource copied implicitly, then double-freed | Medium | The resource is `Copy` or held by multiple owners | Backend vector double-free or stale MSI message reuse | `MsiResource` does not implement `Copy`; release requires moved ownership |

## Failure Modes And Effects Analysis (FMEA)

| ID | Failure mode | Cause | Local effect | System effect | Severity | Handling |
|------|----------|----------|--------|------|----------|----------|
| F-01 | `Io::map_with` returns an error | Provider refuses the mapping or mapping fails | Driver cannot access device registers | Device unavailable | 3 | `ResResult` forces error handling |
| F-02 | `Irq::request_with` returns `Busy` | Provider refuses registration, e.g. shared-handler limit reached or underlying IRQ registration failed | Driver receives no interrupts | Device interrupt function unavailable | 3 | Returns `ResError::Busy`; driver aborts probe or picks another mode |
| F-03 | `DmaCoherent::alloc_with` returns `NoMemory` | Kernel memory exhausted | DMA buffer allocation fails | DMA-dependent device function unavailable | 3 | Returns `ResError::NoMemory`; driver can degrade |
| F-04 | Offset overflow during MMIO access | `checked_add` detects usize overflow | `assert!` panic | Calling thread panics | 2 | `checked_add` prevents silent out-of-bounds |
| F-05 | MMIO access out of bounds | `offset + size > region.size` | `assert!` panic | Calling thread panics | 2 | `assert!` blocks the actual out-of-bounds access |
| F-06 | Drop releases the wrong shared IRQ handler | An armed handle constructed after a failed request, or the provider-returned token not stored | Current handler not released correctly, or other handlers on the line removed | Device interrupts misbehave | 2 | `Irq` is constructed only after `request_irq` succeeds and keeps the token so drop releases precisely |
| F-07 | `Io::map_with` / `Irq::request_with` / `DmaCoherent::alloc_with` called in interrupt context | Driver acquires resources inside an interrupt handler | Provider methods may sleep or block | System hang | 1 | Documentation constrains provider methods to normal context; no runtime detection |
| F-09 | MSI resource freed before its IRQ handler is unregistered | Wrong driver teardown order | Provider may free the backend vector while the OS-visible IRQ is still registered | Later vector reuse may hit a stale handler | 2 | Host providers should diagnose the ordering; drivers must release the handler before freeing the MSI resource |

## Failure Management

`device-res` handles failure through:

- **Error propagation**: every acquisition function returns
  `ResResult<T>`; callers propagate with `?`.
- **Panic paths**: MMIO bounds and overflow checks use `assert!` and
  panic on failure. Those are programming errors and are not recoverable.
- **Drop pairing**: every RAII handle remembers its creating provider and
  drops through it, so acquire/release backends can never mismatch.
- **MSI message encapsulation**: an MSI-X request returns the
  provider-generated `MsiResource`; drivers consume only the virq and
  message and never touch irqchip-private vectors/APIC ids.
- **No errno mapping**: no POSIX error codes are returned; the `ResError`
  enum is used instead.

## Privacy Analysis

This module is a hardware resource abstraction layer: it processes no
user data, performs no I/O on behalf of users, and involves no network
communication. No direct user-privacy concerns apply. DMA buffer contents
may include data received from the network, but their lifecycle is
managed by drivers.

## Known Limitations

1. **No overlap detection**: the module does not check whether
   repeatedly mapped MMIO regions overlap; it relies entirely on the
   `MmioOp` implementation's `Busy` check.

2. **No alignment check in release builds**: MMIO alignment is checked
   only in debug builds. In release builds an unaligned access may raise
   an architecture fault without a clear diagnosis.

3. **Providers must be `'static`**: RAII handles and devres cleanup
   closures store provider references, so short-lived providers
   constructed transiently during probe are not supported by the current
   API.

4. **`devm_*` returns raw pointers**: `devm_iomap` returns `NonNull<u8>`;
   callers must not use the pointer after device removal.

5. **Interrupt-context constraint not enforced**: `IrqHandler::handle`
   documents the no-blocking rule, but the module performs no runtime
   check.

6. **No RAII handle for MSI-X**: `MsiResource` is an explicit
   alloc/free resource without a RAII `Drop` like `Irq`. It does not
   implement `Copy`, but callers must still follow the provider contract
   to free it.

## Audit Checklist

When modifying `device-res`, verify:

- [ ] Every `unsafe impl Send/Sync` carries a `SAFETY:` comment stating
  its invariants.
- [ ] `Io::access_ptr` bounds checks cover every `read*`/`write*` entry
  point.
- [ ] New MMIO access methods include acquire/release fences.
- [ ] RAII handle drop paths use the provider saved at handle creation.
- [ ] `Irq` is constructed only after `request_irq` succeeds and stores
  the provider-returned handler token.
- [ ] New MSI APIs expose no APIC ids, CPU vectors, or controller-private
  cookies.
- [ ] `devm_*` functions acquire the resource before registering the
  cleanup callback (no empty callbacks on failure).
- [ ] New provider trait methods document their execution-context
  constraints.

## CPU-only aligned storage

`src/memory.rs` adds an unsafe `CpuMemoryOp` implementation boundary and two
raw-memory operations: initialization of the complete allocation, and creation
of its exclusive initialized byte view. The safe `CpuBuffer` API depends on the
provider's size/alignment/uniqueness guarantees. Its lifetime retains the
provider; drop returns the original pointer and layout once, after all views
end. Zero-size requests are rejected before provider entry. Acquisition errors
expose neither a handle nor uninitialized memory.

The capability must not establish DMA mappings or change cache attributes.
It grants no bus address or device access guarantee; those remain with the
separate `DmaOp` protocol. The handle has no `Send`/`Sync` overrides. Tests
cover initialization, alignment, matching release, empty requests and injected
allocation failure; real memory exhaustion is not induced.
