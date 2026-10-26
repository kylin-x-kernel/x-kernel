# MM API Design

DmaMappingIf accepts only the two attribute transitions needed by coherent DMA:
Coherent and Normal. It cannot request arbitrary user/execute/page-table flags.
memspace owns the mapping lock, range validation and flag selection. kdma
owns allocation, device quiescence and platform sharing. No runtime adapter,
page allocator or page-table implementation is part of this API.
