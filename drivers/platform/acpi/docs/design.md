# acpi — Design

## Purpose

`acpi` provides early-boot read access to ACPI firmware description: it
locates the RSDP, walks XSDT/RSDT to find tables by signature, and extracts
the platform facts the kernel needs before its own drivers run — CPU/IO APIC
topology (MADT), PCI ECAM windows (MCFG), the PCI host bridge memory window
(DSDT `_CRS` scan), and the power-off register (FADT PM1a control block).

## Responsibilities

- Own the boot-time RSDP address: `init(rsdp_addr)` stores it in a
  crate-global slot, `desc()` / `rsdp_addr()` read it back.
- Validate and dereference firmware tables: RSDP signature and checksums
  (basic and extended), SDT header signature, length, and checksum.
- Parse MADT into `MadtInfo` plus a slice-backed `MadtEntryIter` over
  `LocalApic` / `IoApic` / `InterruptSourceOverride` entries. Iteration
  yields `Result`: a malformed entry is reported with a table-relative
  offset and ends the walk instead of panicking.
- Parse MCFG into `McfgAllocation` (first segment-0 allocation) and compute
  the ECAM `(base, size)` range per bus shift of 1 MiB.
- Best-effort scan of the DSDT for the PCI host bridge `_CRS` memory window
  (`PciHostMemWindow`).
- Extract the FADT PM1a control block (`Pm1aControlBlock`) for the power-off
  path, preferring the ACPI 2.0+ `X_PM1a_CNT_BLK` GAS and falling back to the
  legacy 32-bit `PM1a_CNT_BLK` field.

## Non-Responsibilities

- No AML interpretation: the DSDT scan is a bounded byte-pattern search
  documented as advisory, not a namespace evaluator.
- No runtime ACPI events, no GPE handling, no hotplug notification, no
  power-state transitions: only read-only boot-time table extraction, plus
  the PM1a port description consumed elsewhere for power-off.
- No device instantiation: platform init code turns the parsed facts into
  platform devices; this crate returns plain data.
- No MMIO mapping: tables are read through the direct physical mapping
  (`kaddr_layout::PAGE_OFFSET`); mapping new regions is not this crate's job.

## Scope

```text
drivers/platform/acpi/
├── src/
│   └── lib.rs        # descriptors, table lookup, MADT/MCFG/DSDT/FADT parsing
└── Cargo.toml
```

## Architecture

Single-file crate in three layers:

```text
boot firmware -> init(rsdp_addr) -> LazyInit<AcpiDesc> (global RSDP slot)
                       |
        find_table* / find_madt* / find_mcfg / find_*_from_init
                       |
   validate_rsdp -> XSDT (rev >= 2) preferred, RSDT fallback
                       |
   validate_sdt_header (signature/length/checksum)
                       |
   typed reads: MADT byte parser | MCFG entries | DSDT byte scan | FADT offsets
```

Public data types (`AcpiDesc`, `AcpiTableHeader`, `MadtInfo`,
`McfgAllocation`, `PciHostMemWindow`, `Pm1aControlBlock`,
`LocalApicEntry`, `IoApicEntry`, `InterruptSourceOverride`, `MadtEntry`,
`MadtParseError`) are plain `Copy` views; raw firmware layouts (`Rsdp`,
`AcpiSdtHeader`, `McfgEntryRaw`) are private `repr(C, packed)` types with
compile-time size assertions. MADT entries are decoded from a validated
`&[u8]` with explicit length and structure checks rather than typed reads.

## Execution Context

- Strictly early boot: `init()` must be called by platform init with the
  firmware-passed RSDP address before any lookup; `*_from_init` helpers
  read the global slot afterwards.
- All table reads assume the firmware regions are reachable through the
  direct physical map (`addr_to_ptr` adds `PAGE_OFFSET` to addresses below
  it). No `memspace` device mapping is required or performed.
- No allocator, no threads, no scheduler dependency; callable from a single
  boot thread before kernel services exist.
- `find_pm1a_control_block_from_init` is deliberately reachable from the
  power-off terminal path after other services are torn down and must stay
  panic-free; the other lookups are one-shot boot-time calls.

## Concurrency Model

