# acpi — Security And Reliability

## Scope

This analysis covers the entire crate — the single `src/lib.rs`
(descriptor state, table lookup, MADT/MCFG/DSDT/FADT parsing, and the
four unsafe helpers). No modules are excluded; everything is reachable
from the public lookup functions.


## Trust Model

Firmware-provided ACPI tables are external, only partially trusted input:
the kernel must read them to boot, but a corrupted or hostile table must
not corrupt kernel memory. The crate's stance is validate-then-read:
signature and checksum validation gates every table body access, and
malformed *required* data aborts boot loudly rather than letting the
kernel proceed on guessed topology.

## External Boundaries

- The boot loader hands over an RSDP physical address (`init`). Validated:
  `"RSD PTR "` signature, 20-byte checksum, extended checksum for
  revision >= 2.
- Firmware table memory (RSDT/XSDT, MADT, MCFG, FADT, DSDT) is read
  through the direct physical map. Every SDT is validated (signature,
  minimum length, full-table checksum) before its body is parsed.
- The PM1a control block describes an I/O port the kernel will later
  write for power-off. The port is range-checked (non-zero, `u16`).
- The DSDT `_CRS` byte scan reads only within bounded windows
  (16 KiB `_CRS` search, 8 KiB resource scan) of the checksum-validated
  DSDT body.

## Unsafe Code

All unsafe code is in the four private helpers at the end of `src/lib.rs`,
each with a `SAFETY:` comment:

- `addr_to_ptr` — converts a firmware address to a pointer via
  `PAGE_OFFSET`. Assumes the address is inside the direct-mapped physical
  region.
- `ptr_from_addr::<T>` — forms `&'static T` at a validated structure
  address; callers pass checksum-validated table addresses only.
- `bytes_from_addr` / `sdt_bytes` — `slice::from_raw_parts` over
  validated ranges: the RSDP length or a header-declared table length.
- `find_table_xsdt` / `find_table_rsdt` / `parse_mcfg` — entry-array
  slices bounded by `validate_sdt_header`-checked table lengths.

The packed `repr(C)` firmware layouts are pinned by compile-time size
assertions (`Rsdp` = 36, `AcpiSdtHeader` = 36, ...).

## Invariants

- No table body is dereferenced before its header passes signature,
  length, and checksum validation.
- MADT iteration stays within `header.length`. A truncated entry, an
  invalid length, or a known entry shorter than its fixed structure
  yields one `MadtParseError` carrying the table-relative offset, after
  which iteration ends. The iterator never panics on entry framing; the
  consumer decides whether the error is fatal. Current CPU enumeration
  logs the error and keeps the entries parsed before it.
- A MADT fixed header shorter than its fixed structure still panics
  before entry iteration begins, together with the RSDP/SDT header checks
  above.
- The DSDT scan never indexes outside its bounded windows and validates
  resource-item lengths before reading fields.
- The PM1a port is never `0` and never exceeds `u16::MAX`; the ECAM range
  computation rejects `u64 -> usize` truncation and bus-range overflow
  (`None`, not wraparound).

## Threat Analysis

| ID | Threat | Impact | Trigger | Existing control |
|----|--------|--------|---------|------------------|
| T-01 | Corrupted RSDP (bad signature/checksum) | High — boot aborts | Firmware or loader corruption | `validate_rsdp` panics before any table is walked; the kernel does not continue with a bogus root pointer. |
| T-02 | Table claiming a length larger than reality | High — out-of-bounds reads | Corrupted or malicious SDT header | Header length participates in checksum validation; a forged length fails the checksum. `slice` ranges are built from the validated length only. |
| T-03 | Malformed MADT entry stream | Medium — topology truncated or boot aborts, depending on the consumer | Firmware bug in entry lengths | The iterator bounds-checks each entry, returns one typed error with the table-relative offset, then stops; it never reads past the table. CPU enumeration logs the error and keeps earlier entries; discovery reports `Malformed` and does not fall back. The fixed MADT header is separately length/checksum-validated and panics before iteration. |
| T-04 | Wrong-but-valid table content (lying topology) | Medium — wrong APIC/ECAM configuration | Compromised firmware | Not detectable here: checksums pass. Residual risk accepted; platform policy decides fatality of absence, not of lies. |
| T-05 | PM1a port pointing at an unintended device | Medium — spurious I/O write at power-off | Corrupted FADT | Port must be non-zero and `u16`-sized; the write itself happens in the power-off path where the system is already tearing down. |

## Failure Modes

| ID | Failure mode | Local effect | System effect | Severity | Handling |
|----|--------------|--------------|---------------|----------|----------|
| F-01 | `init` called with a zero RSDP address | `AcpiInitError::MissingRsdp` | ACPI absent; caller chooses fallback | 3 | Explicit `Result` at the boundary. |
| F-02 | Required table missing (MADT/FADT at `find_table`) | Panic during platform init | Boot fails fast | 2 | A kernel that guesses CPU topology is worse than one that stops. |
| F-03 | Optional feature absent (MCFG segment 0, DSDT window, PM1a) | `None` returned | Feature degrades; caller applies policy | 3 | `Option`-returning probe APIs. |
| F-04 | Later `init` call with a different address | Ignored (`LazyInit` first-wins) | No effect | 4 | Documented one-shot initialization. |

## Known Limitations

- The DSDT scan is a heuristic byte search, not an AML interpreter; exotic
  firmwares may hide the PCI host window from it. Results are advisory.
- Checksums protect against corruption, not against a malicious firmware
  that recomputes them.

## Audit Checklist

- Every new firmware structure read still goes through the four unsafe
  helpers (no ad-hoc pointer formation).
- New table parsers validate header signature, length, and checksum
  before body access.
- New bounds checks cover any added `repr(C, packed)` layout with a
  compile-time size assertion.
