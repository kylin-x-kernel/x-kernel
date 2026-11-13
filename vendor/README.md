# Vendored Dependencies

Vendored packages retain their upstream licenses, manifests and formatting.
They are excluded from the kernel workspace, not treated as first-party
workspace members. Kernel consumers still pass the normal build and lint gates.
Do not apply kernel copyright headers or repository-wide refactors to them.

[sources.toml](sources.toml) records, per package, the upstream source
(registry package or repository revision) and the SHA-256 of the upstream
archive. Update it in the same change whenever a vendored package is imported
or upgraded. xreview may skip files verified identical to the recorded upstream
archive; local patches are reviewed for defects only.

## arm-gic-driver (Locally Maintained Dependency)

- Published version: 0.17.13
- Upstream: https://github.com/rcore-os/tgoskits
- Package path: `drivers/intc/arm-gic-driver`
- Source revision: `8fdfcc9e7b57002d4ec3c48ccceb5acc5f24ed83`
- Archive SHA-256: `52210fc25fa222377dfabf2ffd2f07a751debe75d35c173ac14def04525541a5`

The import preserves the published archive byte-for-byte. The manifest declares
MIT, while its README declares Mulan PSL v2 and links a missing package LICENSE;
the upstream repository root at that revision carries Apache-2.0. These
inconsistent declarations are preserved, not resolved or relicensed here.
This is the maintained kernel dependency, not a temporary prerequisite awaiting
an upstream merge. Keep local patches separate from the recorded import, review
updates against that baseline, and run the regression gate below on changes.
An upstream release may replace a local patch only after equivalent behavior
and tests have been verified; upstream merging is not an acceptance condition.

License attribution remains unresolved independently of that maintenance policy.
The conflicting declarations cannot be settled by choosing a license locally.
Preserve them pending authoritative clarification; do not label distribution
attribution as settled. The source evidence, rechecked on 2026-11-03, is:

