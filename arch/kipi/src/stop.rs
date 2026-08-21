// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! System-wide CPU stop protocol.
//!
//! Before the kernel enters a platform power terminal (halt or power-off),
//! every CPU except the initiating one must stop executing tasks and stop
//! taking interrupts, otherwise surviving CPUs can keep mutating shared
//! state while the system is being torn down.
//!
//! The protocol is flag based rather than callback based so it never
//! allocates and works even when a target CPU's IPI event queue is full:
//!
//! 1. The orchestrator CPU claims the orchestration and publishes the
//!    request in a single atomic step: one `compare_exchange` on
//!    [`STOP_STATE`] stores its own logical CPU id, where
//!    [`NO_STOP_REQUESTED`] means no request. Fusing election and
//!    publication into one RMW leaves no intermediate state: any CPU
//!    (including the orchestrator itself) that observes a request also
//!    observes the orchestrator id, so no CPU can mistake itself for a
//!    stop target and park the very CPU that must reach the platform
//!    terminal.
//! 2. Every CPU entering [`crate::ipi_handler`] after the request is
//!    visible and seeing it is not the orchestrator prepares to park.
//!    Everything that may still touch shared state — logging, masking
//!    local interrupts, quiescing the local NMI source — happens before
//!    the acknowledgement; [`STOP_ACKED`] is the last shared write a
//!    stopped CPU ever makes, and after it the CPU executes only the
//!    non-returnable stop loop, so an orchestrator that observes the
//!    acknowledgement knows that CPU will never run again.
//! 3. The orchestrator waits for the target CPUs to acknowledge, bounded
//!    by a timeout. Only present CPUs whose IPI queues were initialized
//!    when the request was published are waited for: a present CPU that
//!    has not reached `kipi::init()` cannot take the shared IPI and
//!    acknowledge, so waiting for it would burn the timeout on every
//!    shutdown. A CPU that never acknowledges (for example because it
//!    spins with interrupts disabled on a lock) does not block the
//!    terminal; the orchestrator warns and proceeds, matching Linux's
//!    `smp_send_stop()` timeout semantics.
//!
//! The orchestrator holds preemption disabled for the whole protocol: it
//! must stay on the CPU whose id it claimed and published, since a
//! migrated orchestrator would leave that CPU running forever while every
//! other CPU treats it as the orchestrator that never parks.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kbuild_config::{IPI_IRQ, NR_CPUS};
use kcpu_id_map::{KCpuMask, KCpuMaskExt, LogicalCpuId};
use khal::{percpu::this_cpu_id, power::SmpStopIf};
use kirq::TargetCpu as IpiTarget;
use kspin::NoPreempt;
use ktime_types::TimeSpan;

/// Sentinel for [`STOP_STATE`]: no stop has been requested.
///
/// No logical CPU index equals it, which is what lets one value carry both
/// facts the protocol needs — that a stop was requested, and by which CPU.
const NO_STOP_REQUESTED: usize = usize::MAX;

/// Bounded time to wait for the other CPUs to acknowledge the stop request
/// before entering the platform terminal anyway.
const STOP_OTHER_CPUS_TIMEOUT: TimeSpan = TimeSpan::from_secs(1);

/// The stop-request state: [`NO_STOP_REQUESTED`], or the logical CPU id of
/// the CPU orchestrating the stop.
///
/// A single atomic both publishes the request and elects the orchestrator,
/// so there is no window in which the request is visible without its
/// orchestrator — the race the two-variable version of this protocol had.
static STOP_STATE: AtomicUsize = AtomicUsize::new(NO_STOP_REQUESTED);

/// Per-logical-CPU acknowledgement of the stop request: the last shared
/// write a stopped CPU makes, set only after it has stopped logging,
/// masked local interrupts, and quiesced its local NMI source, immediately
/// before it enters the non-returnable stop loop.
static STOP_ACKED: [AtomicBool; NR_CPUS] = [const { AtomicBool::new(false) }; NR_CPUS];

/// Disables the local NMI (or pseudo-NMI) source before the CPU parks.
///
/// The CPU-level exception masking in [`karch::stop_cpu`] covers every
/// maskable class, but a real NMI (e.g. the x86 NMI watchdog) is not
/// maskable that way. If such a source stayed enabled, a parked CPU would
/// keep taking watchdog NMIs, and the watchdog would mistake the
/// intentional stop for a hard lockup and panic into a platform
/// power-off, defeating [`khal::power::halt`]. Quiescing the source is
/// also defense-in-depth on platforms whose watchdog is a pseudo-NMI that
/// CPU-level masking already blocks (e.g. the AArch64 PMU cycle counter).
/// No-op when the platform has no NMI facility.
#[inline]
fn quiesce_local_nmi() {
    khal::quiesce_nmi();
}

/// Acknowledges the stop request and parks the current CPU forever.
///
/// The ordering is the terminal contract for stopped CPUs: masking local
/// interrupts, quiescing the local NMI source, and any logging happen
/// *before* the acknowledgement, because they may touch shared state or
/// take locks. The ack is then the last shared write this CPU ever makes —
/// an orchestrator that observes it may enter the platform terminal at
/// once — followed only by the non-returnable stop loop.
fn ack_and_park(current_cpu_index: usize) -> ! {
    karch::disable_local_irq();
    quiesce_local_nmi();
    STOP_ACKED[current_cpu_index].store(true, Ordering::Release);
    karch::stop_cpu()
}

