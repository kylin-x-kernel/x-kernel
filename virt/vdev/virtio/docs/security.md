# vdev-virtio — Security and Reliability Analysis

## Trust Model

- The caller (device implementation / VMM) is trusted: it chooses the queue parameters, passes in the `GuestDma`
  capability, and decides when to process the queues.
- The guest is **untrusted**: the guest may freely write MMIO registers, construct virtqueue descriptors and
  available/used rings, and specify arbitrary guest physical addresses and lengths.
- This crate must perform bounds and consistency checks on guest-controlled input; any out-of-bounds access, wrong access width, or illegal
  state returns an error or degrades safely, rather than causing UB.

## External Boundaries / Attack Surface

| Boundary | Description |
|------|------|
| MMIO register write | The offset/width/value written by the guest; the offset may fall in a standard register or in the device configuration space |
| MMIO register read | The read width chosen by the guest; invalid widths must be rejected |
| Queue address | Guest physical addresses written through the high and low halves of `QUEUE_DESC/AVAIL/USED` |
| Queue size | The relationship between `QUEUE_NUM` and the device `max_size` |
| Descriptor chain | Chain length, `NEXT`/`INDIRECT` flags, addresses and lengths, all coming from guest memory |
| Ring index | The available/used idx is a free-form integer written by the guest and may wrap around |
| Device configuration space | Configuration offsets interpreted by the device implementation, coming from guest reads |

This crate does not touch **user memory / file systems / network data**; it only touches the above
**MMIO/virtqueue/DMA** boundaries.

## unsafe Code Inventory

All `unsafe` code is concentrated in `src/memory.rs` and serves the default implementation of `GuestMemoryAccessor`;
the production path uses `dma::DmaGuestMemory` (no `unsafe`).

| Location | Invariant | Why it is safe | Constraint entry point |
|------|--------|-----------|----------|
| `memory.rs:66` `read_buffer` | The host address returned by `translate_and_get_limit` is dereferenceable for at least `limit` bytes, and `limit >= data.len()` | The length is validated before the copy; the source range of `copy_nonoverlapping` does not overlap the destination `data` | Called only by `GuestMemoryAccessor` implementors |
| `memory.rs:89` `write_buffer` | Same as above (destination side) | The length is validated; the destination is a guest memory mapping | Same as above |
| `memory.rs:110` `read_obj` (`unsafe fn`) | `T` is a POD that is byte-addressable and has no invalid bit patterns, padding invariants, or owned pointers; the byte view of `MaybeUninit<T>` is valid | `assume_init` is called only after `read_buffer` has filled all the bytes | The `# Safety` contract of the `unsafe fn`; the caller guarantees the layout of `T` |
| `memory.rs:132` `write_obj` (`unsafe fn`) | `T` is a POD | Reading the bytes of a live value is safe for a POD | Same as above |

`read_obj`/`write_obj` are declared as `unsafe fn`: `T: Sized` does not guarantee that `T` is a POD, and passing a type that contains
pointers, invalid bit patterns, or `bool` would trigger UB. The POD constraint is therefore made a
`# Safety` contract that the caller must satisfy, rather than a default safe method.

The caller must ensure that the host address returned by `translate_and_get_limit` can be dereferenced directly, and that
the returned `limit` truly reflects the contiguous accessible range.

## Memory Safety Invariants

1. Any length/address coming from the guest must go through translation and upper-bound validation before it is used for reads or writes.
2. Descriptor chain traversal must be bounded (`MAX_DESCRIPTOR_CHAIN_LENGTH`), and must guard against infinite loops
   caused by cycles/self-references.
3. Ring index wraparound must use `wrapping` semantics, and the result must be taken modulo the queue size.
4. `VirtioQueue` must not be accessed by the data path before `ready`; before `ready` is written, the ring
   layout must be validated (ranges do not overlap and are not out of bounds).

## Thread Safety

- `vdev_core::GuestDma: Send + Sync`; `DmaGuestMemory` holds only a shared reference and is naturally
  `Send`.
- All mutable state of `VirtioMmioState` is protected by `SpinNoIrq` and may be accessed
  concurrently on the guest exit path; `T: GuestMemoryAccessor + Clone` is shared as `Arc<T>`.
- This crate neither registers interrupts nor creates tasks; thread-safety responsibility lies with the caller in how it shares the device instance.

## Threat Analysis

