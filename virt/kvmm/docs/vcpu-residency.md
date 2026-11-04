# vCPU Residency And Forwarded Timer Lifecycle

## Scope And Delivery Status

This design covers AArch64 host-backed virtual timers and GICv2 HW list
registers when a vCPU kernel task blocks, migrates, resumes or terminates.
It does not change scheduling policy or treat guest exception exit as task
switch-out. The generic task scheduling observer is implemented in ktask;
KVMM now attaches a stable timer observer before publishing the vCPU task.
GICv2 exposes explicit CPU-local forwarding and validated guest-interface
regions. These changes still require the acceptance cases below; a passing
register-only migration test is not proof of guest IRQ delivery.
Guest-entry failure now invokes explicit device rollback in reverse preparation
order, before restoring IRQs, followed by final timer stop.

## Reference Model

Reference: Linux revision `8d3ae59288f1e7d58d76558a6ee96d533bc5019f`.

- `virt/kvm/kvm_main.c::vcpu_load/vcpu_put` bracket a loaded interval and
  register/unregister a preemption notifier. `kvm_sched_in/kvm_sched_out`
  perform architecture load/put on actual scheduling transitions.
- `include/linux/preempt.h::preempt_ops` distinguishes IRQ-off outgoing from
  IRQ-on incoming callbacks. X-Kernel's incoming callback remains IRQ-off,
  after outgoing handoff cleanup. Cancellation must respect this difference.
- `arch/arm64/kvm/arch_timer.c::timer_save_state` snapshots and disables the
  physical timer. `kvm_timer_vcpu_put` arms a background timer for a published
  blocking interval; ordinary runnable preemption does not need a wake timer.
- `kvm_timer_vcpu_load_gic` reconstructs the physical active state from the
  virtual interrupt's activity and timer output before restoring the timer.
- `drivers/irqchip/irq-gic.c` omits DIR for explicitly forwarded sources,
  exposes active-state operations, and clears active state when masking a
  forwarded source. A build flag or fixed INTID is not forwarding authority.
- `arch/arm64/kvm/vgic/vgic-v2.c` folds LR state into virtual interrupt state
  and reconstructs hardware LRs; it does not treat an old raw LR as proof that
  its physical backing is still owned on another CPU.

Linux GICv2 does not unconditionally clear the physical active bit on each
timer put. Its timer load reconstructs the destination state. Do not attribute
a per-switch revoke/active-clear policy to Linux; any stricter local policy
must preserve virtual activity and be validated separately.

## Ownership

| Owner | Responsibility | Must not own |
| --- | --- | --- |
| kiface-task-api | Scheduling callback contract, logical CPU and switch reason | VMM state, timer policy, register access |
| ktask | Actual task transitions, observer lifetime, source/destination identity | Guest registers, GIC forwarding, virtual interrupt injection |
| KVMM timer | Stable timer snapshot, loaded interval, blocking deadline, task wake route, final stop | Private GIC register bypasses, scheduler policy |
| KVMM vGIC | Logical pending/active state and LR folding/repopulation | Task scheduling or host timer queue internals |
| KIRQ | Validated IRQ capability and source identity/lifetime | vCPU timer deadlines or VM ownership |
| GIC backend | Explicit forwarding and physical active-state operations, host completion | Guest run-loop or task references |

The notification object must own stable shared state, not a raw pointer back
to a `Vcpu` already borrowed mutably by `vmm_run_vcpu`. Timer callback storage
is prepared before activation. Wake references must not create a
task-observer-task or VM-vCPU-VM ownership cycle.

`TimerResidency` implements `kiface_task_api::TaskScheduleObserver` and is
attached with `TaskInner::with_schedule_observer` before task publication.
The trailing Vcpu handle owns the same snapshot without altering the
assembly-visible architecture state. KIRQ's `PerCpuForwarding` retains Line
identity, not an exclusive lease; KVMM owns the permanent registered route
and must exclude competing source management for that handle's lifetime.

## Residency Protocol

The consumer distinguishes an inactive loaded interval, resident CPU state,
off-CPU saved state, and a permanently stopped interval. Guest execution and
host exit handling are substates of residency, not ownership transfers.

1. **Setup:** in sleepable context, validate the actual GIC virtualization
   capability, prepare the disabled host route and timer storage, then publish
   the vCPU. Unsupported GIC/GICH combinations fail before mapping or activation.
2. **Initial load:** with IRQs masked on the current CPU, establish the timer
   snapshot and current-owner route. A task's initial notification before its
   VMM resources are ready remains inactive; it must not touch guest hardware.
3. **Guest entry/exit:** retain the existing IRQ-masked world-switch interval.
   Save timer state and fold virtual IRQ state after guest execution, without
   retaining a borrowed `&mut Vcpu` in the scheduling observer. Ordinary guest
   exit alone does not stop residency.
4. **Task put:** while still on the source CPU, save and disable the resident
   timer before another task can use it. Resolve the source's forwarding/active
   ownership according to the IRQ-chip contract and detach only the matching
   wake owner. Preserve virtual pending/active state independently.
5. **Blocked interval:** publish guest-halt intent before the interruptible
   wait. If the enabled, unmasked timer is relevant, use ktask's owned timer
   future at the actual deadline. A generic blocked MMIO operation is not by
   itself a guest-halt interval. Runnable preemption retains a snapshot without
   creating unnecessary timer wakeups.
