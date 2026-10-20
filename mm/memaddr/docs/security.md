# memaddr Security and Reliability

## Boundaries and Invariants

Addresses are numeric values, not proof that memory is mapped, accessible, or
owned. Pointer conversion helpers in `src/units/addr.rs` produce raw pointers
without dereferencing them. Callers remain responsible for permissions,
lifetimes, alignment, and aliasing before accessing memory. The crate itself
does not access user buffers, MMIO, DMA, firmware, or device state.

Alignment helpers require a nonzero power of two. Checked arithmetic returns
`Option`; explicit wrapping operations retain wrapping behavior. Infallible
operations may panic for invalid alignment, overflow, or unordered bounds.
Callers processing untrusted sizes or addresses should use the checked paths
and validate before invoking infallible operations.

The unsafe constructors `AddrRange::new_unchecked` and
`AddrRange::from_start_size_unchecked` in `src/units/range.rs` require ordered
bounds; the latter additionally requires no addition overflow and an end
valid for the address type. Ranges and pointer values do not grant memory
access rights. The crate has no shared mutable state or synchronization.

## Test Coverage and Limits

The 16 guest cases live in `ut-tests/{lib,units,iter,range}.rs`, mounted through
`path_mod_test!` in their original parents. They exercise alignment, address
conversion, checked failures, wrapping arithmetic, range containment and
overlap, and page stepping including invalid alignment. The migration leaves
assertions and registration unchanged; it adds no coverage of pointer
dereference validity, unsafe-constructor misuse, or every overflow boundary.

There are no external fixtures or hardware prerequisites in these cases.
They need the guest unittest harness, with no additional feature or arch gate.
Production lint exceptions and unsafe contracts remain in the source; the
test macro supplies the same test lint exemptions as the former inline macro.

## Audit and Validation

- Keep numeric-address validation separate from memory-access authorization.
- Preserve checked versus wrapping arithmetic and half-open range semantics.
- Keep test modules gated by `cfg(unittest)` without widening visibility.
- Validate both normal and UT x86 full builds to cover boot consumers; guest
  UT execution alone does not validate allocator-free boot linkage.
- Test results must be reported separately from source-level preservation
  checks; moving these files is not evidence of new runtime coverage.