| ID | Threat description | Impact level | Trigger condition | Mitigation |
|------|----------|----------|----------|----------|
| T-01 | An out-of-bounds guest address causes out-of-bounds host memory reads/writes | High | A descriptor/ring address points to an unmapped region | All reads and writes are translated through `GuestDma`; failure returns `DmaError`→`VirtioError::InvalidAddress`; `GuestMemoryAccessor` implementations must validate `limit` |
| T-02 | `QUEUE_READY` ring ranges overlap or are out of bounds | High | The guest submits an illegal desc/avail/used layout | The layout is validated before ready; if illegal, setting ready is rejected |
| T-03 | A malicious descriptor chain causes an infinite loop/exhaustion | Medium | Descriptor self-reference/cycle | Chain length is capped at `MAX_DESCRIPTOR_CHAIN_LENGTH`; exceeding it returns an error |
| T-04 | Illegal feature request | Medium | The driver writes `FEATURES_OK` but includes bits that were not advertised | The subset relationship is validated; if it is not satisfied, `FEATURES_OK` is cleared |
| T-05 | Wrong-width MMIO access | Low | The guest uses a width other than 1/2/4/8 bytes | `transport` rejects it and returns an error/0 |
| T-06 | Missed interrupt | Low | A new event arrives after the driver reads `INTERRUPT_STATUS` | `raised_after_read` is set and `InterruptPending` is returned, so the device injects again |

## Failure Mode and Effects Analysis (FMEA)

| ID | Failure mode | Cause | Local effect | System effect | Severity | Mitigation |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | Queue processing fails | Illegal guest address / illegal descriptor | A single request reports an error | Guest I/O fails and can be retried | 3 | Return `VirtioError`; the device implementation writes back an error status |
| F-02 | Layout validation rejects ready | Illegal guest parameters | The queue is unusable | Guest driver initialization fails | 3 | Remain not ready; the guest can reprogram |
| F-03 | The backend may need deferred processing | The device backend may block | `QueuePending` | If the VMM has no polling path, the queue stalls | 3 | The device implementation declares `requires_deferred_processing`, and the VMM provides polling |
| F-04 | Inconsistent interrupt state | Concurrent reads and writes of interrupt registers | Interrupt missed/duplicated | The guest may wait until timeout | 4 | Single-lock protection + `raised_after_read` |

## Fault Management

- All guest-controllable failure paths return `VirtioError` (`InvalidAddress`, `InvalidQueue`,
  `InvalidDescriptor`, `InvalidBufferSize`, etc.) and do not panic.
- `VirtioMmioState::new` assumes the caller has validated the queue size; out-of-bounds indices return
  `None`/an error at runtime rather than panicking.
- There is no lock poisoning issue: `SpinNoIrq` is used, and OS thread poisoning is not involved.

## Privacy Analysis

This crate does not handle user private data; it only moves guest memory bytes, and the byte contents are interpreted by the device implementation.
It does not log guest data (except for the device implementation's own trace output as needed).

## Known Limitations

- The 4 regression cases in `ut-tests/` run in the kernel harness and cover fixed mock inputs and layouts;
  they do not cover real VM/DMA isolation. The raw pointer of the descriptor fixture points only to a
  Vec that is alive in this test and is not grown; its translator returns `usize::MAX`, so it can only be used for the existing fixed-range inputs and cannot be used to prove
  bounds safety for arbitrary addresses. This test constraint remains unchanged during the migration.
- Only the VirtIO MMIO transport (version 2) is implemented; the PCI transport is not supported.
- event-index is not yet implemented: the transport does not advertise `VIRTIO_F_RING_EVENT_IDX`, and the queue's
  `event_idx_enabled` is always `false`. The layout computation still treats the layout as including the event-index footer,
  to ensure ring range validation that is independent of negotiation.
- The full data-path capability for indirect descriptors depends on the upper-layer device implementation.
- It is not responsible for interrupt injection or hardware interaction; these are done by the device implementation/VMM.

## Audit Checklist

- [ ] Does any new guest memory read/write go through translation and length validation?
- [ ] Does new descriptor handling preserve the chain length cap and wraparound safety?
- [ ] Is the complete ring layout validation still performed before `QUEUE_READY`?
- [ ] Does each `unsafe` block / `unsafe fn` have a `SAFETY:`/`# Safety` explanation and preserve
      the translation and POD contracts of `memory.rs`?
- [ ] Does the MMIO access width validation cover newly added registers?
- [ ] Is new state protected by a corresponding lock to avoid data races?