- [Package manifest (MIT)](https://github.com/rcore-os/tgoskits/blob/8fdfcc9e7b57002d4ec3c48ccceb5acc5f24ed83/drivers/intc/arm-gic-driver/Cargo.toml).
- [Package README (Mulan PSL v2)](https://github.com/rcore-os/tgoskits/blob/8fdfcc9e7b57002d4ec3c48ccceb5acc5f24ed83/drivers/intc/arm-gic-driver/README.md).
- [Repository-root license (Apache-2.0)](https://github.com/rcore-os/tgoskits/blob/8fdfcc9e7b57002d4ec3c48ccceb5acc5f24ed83/LICENSE).
- The package-local `LICENSE` is absent from the imported archive; its raw
  source URL at the same revision returns HTTP 404. The root license alone does
  not resolve the contradictory package-specific statements.

The upstream working clone is local-only and is not a nested Git submodule in
the kernel repository.

The local patch exposes `v3::Gic::wait_for_distributor_rwp()` and the typed
`DistributorRwpTimeout` error. It reuses the existing 10,000-poll bounded wait
without changing initialization or register-write policy. RWP only tracks
architecturally specified Distributor writes; success is neither a general
configuration barrier nor an active-interrupt/handler drain guarantee.

`v3::CpuInterface::wait_for_redistributor_rwp()` separately exposes the existing
bounded GICR wait with `RedistributorRwpTimeout`. It targets the Redistributor
retained by that interface and polls GICR_CTLR bit 3, not the Distributor's bit
31. It does not clear pending state or certify handler completion. CPU-local
callers must exclude migration and serialize the complete control transaction.
The fixture checks timeout, recovery, frame selection and unchanged control
registers; it does not emulate hardware propagation or physical NMI delivery.

Run `make check_gic` on a native AArch64 Linux host; the AArch64 Build Artifact
CI leg runs this gate. On an x86_64 host with AArch64 musl Rust std, the cross
linker and QEMU user emulator installed, use:

```sh
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-gnu-gcc \
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUNNER=qemu-aarch64 \
make check_gic GIC_TEST_TARGET=aarch64-unknown-linux-musl
```

The register fixture verifies stuck-RWP failure, recovery after clearing RWP,
and preservation of CTLR. It does not emulate hardware write propagation.

## UART Dependencies

The UART imports preserve the published archives byte-for-byte, separately
from local patches. They remain outside the kernel workspace and retain their
upstream licenses. Rebase local changes onto a new published version and remove
the overrides once upstream supplies the required masked initialization API.

| Package | Upstream revision | Archive SHA-256 | License |
| --- | --- | --- | --- |
| [arm_pl011 0.1.1](https://github.com/arceos-org/arm_pl011) | d4e6caedb617ab9774f452f2b7658e77ad4c6c97 | e32b9dcf7e8b53378fa5eff69bc4a9430d69ffea312170c1ce2b1eaec5ee18b9 | GPL-3.0-or-later OR Apache-2.0 OR MulanPSL-2.0 |
| [uart_16550 0.4.0](https://github.com/rust-osdev/uart_16550) | 9a46f42061a1455c3f5fe206e613ca9071e63dfd | 94d293f51425981fdb1b766beae254dbb711a17e8c4b549dc69b9b7ee0d478d5 | MIT |

Local patches add `init_masked` and `set_receive_interrupt` to PL011 and the
I/O-port 16550 backend. Existing `init` still ends with RX notification enabled.
The masked path never enables RX notification; activation does not clear the
FIFO. The 16550 setup selects IER (clears DLAB) before masking it. Register write
sequences are shared with host fixtures; no kernel dependency is introduced.
The uart_16550 MMIO backend is unchanged and unused by console-driver.

Run `make check_uart` on an x86_64 Linux host. CI executes it alongside
`check_x2apic` in the native x86_64 Build Artifact stage. Kernel console tests
also check masked construction and non-destructive activation through the real
SerialPort adapters. Host fixtures do not prove delivery of physical UART IRQs.

## x2apic

- Published version: 0.5.1
- Upstream: https://github.com/kzhao0986/x2apic-rs
- Source revision: b8f989f781f44ba9a2e504c55dba2f2d526590f5
- Archive: x2apic-0.5.1.crate
- Archive SHA-256: e64dd21b207e918377ce79266b54517c75262031948f06324b6ef8473d3294cc
- License: MIT OR Apache-2.0; original license files are retained.

The import commit preserves the published archive byte-for-byte. Local changes
belong in separate commits. When updating, compare against that baseline,
reapply only required patches, and rerun dependency tests plus x86 kernel
build, Clippy and xAPIC/x2APIC boot tests. Remove the patch override once an
upstream release supplies the required behavior.

### Local Patch

`LocalApic::enable_masked()` first masks LVTERR with a valid error vector,
then prepares a stopped one-shot timer and masks both LINT pins without
changing their routing. `enable_error()` activates error reporting separately
after the host publishes its APIC handle and has a ready fixed-vector handler.
Neither operation alters `enable()` or
introduce any kernel dependency. The register-write observer exists only in
host tests and checks the entire write sequence, not just its final state.
This is not a drain of firmware-pending interrupts or a reset of other LVTs.

Run the dependency regression on an x86_64 Linux host:

```sh
make check_x2apic
```

Do not replace this with only a successful kernel boot: boot alone cannot
detect a temporary unmask followed by a later mask.

CI runs this target in the normal x86_64 Build Artifact stage on a native
x86_64 agent. Other architectures do not execute this architecture-specific
host binary. The standalone lockfile pins x86_64 to 0.15.4, matching the kernel;
this is a local change separate from the pristine archive import.
Kernel unittest boot checks additionally verify the caller's prepared hardware
state before activation; host tests alone cannot detect the host choosing the
wrong initialization API.

`LocalApic::acknowledge_error()` supplies the separate APIC error protocol:
refresh ESR (subject to the legacy maximum-LVT guard), read status, and EOI
once. The host register-access tests verify this order and the legacy branch;
they do not emulate the hardware mechanism that latches physical error bits.

## acpi

- Published version: 6.1.1
- Upstream: https://github.com/rust-osdev/acpi
- Source revision: `ae81c54b468decc1cb13f877fa2652ab5115426a`
- Archive SHA-256: `b763acdc1d85c36d61acf97a59938f23202d0e8efe45e8759de10c02db242744`
- License: `MIT/Apache-2.0` (MIT OR Apache-2.0); original license files are
  retained.
- Edition: 2024. Default features: `alloc`, `aml` (`aml` implies `alloc`).

The import commit preserves the published archive byte-for-byte. The archive
was independently re-fetched from crates.io and re-hashed on 2026-11-08; the
SHA-256 above is the re-fetched value, not a copy of the previously cached
archive. Crate consumers must alias it (`acpi-rs`) because the workspace
already has an internal package named `acpi`. The crate is `#![no_std]` with
`#![feature(allocator_api)]`, so it needs `RUSTC_BOOTSTRAP=1`; the repository
`.cargo/config.toml` already sets that for all workspace and host test builds.

### Source Review

The local ACPI follow-up adds `sdt::borrowed`, an allocation-free strict
slice decoder used by the kernel's early table path. It owns RSDP/root
validation and MADT/MCFG/FADT/address-descriptor formats; the kernel supplies
address accessibility and selects failure/compatibility policy. It reuses
the vendor header and GAS types without requiring full-sized typed mappings
for short tables. This does not silently change or claim to harden all older
typed `AcpiTables` paths. Regression coverage also runs without default
alloc/AML features. The PM1a GAS-offset fix and stricter partial-entry checks
are behavior changes, not merely file movement.

The imported tree was reviewed line by line before integration. Relevant
findings, to be accounted for by the consumer:

- `src/lib.rs` accepts the RSDT/XSDT length from firmware when enumerating
  tables without a minimum header-length check, and `SdtHeader::validate`
  computes the checksum over the header-declared length, which the caller must
  have mapped. `table_entries` also assumes a region at least as large as
  `SdtHeader`.
- `src/sdt/srat.rs` bounds entries by the table's remaining length, but not by
  the concrete entry type's size, and a zero-length entry does not advance the
  cursor.
- `src/aml/mod.rs` has unchecked arithmetic on firmware-supplied offsets
  (`region.base + offset`, AML package lengths, multiplication for field
  accesses), panic paths for unexpected access widths/object types,
  `unimplemented!`/`todo!` for Bank/Index fields, and `unsafe` method/table
  parsing over raw table bytes. The AML stream reader itself bounds-checks
  `next`/`peek`, but package decoding is not uniformly length-checked.
- `src/aml/pci_routing.rs` indexes `pin_package[0..3]` without checking the
  package length, and direct GSI entries are interpreted as level/active-low.
- `src/aml/resource.rs` uses `split_at` on declared lengths, has
  `unimplemented!` descriptor variants, and `extended_interrupt_descriptor`
  asserts a single interrupt.
- `src/address.rs` maps a fixed 4 KiB SystemMemory GAS and `todo!()`s when the
  access width cannot be decoded; unsupported address spaces are
  `unimplemented!`.
- No file, network, process, or environment access exists. The only compile
  time input is `env!("CARGO_PKG_VERSION")`.

The AML consumer must treat firmware tables as untrusted, pre-validate table
sizes, and avoid the panicking `_PRT`/`_CRS` helpers noted above until their
inputs are validated. The same constraints apply to future updates.

### Local Patches

Upstream 6.1.1 omits the OperationRegion base from PCI-config accesses. The
patch adds `region.base` to the offset in the PciConfig branch of both
`do_native_region_read` and `do_native_region_write`, so a link `_CRS` field
over OVMF's `PIRQ` region is read at the declared offset instead of offset 0.
Upstream fixed this in `7.0.0-pre1`
(`386035c1badb74e8bd5f3a63b385e77c60d79b1f`) as
`region.base as u16 + offset as u16`. Re-check on the next stable release and
remove the override when equivalent behavior is available.

The same tree also carries the host-safety prerequisites for the kernel OSL:

- `Handler::map_physical_region` is fallible
  (`Result<PhysicalMapping<Self, T>, AcpiError>`), adding
  `AcpiError::HostMappingFailed`. Table iterators and RSDP search skip a
  region that cannot be mapped; explicit AML table loads return the error.
  A host never fabricates a mapping or a shared scratch buffer.
- Table enumeration, RSDP/RSDT mapping, AML table loading and FACS access use
  `ReadOnlyPhysicalMapping`, which implements `Deref` but not `DerefMut`.
  Its private fields are set only through the unsafe
  `ReadOnlyPhysicalMapping::new`, whose contract is valid, shared and readable;
  it neither requires writability nor exclusive access, so a header mapping
  and the later full-table mapping may legally overlap. `Handler` has a
  separate required read-only mapping entry and a matching unmap entry, so it
  cannot fall back to the writable constructor. `PhysicalMapping` remains
  writable for GAS and other hardware regions; its private fields are set only
  through the unsafe `PhysicalMapping::new`, whose contract covers pointer
  validity, mapped length, writability and reference exclusivity. Exposing a
  safe field constructor would let safe code turn a raw pointer plus a handler
  clone into a writable `DerefMut` target. The OSL's writable entry accepts
  only ACPI reclaim/NVS memory; firmware-reserved memory is read-only and is
  accepted only by the read-only entry. Exclusivity is enforced by the host,
  not asserted in a comment: `AcpiOs` retains the `T` object range of each live
  writable mapping, rejects a second writable mapping that overlaps it, rejects
  writable ranges over read-only table ranges, rejects read-only ranges over
  live writable objects, and releases the object range from
  `unmap_physical_region`. The vendored writable call sites are `MappedGas`,
  which keeps its mapping private and uses volatile raw accesses instead of
  `DerefMut`, and `wake_aps`, which holds a single local mailbox mapping that is
  dropped before returning.
- Native `SystemMemory`/`SystemIO`/`PCI_Config` region accesses validate the
  full `base + offset + width` and the address-space bound before narrowing
  to the handler's address type; a rejected access returns
  `AmlError::RegionAccessOutOfRange` without calling the handler.

Run the dependency regression on an x86_64 Linux host:

```sh
make check_acpi
```

The fixture evaluates an AML `_CRS` method backed by a PCI-config Field and
checks that the fake `Handler` observes the region base as the PCI-config
offset (and the write path does the same). It also evaluates a `SystemIO`
field at the top of the 16-bit port space and checks that an access past the
end returns `RegionAccessOutOfRange` without reaching the handler. The PCI
fixtures must fail against the pristine import and pass with the patches; a
successful kernel build alone does not exercise these paths.

## kvirtiodrivers

- Published version: 0.13.5
- Upstream: https://github.com/727Hsj/kvirtiodrivers.git
- Imported branch: `kmain` (rebased on the upstream virtio-drivers line)
- Source revision: ab99294a6860704142d3b18a9a4884349adceede
- License: MIT; original license file is retained.

Local changes belong in separate commits. When updating, re-import the target branch, reapply only required patches, and rerun the kernel virtio driver build and boot tests. Remove the
patch override once an upstream release supplies the required behavior.
