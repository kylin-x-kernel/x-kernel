---
applyTo: "**/*.rs"
---

## Rust OS style
- Write Rust-first kernel code, not C or C++ code transliterated into Rust.
- Prefer expressing state and invariants through the type system: enums over integer mode switches, newtypes over raw `usize` identifiers, and dedicated config/descriptor structs over long positional argument lists.
- Prefer traits plus composition over inheritance-shaped layering or large god objects. Keep platform glue thin and move reusable hardware logic into shared driver crates.
- Prefer ownership and borrowing to shared mutable global state. When global state is unavoidable, keep it minimal, encapsulated, and tied to a clear subsystem boundary.
- Keep `unsafe` small and well-contained. Put the unsafe boundary near the hardware/register access point and keep the surrounding API safe when possible.
- Prefer `Option`/`Result` and explicit error propagation over sentinel values, magic return codes, and silent fallback behavior.
- Prefer pattern matching and small helper types over deeply nested conditionals and boolean parameter combinations.
- When behavior differs by architecture or transport, encode the distinction in types/config enums or subsystem boundaries instead of scattered `if arch == ...` style branching.
- Reuse existing helpers in `khal`, `drivers`, and `memspace` before adding parallel abstractions.

## Kernel-specific design guidance
- For MMIO/IO-port/device resources, keep discovery, mapping, and runtime access responsibilities separate.
- For interrupt-driven code, separate descriptor/configuration, low-level ack/mask operations, and higher-level wakeup/consumer logic.
- For boot/runtime handoff code, keep boot-only assumptions localized and do not leak them into runtime subsystems unless the handoff contract explicitly requires it.
- Avoid overusing raw addresses and integers in business logic. Convert them into address wrapper types, descriptors, or transport enums as early as possible.
- Prefer APIs that make the valid path easy and invalid states hard to represent.
