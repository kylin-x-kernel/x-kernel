// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared callback state with a single, non-reconstructible control owner.

use core::sync::atomic::{AtomicBool, Ordering};

use crate::{
    CallbackDriver, CallbackNode, ReaderNode, TaskReader, callback_queue::CallbackQueue,
    grace_period::GracePeriodCoordinator,
};

/// Owns callback queues and their coordinator, not a kernel task or timer.
///
/// The host may obtain exactly one driver and move it into its GP task. Losing
/// that driver does not permit another controller to restart an active period.
/// This is an integration boundary, not a public retirement/readiness API;
/// hardware observation and callback execution remain separate obligations.
pub struct CallbackState<'nodes, const CPUS: usize> {
    coordinator: GracePeriodCoordinator<'nodes, CPUS>,
    queues: [CallbackQueue; CPUS],
    is_driver_claimed: AtomicBool,
}

impl<'nodes, const CPUS: usize> CallbackState<'nodes, CPUS> {
    /// Creates empty callback queues over a fixed set of participating nodes.
    /// Capacity slots with `false` do not owe observations. An empty participant
    /// set returns `None`, without changing any node.
    ///
    /// # Safety
    /// Participating nodes must have no active period and must not be controlled
    /// by another coordinator for this state's lifetime. Existing task readers
    /// may remain enrolled: creation does not reset or remove them. The host must
    /// retain the state and all accepted callback storage through execution;
    /// dropping state is not cancellation or completion.
    pub unsafe fn new(
        nodes: &'nodes [ReaderNode; CPUS],
        participants: [bool; CPUS],
    ) -> Option<Self> {
        Some(Self {
            coordinator: GracePeriodCoordinator::new(nodes, participants)?,
            queues: [const { CallbackQueue::new() }; CPUS],
            is_driver_claimed: AtomicBool::new(false),
        })
    }

    /// Grants control once; duplicate or post-drop claims return `None`.
    ///
    /// The state must stay at its borrowed address while the driver is live.
    /// Move the original driver to hand off control, preserving its progress.
    /// Dropping it leaves control claimed, even when the last pass was idle.
    /// This does not publish runtime readiness or start a service task.
    pub fn claim_driver(&self) -> Option<CallbackDriver<'_, 'nodes, CPUS>> {
        if self.is_driver_claimed.swap(true, Ordering::AcqRel) {
            return None;
        }
        // SAFETY: these private queues/coordinator start empty/idle, and no
        // control API is exposed before this one-shot claim. The flag is never
        // reset, so no other driver can control or reconstruct this state.
        Some(unsafe { CallbackDriver::new(&self.coordinator, &self.queues) })
    }

    /// Enrolls the outgoing task before reporting this CPU's scheduler boundary.
    ///
    /// Returns whether a ready node report was accepted. A reading task remains
    /// a blocker even though its CPU has now supplied an observation. No wake,
    /// callback execution or GP completion occurs here. Inactive CPUs are ignored.
    ///
    /// # Safety
    /// `reader` must belong to the outgoing current task on `cpu`, with IRQs and
    /// preemption excluded throughout the call, in RCU-watched scheduler context.
    /// Retain stable task storage through enrollment and terminal cleanup as
    /// required by [`ReaderNode::enroll`]. No other task or interrupt may enter,
    /// exit or concurrently enroll this reader. NMI is unsupported. Hold no RCU
    /// bookkeeping lock; this method does not enter scheduler wake paths.
    pub unsafe fn note_context_switch(&self, cpu: usize, reader: &TaskReader) -> bool {
        // SAFETY: the caller supplies its retained outgoing task under scheduler
        // exclusion; coordinator enrollment precedes observation and reporting.
        unsafe { self.coordinator.note_context_switch(cpu, reader) }
    }

    /// Rechecks retained node readiness after a blocked reader may have exited.
    ///
    /// Returns whether a ready report was accepted. This does not supply a CPU
    /// observation: an unobserved CPU still prevents its node from reporting.
    /// The host may inspect another CPU's node; no wake or callback occurs here.
    ///
    /// # Safety
    /// Call from continuously RCU-watched task/IRQ context, never NMI, with no
    /// RCU bookkeeping lock held. The internal IRQ/preemption guard spans the
    /// fresh node snapshot and acceptance; do not substitute a cached report.
    pub unsafe fn report_ready(&self, cpu: usize) -> bool {
        self.coordinator.report_node(cpu)
    }

