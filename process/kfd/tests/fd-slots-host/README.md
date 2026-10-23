# Descriptor slot storage standalone HOST / Miri fixture

This dependency-free binary compiles the real `process/kfd/src/fd_slots.rs`
using `#[path = "../../../src/fd_slots.rs"]`. Its own workspace does not load
kernel UT modules: the `#[cfg(unittest)]` accessors are absent, so the free-slot
bound is asserted through allocation results instead of by reading it.

`FileSlots` keeps values in `MaybeUninit` storage and publishes occupancy
through a bitmap, so its four `assume_init_*` calls depend on one rule: a set bit
always means the slot already holds a value. The kernel unit tests cannot pin
that rule, because they only instantiate `FileSlots<FileDescriptor>` and an `Arc`
clone or drop never unwinds. This fixture drives the same paths with a `T` whose
`Clone` and `Drop` panic, which is where an inverted publish order shows up as
reading uninitialized memory or dropping one slot twice.

Checks:

- publish order: a panicking `Clone` must leave the target empty, so nothing is
  published and a later drop touches no slot; a panicking `Drop` while
  `clone_from` clears the target must leave the target empty as well, so a later
  drop cannot drop an already dropped slot. These two paths are what the fixture
  actually distinguishes: `place` writes before setting the bit as well, but both
  of its steps are infallible, so swapping them is not observable on its own;
- allocation, `add`, `add_at` and `remove` on a dense capacity-1024 table
  compared against brute force, which knows nothing about the bitmap or the
  bound, covering word-edge ids and non-aligned `end` values;
- `get` and `get_mut` read assigned slots and refuse free ones;
- `ids()` forward, backward and interleaved consumption;
- `clone_from` clone and drop counts, including the previous occupants it clears.

Run from `/tmp` to avoid the kernel's Cargo configuration, which applies to any
directory inside the checkout. Substitute your own checkout path, as
`arch/kirq/tests/irq-work-host/README.md` also asks:

```bash
checkout=/path/to/x-kernel          # substitute your checkout path
fixture="$checkout/process/kfd/tests/fd-slots-host/Cargo.toml"
cd /tmp
CARGO_TARGET_DIR=/tmp/fd-slots-host-target cargo run --manifest-path "$fixture"
CARGO_TARGET_DIR=/tmp/fd-slots-host-miri \
  MIRIFLAGS="-Zmiri-strict-provenance -Zmiri-ignore-leaks" \
  cargo +nightly-2026-03-08 miri run --manifest-path "$fixture"
```

`-Zmiri-ignore-leaks` is required: the panic paths intentionally leak the values
that were written but never published, instead of dropping an unpublished slot.
Without the flag Miri reports those leaks as errors after the checks pass.

Add `RUSTFLAGS="--cfg unittest"` to also assert the free-slot bound directly
(`FileSlots::next_free` is gated on that cfg). Without it the bound is still
covered, because a bound that skipped a lower free id would change an allocation
result and fail the brute-force comparison.

Inverting the publish order in `clone_from` or `clear` makes this fixture dump
core natively (SIGSEGV) and makes Miri report
`reading memory ... but memory is uninitialized`, which is exactly the property
the ordering exists for.

`FILE_LIMIT` mirrors `krlimit::FILE_LIMIT`. Aliasing this crate under the kernel
crate's name keeps the fixture dependency-free, the same way
`arch/kirq/tests/irq-work-host` covers its kernel dependencies; the value only
scales the checks. This is a data-structure fixture: it does not exercise
`FdTable`, syscall paths, locking or QEMU.