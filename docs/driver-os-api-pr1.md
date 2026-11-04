# PR1: replace device-res with driver-os-api

## Baseline and requirements
Main baseline: b8b1bc3c3. Development reference: 2c90f18d8.
Extract selected responsibilities, not the entire development branch:

- One capability contract crate and host adapter; delete device-res and
  device-res-xkernel, migrating every in-tree consumer and dependency.
- Preserve MMIO/port, DMA/typed coherent buffers, fallible CPU allocation,
  ordinary IRQ/MSI and monotonic time capabilities.
- Keep resource inventories and managed cleanup in kdevice.
- Share only IRQ value definitions with kirq, never driver lifecycle.
- Preserve current mainline discovery, binding and block claim/sector-I/O.
- Exclude new model/class/RNG routing, NativeIrq, transport drivers, per-device
  DMA policy, network shutdown and MSI owner retry redesign.

## API changes
Io becomes IoMap, DmaCoherent becomes CoherentMem, Irq becomes DevIrq.
Resource variants distinguish plain virq from domain-mapped origin; explicit
domains must not be overwritten by root fallback. Remove unused IRQ source
bitmap but preserve device ack and RX scheduling.
Remove unused ResourceProvider and DeviceResource indirection. Framework helpers
use DeviceObject directly. Guard cleanup retains the original provider/token.
Threaded defaults still return Unsupported; MSI rejection is not claimed fixed.
An unmatched IRQ release (Ok(None)) can no longer authorize reclamation: the
infallible provider fails closed. This and explicit-domain preservation are
intentional contract corrections, not KIRQ core changes. The provider design
document lists their mainline-to-PR behavior and tests.

## Review and validation
Initial Standards review found one nonblocking simplification: external hook
constructors had no production consumers. Accepted: omit those constructors and
unused shared-provider IRQ acquisition; keep one provider-backed ownership path.
The guard tests now verify exact domain/hwirq/virq/token release through that path.
Final independent Standards re-review: no remaining findings; the simplification
is closed. Final independent Spec re-review: no blockers or missing requirements.

Validation on the final simplified code:
- AArch64 SMP4 guest UT: 381 passed, zero failures, zero ignored; coverage exported.
- Selected crates: driver_os_api, driver_os_api_xkernel, kdevice, kdriver,
  virtio, block and kirq. Mainline exclusive claim, failed-open rollback,
  same-holder reopen and 4-KiB logical-sector tests passed.
- AArch64 and x86_64 UNITTEST clippy passed, including dependency and layout checks.
- Restored AArch64 configuration. Separate disk.img, no networking or vsock.
- block src/lib.rs and ut-tests/lib.rs remain byte-for-byte equal to b8b1bc3c3.
- Source, manifests and lockfile have no dependency on the retired crates.

The initial compile found remaining bitmap assertions in virtio-net UT; these
now assert handled/unhandled while retaining RX scheduling-count checks.
The initial 382-test run preceded removal of one hook-only test; 381 is the final
count. Local logs: .scratch/pr01-extraction/{aarch64-clippy-final,aarch64-ut-final,
x86-clippy}.log. Runtime hardware acceptance on x86_64 is not claimed by clippy.
No remote PR is created by this extraction.
