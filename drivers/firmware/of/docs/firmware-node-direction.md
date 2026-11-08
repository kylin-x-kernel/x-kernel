# Firmware node migration direction

## Agreed ownership

rs_fdtree is the DT backend library, not merely a token reader. It owns
structural access, property decoding, node status and DT address translation.
GIC/PLIC controller-specific register roles and interrupt encodings belong to
their drivers. No kernel registration, IRQ mapping, MMIO mapping or DMA
allocation belongs in the DT library.

A future fwnode contract sits above backends. Its public handles, property
operations, parent/child relations, availability and parameterized references
must not expose FdtNode, LinuxFdt or DT cell layout. Resource interpretation is
separate from ordinary properties; resource acquisition remains in the existing
OS provider contracts. The initial real consumer can be DT only. Do not invent
ACPI support or a dynamic backend registry to justify the interface.

Existing OF is transitional integration, not that generic contract. Move DT
interpretation into rs_fdtree, retain boot DTB ownership/initialization in host
integration until migrated, and move device-specific policy to drivers.
Replace OF callers incrementally, then remove OF; do not rename the current
crate and claim that a generic node abstraction exists.

## Current correction

The scoped change moves status and numeric reg/ranges interpretation to
FdtNode methods implemented inside rs_fdtree. Address collection is gated by
the optional alloc feature; borrowed structural/status access remains usable
without allocation. Host tests exercise the same public methods, including
inclusive maximum-address bounds. GIC consumes these DT backend methods.
It still uses a DT-specific private discovery adapter; backend-independent
driver discovery is a later change, not an accomplished claim here.

Remove the moved OF implementations and forwarding entry points together.
No duplicate interpreter or new fwnode wrapper is retained. Existing IRQ/PCI
or boot helpers not included in this slice remain separately tracked debt.
PR #1291 must contain only this DT capability move and its GIC consumer, not
the full firmware-node framework.

Implementation order: update and validate the full driver development branch,
then the OF/IRQ development branch, then extract the validated delta to the
independent PR branch. Preserve each branch's unrelated capabilities; do not
replace directories from one branch with another.

## Validation of this extraction

The correction was first validated on both development branches: AArch64
selected guest suites passed 200 cases (six ignored) and 129 cases (five
ignored), respectively. The intermediate branch also passed 44 RISC-V cases.

On this independent PR branch, parser host tests passed without alloc (25)
and with alloc (34, including nine address cases). AArch64 make clippy
UNITTEST=y passed; selected OF/GIC guest tests passed 47 cases and exported
coverage. The ten status/address cases now run on the host rather than in
the guest, so guest totals are not directly comparable to the earlier PR.
GICv3 boot is exercised; GICv2 remains fixture-only coverage.
The independent branch also passed UNITTEST-enabled Clippy on RISC-V, x86_64
and LoongArch; this is compile validation, not guest execution on those targets.
Read-only review found no code blockers.

That extraction landed without a generic fwnode API. The first fwnode slice is
recorded below.

## Implemented neutral contract (first slice)

`drivers/firmware/fwnode` defines the backend-neutral contract:
`FwNodeId`, opaque `FwNode<'a>` handles over a `FwNodeOps<'a>` provider,
`FwProperty`/`FwPropertyValue`, `FwMmioResource`, and
`FwIrqSpec`/`FwParams`. Handles are comparable across backends and carry no
`FdtNode`, `LinuxFdt` or cell-layout types.

`rs_fdtree` implements the contract as `DtBackend` behind its optional
`fwnode` feature. Structure, availability, compatible strings and translated
MMIO resources come from the existing strict DT interpretation. Generic DT
interrupt translation is still `Unsupported`: it moves into the backend only
when controller `xlate`/`irq_domain` work makes the mapping well defined.
Existing `of` interrupt helpers remain in place for other consumers.

`khal::firmware` publishes the root after `of` validates and installs the DTB:
`fwnode_root() -> Option<FwNode<'static>>`. Platform bring-up no longer reaches
into `of` for GIC discovery; `kplat-aarch64` passes this root to
`irq_driver::gic::init_from_fwnode`.

The controller keeps its own common output rather than exposing firmware
types. `GicConfig` now retains multiple GICv3 redistributor regions and
`redistributor-stride`; `GicConfig::from_fwnode` is the DT producer, and
`from_madt` (ACPI) and `from_static` (board table) are the planned peers that
feed the same `gic::init(config)`. The vendored GIC driver still supports only a
single contiguous redistributor frame array with architectural stride, so
`init` validates and rejects other layouts instead of misprogramming them.

Not included in this slice: an ACPI fwnode backend, device-to-controller IRQ
lookup through `FwIrqSpec` (still `Unsupported` in `DtBackend`), a dynamic
`irq_domain`, and multi-region/custom-stride redistributor programming.
