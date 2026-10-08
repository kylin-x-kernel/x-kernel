# pci — Security And Reliability

## Scope

This analysis covers the entire crate: `src/lib.rs` (config-space
resolution, `PciBus`, `PciConfigAccess`, BAR allocation, INTx routes)
and `src/msix.rs` (MSI-X capability support). No modules are excluded;
all MMIO access is confined to the audited unsafe entry points.


## Trust Model

Two untrusted inputs meet in this crate: PCI configuration space contents
(hardware-controlled) and the firmware description (boot-provided). The
crate validates hardware-derived data before use, trusts the firmware
description as boot policy, and confines all raw MMIO access to the one
ECAM window mapped through `memspace`. MSI-X table contents are device
memory and are treated as untrusted until layout validation passes.

## External Boundaries

- **Configuration space reads**: header fields (BAR sizes, interrupt pin,
  MSI-X capability structures) are device-controlled. Validation: BAR
  sizes checked before allocation; MSI-X `table_size` capped at 2048;
  table/PBA offsets range-checked against their BAR sizes
  (`validate_msix_layout`); interrupt pin `0`/`0xff` treated as
  unprogrammed.
- **MSI-X table writes**: the kernel writes message addresses/data into
  device-owned MMIO. That is an intentional device programming boundary;
  only `memspace`-mapped windows are touched.
- **Firmware description** (`khal::firmware::devices`): ECAM base/size,
  bus range, BAR range, INTx routes. Trusted boot data; a CAM-kind
  mismatch with the build-selected `Cam` falls back to static
  configuration with a warning rather than proceeding on mixed geometry.
- No user-space input reaches this crate.

## Unsafe Code

All unsafe usage is volatile MMIO access over the mapped ECAM window and
MSI-X tables, each with `SAFETY:` comments:

- `PciBus::new` / `new_static` — `MmioCam::new` and
  `PciConfigAccess::new` over the `memspace::iomap_device` window;
  safety rests on the full-window mapping and program-lifetime retention
  of the mapping.
- `PciConfigAccess::new` (`pub unsafe fn`) — documented `# Safety`:
  valid, 4-byte-aligned, program-lifetime MMIO mapping sized for every
  later BDF access, and no aliasing abstraction on the same window.
- `read_word` / `write_word` — volatile u32 access at cam-computed offsets;
  `write_halfword` — aligned volatile u16 access within the selected word.
  All are bounded by CAM geometry (bus shift 8/12 per access width).
- `msix::MsixTable::new` (`pub unsafe fn`, x86_64) — documented `# Safety`:
  mapped table with `len` valid entries, unique ownership of table
  register accesses, lifetime coverage.
- `configure_msix_entry` — volatile writes plus a read-back flush of
  `msg_data` to order the write against the device.

No FFI or inline assembly exists in the crate.

## Protected Resources

- The mapped ECAM window ("pci-ecam" mapping): the only memory this crate
  may dereference. `PciConfigAccess` offsets are geometry-bounded.
- `PCI_CONFIG_BASE_OVERRIDE` / `PCI_BUS_END_OVERRIDE` (`Relaxed`
  atomics): configuration override installed before bus init; single
  writer, no torn reads (aligned u64/u32).
- The BAR allocation window (`PciRangeAllocator`): bump-allocated with
  power-of-two natural alignment and checked arithmetic — no overflow-
  induced aliasing of device MMIO ranges.

## Invariants

- Every config-space byte accessed lies inside the mapped ECAM window.
- BAR assignments never overlap: the allocator is sequential, aligned,
  overflow-checked, and allocations are committed only after the range
  fits.
- MSI-X enable is only set after layout validation succeeds; a failed
  validation leaves the capability disabled.
- `configure_device` assigns only BARs whose current address is 0
  (unassigned), never reassigning firmware- or firmware-policy-placed
  addresses.

## Threat Analysis

| ID | Threat | Impact | Trigger | Existing control |
|----|--------|--------|---------|------------------|
| T-01 | Malicious device lying about BAR sizes | Medium — wrong MMIO window sizing | Device header reports bogus sizes | Sizes validated before allocation (`alloc_buf` rejects 0/non-power-of-two/overflow); allocation failures surface as `PciInitError::NoMemory`. |
| T-02 | Malicious device with corrupt MSI-X capability | Medium — writes to arbitrary device MMIO via table pointer | Table/PBA offsets outside their BARs | `validate_msix_layout` range-checks offsets against BAR sizes and rejects; enable bit stays clear on failure. |
| T-03 | Config-space access beyond the mapped window | High — kernel memory corruption via MMIO | BDF/register arithmetic overflow | `cam_offset` builds bounded offsets from the CAM geometry; word offsets are masked to 32-bit alignment and halfword writes add only the selected in-word offset; `unsafe` contracts require a window sized for all accesses. Residual risk sits in the `PciConfigAccess::new` contract. |
| T-04 | Firmware/build CAM mismatch | Medium — garbage config accesses | Firmware CAM kind differs from build-selected `Cam` | Cross-check at `PciBus::new` with warning and fallback to static configuration. |
| T-05 | ECAM base from runtime override pointing at non-ECAM memory | High — MMIO corruption of unrelated regions | Operator error via `set_pci_config_space` | Override is a bring-up/debug facility requiring explicit action; mapping still goes through `memspace` which owns region sanity. Residual risk accepted as a debug feature. |

## Failure Modes

| ID | Failure mode | Local effect | System effect | Severity | Handling |
|----|--------------|--------------|---------------|----------|----------|
| F-01 | No config space (`PCI_ECAM_BASE=0`, no firmware host) | `(0, 0, Static)` / `PciInitError::InvalidRange` | PCI skipped entirely; boot continues | 3 | Explicit skip path with logging. |
| F-02 | ECAM mapping fails | `PciInitError::MappingFailed` | PCI bus unavailable | 3 | Error mapped from `memspace::IoMapError`, logged with the failing range. |
| F-03 | BAR allocator exhausted | `PciInitError::NoMemory` | Device skipped | 3 | Checked, logged. |
| F-04 | Missing BAR allocator in `configure_device` | Panic ("No memory ranges available for PCI BARs!") | Boot fails | 2 | Platform configuration error; failing fast is intentional. |
| F-05 | Unprogrammed INTx pin | `legacy_interrupt_route` returns `None` | Device falls back to MSI-X or stays IRQ-less | 4 | Logged `Option`. |

## Known Limitations

- Only MMIO configuration spaces; x86 I/O-port CAM is unsupported.
- `configure_msix_entry` is x86_64-only; other architectures cannot
  program MSI-X entries through this crate yet.
- The MSI-X entry write is a direct device programming boundary; a device
  may still ignore or misinterpret it, which is not detectable here.

## Audit Checklist

- New config-space reads validate before use (sizes, offsets, caps).
- New unsafe blocks stay inside the memspace-mapped window and cite the
  mapping in their `SAFETY:` comments.
- MSI-X paths keep the validate-before-enable ordering.
