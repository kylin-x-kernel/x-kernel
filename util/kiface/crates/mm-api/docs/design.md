# MM API Design

DmaMappingIf accepts only the two attribute transitions needed by coherent DMA:
Coherent and Normal. It cannot request arbitrary user/execute/page-table flags.
memspace owns the mapping lock, range validation and flag selection. kdma
owns allocation, device quiescence and platform sharing. No runtime adapter,
page allocator or page-table implementation is part of this API.

`set_mode` returns a `DmaMappingCompletion` when the provider detaches the
kernel invalidation from the page-table lock. memspace creates it; the caller
(kdma) owns it and must call `complete()` after releasing every lock held around
the mode change, because a CPU that needs such a lock spins with interrupts
disabled and cannot acknowledge the TLB shootdown. `complete()` reports whether
the invalidation completed; dropping the value still attempts it but cannot
report the result. `None` means the provider completed the invalidation in place
(user page table, fresh-map fence, or a pending managed retirement).
