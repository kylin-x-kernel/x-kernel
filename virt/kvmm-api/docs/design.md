# kvmm-api design

## Purpose

AArch64 `setup_gic` obtains the active backend's guest-interface capability
before mapping hardware. It rejects absent GICH/GICV regions, prepares the
disabled timer route in sleepable context and checks that GICH exposes at least
four LRs. Only the two validated GICV pages are mapped at the guest GICC address.
The control-plane kernel fixture in `ut-tests/timer_guest.rs` exercises this
same setup with a finite EL1 program, real timer IRQ acknowledge/EOI and MMIO
migration checkpoints. It is not a mocked register test or a performance test.

`kvmm-api` is the control-plane (policy) crate for the VMM. The [`kvmm`] crate
provides only *mechanism* — `Vm`, `Vcpu`, guest memory, and the virtual-device
substrate — with no opinion on how a VM is created or destroyed. `kvmm-api`
supplies the devices that make those decisions, so that VM lifetime is easy to
reason about in one place.

Keeping policy out of `kvmm` means the mechanism crate has no global
lifecycle state to audit: a `Vm` lives exactly as long as some `kvmm-api`
owner holds it.

## Devices

### MMIO owner preparation

The boot path holds the per-open instance state mutex. New KBox allocations
therefore use NOWAIT rather than assuming reclaim can safely re-enter the
control device. This policy constrains owner allocation only; image loading,
device constructors and existing nested allocations retain their own contracts.
VirtIO net and block owners are reserved before VM setup, including host GICH
mapping. Their later constructors initialize those reservations without another
owner allocation. UART and PLIC owners are prepared before bus registration.
The GICD register buffer and distributor owner are both reserved before GICC/GICH
mapping; GICD construction itself only consumes and clears the provided storage.
Owner failure propagates as VfsError::NoMemory while the instance remains Idle,
before registry publication, CPU-on marking or vCPU task creation. Earlier
owner reservation may precede image-loading errors. Vec/Arc, guest pages and
world-switch hook allocations remain outside this migration.

### `KvmmVmDevice` — fd-bound VM instance (`/dev/kvmm-vm`)

The problem it solves: VM lifetime must be owned by a real userspace process,
not by a transient write command. Without an owning file description there is no
natural point at which to tear a VM down.

`KvmmVmDevice` ties one VM to one open file description:

| VFS event      | Action                                                                 |
|----------------|------------------------------------------------------------------------|
| `open`         | install a fresh idle `VmInstance` in the file's private data           |
| first `write`  | parse `bootlinux [kernel dtb initrd] [@0xBASE]`, boot the VM           |
| later `write`s | push bytes into the guest UART RX FIFO (console input)                 |
| `read`         | drain the guest UART TX channel (guest console output)                 |
| `release`      | request vCPU stop, join vCPU threads, drop the VM (frees guest memory) |

`VmInstance` holds `ksync::Mutex<InstanceState>` where `InstanceState` is
`Idle` or `Running(Vm<CurrentArch>)`. The mutex gives `open` a placeholder
that a later `write` fills in, without threading the VM through the file
builder.

The device relies on two mechanism features:

* `DeviceFileOps::release` (kvfs) — the close hook, forwarded from
  `VfsFile::drop` through the character-device `FileOperations` shim. Without
  it there is no place to tear a per-fd VM down.
* `Vm::stop_and_join` / `VmShared::request_stop` (kvmm) — the cooperative vCPU
  stop path. `request_stop` sets a flag checked at the top of the vCPU run loop
  and wakes any vCPU parked in WFI; `stop_and_join` then joins each vCPU
  thread. Only after every vCPU thread has exited does the last
  `Arc<VmShared>` drop, releasing the second-stage page table.
  The vCPU thread drops its local `Vcpu` before calling `ktask::exit`, because
  that exit path does not unwind the closure stack.

## Teardown ordering

```
release
  └─ Vm::stop_and_join
       ├─ VmShared::request_stop      (set stop flag, interrupt vCPU tasks)
       └─ for each vcpu_task: join()  (block until the thread leaves the loop)
  └─ *state = Idle                    (drop Vm → last Arc<VmShared> → GStage Drop)
```

A vCPU that never exits guest mode (a tight guest loop taking no traps) will
not observe the stop request until its next exit — a timer tick guarantees
this in practice for Linux guests.

## Scope

* riscv64 and aarch64 Linux boot are implemented. On other architectures the
  boot command logs and returns an error; the crate still compiles so the
  workspace builds on every target.
  The private `loader` module and its import use the same AArch64/RISC-V gate
  as these boot implementations; unsupported targets do not compile unused
  image-loading and DTB-patching helpers.
* This crate owns the Linux boot control plane, including image loading and DTB
  patching. The `kvmm` crate keeps only the VMM mechanism (`Vm`, `Vcpu`, guest
  memory, and virtual-device substrate).
* The `bootlinux` argument parse and default memory-base selection are shared
  across arches; only the `build_and_start_{rv64,aarch64}` VM-construction tails differ
  (console model, interrupt controller, and vCPU entry-register conventions).
  The guest-visible memory base is patched into the DTB; the host backing pages
  are allocator-chosen by `kvmm` and need not live at the same physical address.
* `read` drains the guest console TX channel. The channel
  (`vdev_vpl011::TxChannel`) is a bounded SPSC FIFO the boot path installs and
  enables per VM; the UART's guest-output path then forwards bytes into it
  verbatim instead of the host kernel log. The device is non-blocking and
  relies on the reader's `poll` (level-triggered on pending output); it does
  not inject a wakeup, so first-byte latency after an idle period is bounded by
  the reader's poll timeout.
* The global `VM_REGISTRY` (used by `/proc/kvmm`) still lives in `kvmm`. VMs
   booted through this device register there via `Vm::register`, so they appear
  in `/proc/kvmm`.

## Future direction

Kernel integration tests are selected by `kernel-tests` through kfeat, not by
production VMM selection. Their optional irq-driver dependency provides a
read-only GICv2 PPI probe: tests check physical active/enable/delegation state
after a withdrawn pending timer and after final stop, including an asserted,
guest-active source. The production control plane uses only KIRQ capabilities.
The fixture exercises two guests and CPU migration; it is not a performance
benchmark or a proof of every asynchronous wakeup interleaving.

* Add an event-driven console wakeup (`poll` wait queue woken on TX push) if
  the poll-timeout first-byte latency proves too coarse for interactive use.
* Add a KVM-compatible ioctl device (`kvm_device.rs`) alongside the fd-bound
  Linux boot device.