    /// Samples CPU quiescence for the current period and reports a ready node.
    ///
    /// Returns whether a node report was accepted. Quiescence clears only the
    /// CPU obligation: old enrolled readers still prevent reporting. Continue
    /// calling `report_ready` after those readers exit. Absent CPUs are ignored.
    /// The sample is taken under the node lock after GP-start ordering, not by
    /// the caller; snapshots are never reused across periods.
    ///
    /// # Safety
    /// For real retired objects, `watching` must be the permanent state of
    /// `cpu`, covering every protected execution path with CpuWatching's ordered,
    /// balanced transitions. Isolated tests may model those same obligations,
    /// but must never substitute a simulated state for real protected execution.
    /// Call from continuously watched task/IRQ context, never NMI, with no RCU
    /// bookkeeping lock held. No callback or scheduler wake occurs here.
    pub unsafe fn observe_watching(&self, cpu: usize, watching: &crate::CpuWatching) -> bool {
        // SAFETY: the host supplies the matching CPU's complete watching state
        // and watched context. The coordinator pins sampling through acceptance.
        unsafe { self.coordinator.observe_watching(cpu, watching) }
    }

    /// Whether this participating CPU still owes an observation for its period.
    ///
    /// A snapshot for progress assistance, not proof of completion. `false`
    /// includes absent CPUs, nodes without an active period and nodes still
    /// blocked by old readers after their CPU has reported. It neither records
    /// nor caches an observation.
    /// Reads an atomic hint without taking the node lock; it is not an NMI
    /// observation API.
    pub fn needs_cpu_observation(&self, cpu: usize) -> bool {
        self.coordinator.needs_cpu_observation(cpu)
    }

    /// Transfers a prepared node to a participating CPU's queue without allocation.
    ///
    /// Returns `false` without accessing `node` if no driver has been claimed or
    /// the CPU does not participate. In that case ownership stays with the caller.
    /// Success neither invokes the callback nor wakes the driver: the host must
    /// publish retained demand after this call and keep its service available.
    /// A driver claim is not proof of runtime readiness.
    ///
    /// # Safety
    /// On acceptance, `node` must be initialized, uniquely owned, unqueued and
    /// stable through invocation. Retain the state, storage, callback code and
    /// resources until execution completes. Withdraw the protected object before
    /// submission. The callback must match the containing layout and ownership,
    /// including cross-CPU transfer, and must not block, unwind or wait on RCU.
    /// Derive the pointer from the containing allocation's raw pointer if the
    /// callback recovers that allocation. Do not access this node after acceptance
    /// without independent ownership. Ordinary IRQ-safe locking must be permitted;
    /// NMI is unsupported. The host must establish readiness before admission.
    #[must_use = "rejected nodes remain caller-owned and need retry or cleanup"]
    pub unsafe fn submit(&self, cpu: usize, node: *mut CallbackNode) -> bool {
        if !self.is_driver_claimed.load(Ordering::Acquire) || !self.coordinator.is_participant(cpu)
        {
            return false;
        }
        // SAFETY: the caller retains a unique initialized node with its original
        // allocation provenance and matching callback until invocation.
        let node = unsafe { CallbackNode::from_raw(node) };
        // SAFETY: withdrawal precedes this transfer; the host retains state and
        // storage, permits IRQ-safe locking and supplies nonblocking cleanup.
        unsafe { self.queues[cpu].submit(node) };
        true
    }

    /// Reports queued eligibility, not completion of detached callback execution.
    ///
    /// The carrier rechecks after its bounded batch and requests another pass
    /// only for eligible work. Waiting for a GP is the driver's responsibility.
    /// Uses IRQ-safe locking; must not be called from NMI.
    pub fn has_ready(&self, cpu: usize) -> bool {
        self.coordinator.is_participant(cpu) && self.queues[cpu].has_ready()
    }

    /// Executes at most `budget` eligible callbacks outside bookkeeping locks.
    ///
    /// Returns the completed count; absent CPUs, zero budgets and competing or
    /// recursive executors return zero. No GP is advanced and no wake is sent.
    ///
    /// # Safety
    /// CPU observations used by the driver must be valid, not simulated for real
    /// retired objects. All independent retirement obligations must be satisfied.
    /// The context must permit the callbacks and their cleanup, with no RCU or
    /// callback-dependent locks held. Invocation must not block, unwind or wait
    /// on this engine. NMI is unsupported. Retain state through execution.
    pub unsafe fn invoke_ready(&self, cpu: usize, budget: usize) -> usize {
        if !self.coordinator.is_participant(cpu) {
            return 0;
        }
        // SAFETY: the host certifies observation/retirement validity and callback
        // context; the queue enforces unique detached execution and accounting.
        unsafe { self.queues[cpu].invoke_ready(budget) }
    }
}

#[cfg(all(test, not(unittest)))]
#[path = "../ut-tests/service.rs"]
mod tests;

unittest::path_mod_test!(tests, "../ut-tests/service.rs");