- One-shot initialization through `lazyinit::LazyInit`: the first
  `init()` wins, later calls with different addresses have no effect.
  Platform init calls it once on the boot hart before APs start.
- After initialization the global descriptor is read-only; all lookup
  functions take no locks and are safe to call concurrently once `init()`
  has completed.

## Error And Panic Model

Two deliberately different regimes:

- Fallible, `Option`/`Result`-valued: `init` returns
  `AcpiInitError::MissingRsdp` for a zero address; `desc`, `rsdp_addr`,
  `find_mcfg`, `find_pci_host_mem_window_from_init`, and
  `find_pm1a_control_block_from_init` return `None` when the described
  feature is absent or unparseable. `find_madt*` returns `None` when ACPI is
  uninitialized or the firmware has no MADT.
- Panicking, for boot-critical firmware corruption: `validate_rsdp` panics
  on a zero address, bad `"RSD PTR "` signature, or checksum failure;
  `find_table` / `find_table_from_rsdp` panic when the RSDP is missing or
  names neither XSDT nor RSDT, when an explicitly required table is absent,
  or when an SDT header is too short, mismatched, or fails its checksum.
  A MADT fixed header that is too short also aborts boot.
- MADT entry framing is neither panicking nor silently dropped: iteration
  yields `Err(MadtParseError { offset, kind })` for a truncated entry, an
  invalid length, or a known entry shorter than its fixed structure, then
  stops. Callers decide whether a malformed topology is fatal.

## Firmware Trust Boundary

Firmware tables are external input and are only partially trusted:

- RSDP: signature plus basic (20-byte) and extended (rev >= 2) checksums.
- Every SDT: header signature, minimum length, and full-table checksum
  before its body is parsed.
- MADT entries: per-entry length checked against the remaining region and
  against each known entry's fixed structure; unknown entry types are skipped
  by their declared length. Interrupt source override flags are returned raw,
  so trigger/polarity policy stays in the consuming architecture layer.
- PM1a ports: non-zero and `u16`-range checked; ECAM ranges are checked for
  `u64 -> usize` and bus arithmetic overflow (`ecam_region` returns `None`).
- The DSDT byte scan bounds its search windows (16 KiB `_CRS` search, 8 KiB
  resource scan) and validates item lengths before indexing.

What is *not* defendable here: the firmware choosing a wrong-but-valid
table (checksums pass, content lies). Residual risk is accepted; platform
policy decides whether absence (`None`) is fatal.

## Unsafe Code

All unsafe code is concentrated in private helpers at the bottom of
`src/lib.rs`, each with a `SAFETY:` comment:

- `addr_to_ptr` — maps a firmware address into the direct physical map.
- `ptr_from_addr` — forms a `&'static T` at a validated structure address.
- `bytes_from_addr` / `sdt_bytes` — form byte slices over validated ranges
  (RSDP length, header-declared table length).
- One `slice::from_raw_parts` call site in `parse_mcfg` — bounded by a
  header length validated by `validate_sdt_header`.

XSDT/RSDT scanning and MADT entry parsing use safe slices over the
validated table bytes.

The soundness contract is: addresses come from the firmware-provided RSDP
chain, every table header is checksum-validated before body access, and
`repr(C, packed)` layouts are pinned by compile-time size assertions.

## Design Decisions

- XSDT preferred, RSDT fallback for RSDP revision >= 2: matches the ACPI
  specification's deprecation of 32-bit entry addresses while keeping
  legacy firmware working.
- Panic on explicit required-table lookups, `None` for absence probes:
  a kernel that guesses CPU topology or ECAM location is worse than one
  that stops loudly. `find_madt*`, MCFG absence, and the DSDT window return
  `None` so callers can choose platform fallback, while entry framing
  corruption is surfaced through `Result` for the same reason.
- Panic-free power-off lookup: the PM1a path runs after the kernel is torn
  down, where a panic means a hung machine instead of a clean shutdown.
- Byte-scan instead of an AML interpreter for the PCI host window: QEMU and
  common firmware emit a stable `_CRS` layout; a full AML interpreter is
  out of scope, and the result is documented as advisory.
- Typed `Copy` snapshots instead of retained references: callers cannot
  hold firmware memory across mapping changes, and the crate keeps no
  state beyond the RSDP address.
