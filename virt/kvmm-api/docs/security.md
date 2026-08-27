# kvmm-api security

## Trust model

`/dev/kvmm-vm` is a privileged control device. Anyone able to open it can
create a VM that consumes host physical RAM and CPU time. Access is governed
by the devfs node's permissions; the device itself performs no capability
check beyond that. Treat open access as equivalent to the ability to run the
VMM.

## Resource lifetime

The central guarantee of this crate is that VM resources are released when the
owning file description is closed:

* `release` calls `Vm::stop_and_join`, which blocks until every vCPU kernel
  thread has left its run loop. Only then is the last `Arc<VmShared>` dropped,
  which runs the second-stage page table `Drop`.
* Because teardown joins the vCPU threads synchronously, a closing process
  cannot leave detached vCPU threads still executing guest code against
  freed VM state.

Guest RAM itself is reserved in `kvmm::mm::reserve_guest_ram`, whose current
implementation does not hold the reservation past the call (a pre-existing
mechanism-crate behaviour). This crate does not change that; it only ensures
the page-table and vCPU-thread resources it introduces are reclaimed.

## Input handling

* The boot command is parsed from the first `write`. Malformed input returns
  `InvalidInput`/`NoSuchDevice` without booting; it cannot corrupt device
  state (the instance stays `Idle`).
* Guest image paths come from the writer and are resolved through the normal
  VFS. Loading is bounded by the control-plane loader's own validation
  (`kvmm-api::loader::load_image_to_guest`).
* After boot, `write` bytes are copied verbatim into the guest UART RX FIFO.
  The guest console runs in polled mode, so the guest's own poll timer drains
  the FIFO; no host-side interrupt is injected on the input path. The bytes are
  guest-visible input only and cannot perturb host interrupt state. A writer can
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
  create multiple VMs. Slot/base allocation is monotonic and never reused
  within a boot session, so bases do not alias, but there is no cap on VM
  count or aggregate memory. A production control plane should enforce quotas.
* riscv64 and aarch64 are supported; other architectures reject boot.
* The console TX FIFO is bounded and drops on overflow (see Input handling), so
  guest output under burst can be truncated. This is a fidelity limit, not a
  safety one; a production path wanting loss-free capture would add backpressure
  or a larger/spillable buffer.
