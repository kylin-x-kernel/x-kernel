# kvmm-api security

## Trust model

GIC virtualization setup trusts only an explicit active irqchip capability.
`device.rs::setup_gic` reads GICH_VTR through a device mapping of the provider's
validated control page. Constructing the shared `tock_registers::ReadOnly<u32>`
accessor is the unsafe boundary: the mapping must remain valid, the register
must be aligned and in bounds, and its reads must have no side effects. The
typed accessor performs the volatile read. Firmware
must correctly describe accessible hardware. Missing regions or insufficient
LR capacity return NoSuchDevice; fixed QEMU addresses are not a fallback.
The permanent host timer action is shared infrastructure, while residency and
its weak wake route belong to each vCPU. Preparing it does not enable delivery.

`/dev/kvmm-vm` and `/dev/kvm` are privileged control devices. Anyone able to
open them can create a VM that consumes host physical RAM and CPU time. Access
is governed by the devfs node permissions; the devices themselves perform no
capability check beyond that. Treat open access as equivalent to the ability to
run the VMM.

## Resource lifetime

New MMIO owner failures occur before VM publication or vCPU spawn and propagate
through the existing boot Result. The state mutex remains held and the instance
does not transition from Idle on failure. NOWAIT excludes reclaim for these
allocations; it does not promise bounded latency, NMI safety or nonblocking file
IO. VirtIO owners are reserved before setup, and GICD storage plus its outer
owner precede host MMIO mapping, so these new failures do not occur after GICH
mapping or require a new unmap rollback protocol. Existing failures after host
mapping, infallible nested allocations and hook/task creation are not repaired
by this change. Device-side DMA uses Weak VM references; this migration does
not change that ownership model. Unit tests cover prepared-owner dispatch and
drop, not forced allocator OOM or complete Linux guest boot rollback.

The central guarantee of this crate is that VM resources are released when the
owning file description is closed:

* `release` calls `Vm::stop_and_join`, which blocks until every vCPU kernel
  thread has left its run loop. Only then is the last `Arc<VmShared>` dropped,
  which runs the second-stage page table `Drop` and releases the guest RAM
  reservation held by the VM.
* Because teardown joins the vCPU threads synchronously, a closing process
  cannot leave detached vCPU threads still executing guest code against
  freed VM state.
* vCPU threads explicitly drop their local `Vcpu` before calling `ktask::exit`,
  so the task-exit path cannot leak the vCPU's `Arc<VmShared>` by bypassing Rust
  stack unwinding.

Guest RAM itself is reserved in `kvmm::mm::reserve_guest_ram` and held by the VM
through an RAII reservation handle. Closing `/dev/kvmm-vm` therefore releases
the reserved physical range before a later VM attempts to reuse the default
guest memory base. The guest-visible physical base is translated to an
allocator-chosen host physical backing range, so the control plane does not need
to reserve a hard-coded host physical address.

The `/dev/kvm` lkvm path uses a different ownership model: userspace owns the
RAM backing and passes page ranges with `KVM_SET_USER_MEMORY_REGION`. The kernel
validates slot alignment, guest range bounds, and that each userspace page is
currently mapped before installing stage-2 entries. This is enough for initial
lkvm bring-up, but it does not yet install mmu-notifiers or pin the user pages;
therefore `/dev/kvm` must remain privileged until page lifetime and invalidation
are implemented.

## Input handling

* The boot command is parsed from the first `write`. Malformed input returns
  `InvalidInput`/`NoSuchDevice` without booting; it cannot corrupt device
  state (the instance stays `Idle`).
* Guest image paths come from the writer and are resolved through the normal
  VFS. Loading is bounded by the control-plane loader's own validation
  (`kvmm-api::loader::load_image_to_guest`).
* After boot, `write` bytes are copied verbatim into the guest UART RX FIFO.
  The console push path injects the guest UART RX interrupt (registered via
  `set_console_irq`) so the guest driver drains the FIFO. The bytes and the
  injected interrupt are guest-visible only and cannot perturb host interrupt state. A writer can
  at most flood the guest with input, which the guest already controls by
  reading (or not reading) the UART.
* `read` returns guest UART output drained from the console TX channel. The
  channel is a bounded SPSC FIFO; the guest is the sole producer and the fd
  owner the sole consumer, so the direction carries only guest-authored bytes
  and cannot expose host memory. On overflow the newest byte is dropped rather
  than blocking the vCPU or growing unbounded, so a guest that produces output
  faster than userspace drains it can at most lose (truncate) its own output —
  it cannot exhaust host memory or stall the VMM.

## Known limitations (skeleton)

* No per-open resource limits: a caller can open the device repeatedly to
  create multiple VMs. Automatic boots use the architecture's default guest
  memory base, so a concurrent second VM without an explicit `@0xBASE` fails
  reservation instead of aliasing RAM. A production control plane should enforce
  quotas and a real address allocator.
* riscv64 and aarch64 are supported; other architectures reject boot.
* `/dev/kvm` currently targets the aarch64 lkvm subset. Unsupported KVM ioctls
  return errors; they are not silently treated as successful no-ops.
* `/dev/kvm` memslots map current userspace pages into stage-2 but do not pin
  those pages or install an mmu-notifier, so a userspace `munmap`/remap after
  registration is not observed. Explicit `KVM_SET_USER_MEMORY_REGION` deletion
  or replacement does revoke the old stage-2 range before returning. Restrict
  access to trusted callers until the memory-lifetime model is completed.
* The console TX FIFO is bounded and drops on overflow (see Input handling), so
  guest output under burst can be truncated. This is a fidelity limit, not a
  safety one; a production path wanting loss-free capture would add backpressure
  or a larger/spillable buffer.