6. **Task load:** establish destination ownership, reconstruct the
   physical active state from logical virtual state and current output, and
   restore CVAL/offset before CTL. Do not cancel or wait for remote callbacks
   under scheduler notification. The WFI future owns cancellation on return
   in sleepable task context, outside the observer and its state lock.
7. **Final stop:** the outer WFI wait first drains its owned callback before
   returning in sleepable task context. Then, with IRQs masked, publish stopped
   before resource destruction, prevent future loads/re-enables, and put any
   resident state on its owning CPU. `VcpuHook::on_stop` clears saved HW LR
   associations before their backing is retired. Nonreturning task exit still
   receives terminal put, but it is not the sleepable teardown mechanism.

The current implementation reuses ktask's interruptible timer future instead
of duplicating KVM's hrtimer wrapper. Each WFI owns one future and its wake
registration. The callback only wakes the task; Drop drains the callback before
that wait returns. A new WFI cannot reuse old callback storage. Neither
scheduler callback allocates, arms an alarm, wakes tasks, or drains callbacks.
This preserves the KVM responsibility split, with cancellation at the outer
sleepable wait boundary rather than inside scheduler load.

## Deadline And Wake Rules

Saved CNTV_CVAL and CNTVOFF are counter ticks, not nanoseconds. Convert the
remaining physical-counter interval into the host monotonic clock domain using
the platform frequency. Cover already-due values, masked/disabled timers and
arithmetic overflow explicitly. Never read another CPU's current CNTVOFF as the
authority for an off-CPU vCPU.

The owned timer future wakes the task, never programs guest hardware or injects
a virtual IRQ on the timer's CPU. It releases its state lock before waking.
Guest entry recomputes output before injection, so an early or spurious wake
does not inject a premature IRQ. Register pending task wakes before parking;
query queued vGIC state after publishing WfiSleeping to include earlier events.
Disabled/masked timers park without a polling deadline and remain interruptible
by virtual IRQ injection or stop. Unsigned guest-counter comparisons prevent a
CVAL near u64::MAX from being mistaken for an already-expired signed delta.
Conversions round up and cap a host wait at the timer's u64-nanosecond horizon;
expiry is still checked before injection.

## Forwarding And Entry Failure

Registration, current CPU wake ownership, forwarding authority and physical
active state are separate facts. A globally registered IRQ 27 does not grant
every CPU permission to omit DIR or restore an HW LR. Source disable, virtual
EOI, saved active state and reasserted timer level must be reconciled together;
guest exit also reconciles the physical active bit without requiring a task
switch. Lowering a pending level before guest acknowledgement must not leave
an active physical backing that no future guest EOI can release.
Copying an old pending LR can replay a timer the guest has already lowered.

If guest entry fails after device preparation, unwind the installed device
context before releasing the IRQ-masked window. Do not run a normal read-back
path that presumes the guest successfully executed unless the architecture
contract guarantees that path is valid. Stop/background-drain requirements are
the same as normal final teardown.

`VcpuHook::on_entry_failure` is mandatory and distinct from `on_exit`.
The GICv2 hook disables GICH and clears CPU-local LRs/VMCR/APR without reading
guest progress; its saved LR image retains pending work consumed at preparation.
The timer hook disables CNTV and detaches wake ownership. RISC-V hooks clear
their injected pending bits. Architecture teardown still runs after rollback.

## Acceptance

Generic observer tests cover first entry, unpublished destruction, paired
blocking/resumption, same-task reselection, final exit, actual SMP migration,
CPU reuse and arming a prepared wake timer from outgoing notification. They do not prove
guest hardware correctness.

KVMM acceptance must separately cover ordinary guest exit/resume, WFI expiry
and expiry-before-park, disabled/masked timers, preemption without guest halt,
remote callback racing resume and rapid re-block, active and pending HW LR
migration, CPU reuse by another vCPU, late physical PPI delivery, guest-entry
failure and final stop with expiry in flight. Register-model tests establish
operation ordering; a GICv2 VMM QEMU workload must establish actual guest timer
delivery. Unsupported virtualization capabilities must fail before activation.

The disabled/masked WFI re-block test proves actual parking, repeated wakeup
and stop handling. It does not distinguish the unbounded wait branch from a
bounded polling sleep; no elapsed-time assertion establishes that distinction.

The scoped sleep probe in `ktask` is test-only and selects one preparing task.
`ut-tests/wfi.rs` pauses its real owned timer callback before wake on CPU0,
resumes the blocked WFI on CPU1, and waits until cancellation observes Running
before CPU2 releases the callback. Both stop and resume/re-block cases check that WFI cannot
return before callback completion. This drives the actual WFI consumer and
timer destructor, without executing guest instructions or imposing a deadline
on host scheduling. Expiry before parking is retried, not treated as failure.

`ut-tests/irq_route.rs` software-pends the real GICv2 PPI after detachment and
after CPU reuse by a distinct task, retaining the exited task to check that it
receives no interrupt hint. The no-owner delivery must complete in host mode without a
task wake; a reused CPU may wake its current owner but must not inject a guest
IRQ until that guest's own timer output is asserted. A positive-control entry
checks the injection probe. The fixture drains delivery and clears delegation;
the test-only route count and pending writer do not exist in production builds.
These controlled interleavings complement the two real migrated guests rather
than claiming that firmware or a physical board reproduces the injected race.
