# memaddr Design

## Scope and Layout

`memaddr` provides integer-backed physical and virtual addresses, half-open
address ranges, page iterators, and alignment helpers. It does not establish
memory mappings or own the memory represented by an address.

- `src/lib.rs`: public exports, size constants, and alignment helpers.
- `src/units/addr.rs`: address types and arithmetic operations.
- `src/units/range.rs`: ranges and range-construction macros.
- `src/units/iter.rs`: fixed-size and runtime-size page iterators.
- `src/units/mod.rs`: internal exports.

The implementation uses value types without allocation, locks, or scheduler
state. It can be used in early boot and interrupt contexts subject to each
operation's alignment and arithmetic preconditions. Address values do not own
resources and do not unmap or free memory when dropped.

## Guest Tests

`unittest::path_mod_test!` mounts these files at their original module locations,
preserving visibility and `super` resolution:

| Source parent | Module / visibility | File | Cases |
| --- | --- | --- | --- |
| `src/lib.rs` | `tests_memaddr` / `pub` | `ut-tests/lib.rs` | 3 |
| `src/units/mod.rs` | `tests_units` / `pub(crate)` | `ut-tests/units.rs` | 7 |
| `src/units/iter.rs` | `tests_iter` / `pub(super)` | `ut-tests/iter.rs` | 3 |
| `src/units/range.rs` | `tests_range` / `pub(super)` | `ut-tests/range.rs` | 3 |

All 16 cases use `def_test` registration under `cfg(unittest)`, without extra
feature, architecture, serial, or user-task gates. They are guest tests, not
Cargo host `#[test]` cases, and require no external fixtures.

Each invocation also has an outer `cfg(unittest)` to avoid resolving the
allocator-dependent unittest runtime macro in non-UT boot consumers. This
does not remove the existing Cargo dependency; boot helper builds must still
exclude the kernel's `--cfg unittest`, as in the standard build flow.
