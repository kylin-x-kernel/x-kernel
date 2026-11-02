# Virtual Device Core

This crate supplies shared MMIO device interfaces, an owning MMIO bus, guest DMA
and interrupt/wakeup interfaces, and bounded console channels. Concrete devices
and VMM control-plane policy remain outside this crate.

## MMIO ownership

`MmioBus` stores `KBox<dyn MmioDevice>` in a Vec. `register` transfers a prepared
owner; `VmDevices::register_mmio` takes the bus mutex only after the caller has
prepared it. These functions do not allocate the device owner themselves, but
Vec growth still uses its existing infallible allocation behavior. Dropping the
bus drops every registered device. Allocation policy and constructor failures
belong to the caller; dispatch does not inherit the owner's allocation flags.

Guest reads and writes dispatch through the first matching MMIO window, with
the current vCPU identifier supplied to the device. IRQ, DMA and console state
remain independent capabilities; this owner migration changes no range policy,
console synchronization or device execution contract.

Kernel integration tests in `kvmm/ut-tests/vdev.rs` cover KBox trait-object
dispatch, transfer and single destruction, alongside the existing device test.
