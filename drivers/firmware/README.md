# Firmware Description Layers

This directory groups firmware contracts and description backends. Location
does not imply a common runtime or require every early table consumer to use
a firmware node.

| Crate | Responsibility |
| --- | --- |
| `fwnode` | Backend-neutral node identity, properties, references and resource declarations. It does not own controller families, OS policy or hardware operations. |
| `rs_fdtree` | DT structure and property decoding, availability, address translation and reference semantics. Its optional `fwnode` backend remains an internal module. |
| `acpi` | ACPI table access and firmware semantics. Future namespace evaluation and PCI routing interpretation belong here, not in concrete device drivers. |
| `of` | Transitional boot DT ownership and compatibility entry points. Moving this crate does not complete its migration; do not introduce new dependencies on its legacy controller decoding. |

## Dependency Direction

Description backends implement the `fwnode` contract; `fwnode` must not depend
on those backends or on host-kernel implementations. Consumers use the neutral
contract where available, or a deliberately scoped firmware-table interface.
Controller-specific binding interpretation stays in controller drivers.

Firmware contracts and parsing must not acquire dependencies on `driver-os-api`,
`pci`, `irq-driver`, an ACPI OSL implementation, or concrete device drivers.
They do not register devices, allocate virqs, map device resources or run probes.
Existing boot integration in `of` and `acpi` is retained by this directory-only
migration, not recast as a new portable abstraction.

## Host Services and Integration

An ACPI interpreter's OS Services Layer (OSL) belongs in the X-Kernel host
adapter, not in the PCI routing parser. It adapts existing OS capabilities and
PCI access mechanisms to the interpreter's host interface. Reuse
`drivers/base/os-api` where its contracts fit; add missing general capabilities
there only when justified, rather than creating another OS API crate.

ACPI-specific host callbacks and failure handling remain implementation details
of that adapter. Firmware semantics decide what results mean; the adapter
provides valid access and execution mechanisms. Boot integration owns ordering
and publication of firmware roots. No empty OSL crate is introduced here.

Third-party interpreters remain under `vendor/` and follow the repository's
vendor provenance and patch policy. Device discovery and driver lifecycle stay
outside this directory.