/// Provider for [`SmpStopIf`], called by the terminal paths in
/// `khal::power` before they reach the bare platform terminal.
///
/// The calling CPU is expected to enter a platform power terminal
/// (`khal::power::halt()` or `khal::power::power_off()`) after this
/// returns. Stopped CPUs are parked with local interrupts masked and never
/// wake up, so this must only be used on the final shutdown path.
///
/// Before parking, each stopped CPU quiesces its local NMI / pseudo-NMI
/// source via [`khal::quiesce_nmi`]. This prevents an NMI-driven hard-lockup
/// watchdog from waking a parked CPU and mistaking the intentional stop for a
/// lockup, which would panic into a platform power-off and defeat
/// `khal::power::halt()`. The calling CPU's own NMI source is quiesced by
/// `khal::power` just before the bare platform terminal.
///
/// Re-entrant by construction: if another CPU is already orchestrating a
/// stop, the caller acknowledges and parks itself instead of returning.
///
/// # Context
///
/// May be called with local interrupts disabled (for example from the
/// scheduler), but not from interrupt or NMI context. Does not allocate.
/// Best-effort: CPUs that fail to acknowledge within
/// [`STOP_OTHER_CPUS_TIMEOUT`] are left running and reported with a
/// warning.
#[kiface::provide]
impl SmpStopIf {
    fn stop_other_cpus() {
        // Pin the orchestrator to this CPU for the whole protocol. The
        // claim, the broadcast, and the wait loop below all key off
        // `current_cpu_index`; if the calling task were migrated in
        // between, the CPU it left behind would be treated as the
        // orchestrator and never park, while this CPU waited for it until
        // the timeout expired.
        let _no_preempt = NoPreempt::new();

        // Degrade to a no-op until this CPU's IPI event queue is ready, so
        // terminals reached before `kipi::init()` fall through to the bare
        // platform terminal instead of broadcasting into uninitialized
        // queues.
        if !crate::is_ipi_queue_ready(this_cpu_id()) {
            return;
        }

        let current_cpu_id = this_cpu_id();
        let current_cpu_index = current_cpu_id.as_usize();

        // Atomically claim the orchestration and publish the request in one
        // step: the CAS stores this CPU's id into STOP_STATE, a value that
        // simultaneously means "a stop was requested" and "this CPU
        // orchestrates it". A concurrent caller loses the claim,
        // acknowledges and parks instead of returning. Fusing election and
        // publication into a single RMW leaves no intermediate state: any
        // CPU (including this one) that observes a request also observes
        // the orchestrator id, so an unrelated IPI hitting any CPU
        // mid-protocol can never make it mistake itself for a stop target
        // and strand the system before any platform terminal is reached.
        if STOP_STATE
            .compare_exchange(
                NO_STOP_REQUESTED,
                current_cpu_index,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            warn!(
                "CPU {} parked by a concurrent stop request",
                current_cpu_index
            );
            ack_and_park(current_cpu_index);
        }

        // Snapshot the CPUs this protocol waits for: every present CPU
        // except the caller whose IPI queue is already initialized. A
        // present CPU that has not reached `kipi::init()` cannot take the
        // shared IPI and therefore cannot acknowledge; waiting for one
        // would burn the full timeout on every shutdown. The broadcast
        // below still reaches such a CPU, and `handle_stop_request` parks
        // it on any later IPI, so excluding it here narrows only the wait
        // set, not the stop coverage.
        let mut target_cpus = KCpuMask::new();
        kcpu_id_map::for_each_present_logical_cpu(|_, cpu_id, _| {
            if cpu_id != current_cpu_id && crate::is_ipi_queue_ready(cpu_id) {
                target_cpus.set_logical(cpu_id, true);
            }
        });

        debug!(
            "requesting stop of all CPUs except CPU {}",
            current_cpu_index
        );
        kirq::notify_cpu(
            IPI_IRQ,
            IpiTarget::AllButSelf {
                me: current_cpu_index,
                total: kcpu_id_map::nr_cpus(),
            },
        );

        let deadline = khal::time::monotonic_time() + STOP_OTHER_CPUS_TIMEOUT;
        let unacked = |cpu_id: LogicalCpuId| !STOP_ACKED[cpu_id.as_usize()].load(Ordering::Acquire);
        loop {
            if !target_cpus.iter_logical().any(unacked) {
                debug!("all other CPUs acknowledged the stop request");
                return;
            }
            if khal::time::monotonic_time() >= deadline {
                for cpu_id in target_cpus.iter_logical().filter(|cpu_id| unacked(*cpu_id)) {
                    warn!(
                        "CPU {} did not acknowledge the stop request",
                        cpu_id.as_usize()
                    );
                }
                warn!(
                    "timed out waiting for other CPUs to stop; proceeding with CPU {} only",
                    current_cpu_index
                );
                return;
            }
            core::hint::spin_loop();
        }
    }
}

/// Parks the current CPU if a system-wide stop was requested for it.
///
/// Called at the entry of [`crate::ipi_handler`] so that any CPU receiving
/// the stop IPI acknowledges and stops before processing any further IPI
/// work. Returns normally for the orchestrator CPU and while no stop is in
/// progress.
pub(crate) fn handle_stop_request() {
    let stop_state = STOP_STATE.load(Ordering::Acquire);
    if stop_state == NO_STOP_REQUESTED {
        return;
    }
    let current_cpu_index = this_cpu_id().as_usize();
    if stop_state == current_cpu_index {
        return;
    }

    // Log before acknowledging: after the ack this CPU must not touch
    // shared state again (see `ack_and_park`).
    debug!("CPU {} stopping for system halt", current_cpu_index);
    ack_and_park(current_cpu_index);
}
