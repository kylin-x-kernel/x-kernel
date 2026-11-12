// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The scheduler side of priority inheritance.

use crate::PiTaskNode;

/// Task operations a scheduler supplies to the inheritance graph.
///
/// The graph never names a scheduling class. The host maps its policies to a
/// donation level in `0..=99`, where `0` donates nothing, and applies the
/// resulting boost to its own run queues.
pub trait PiHost: Sized + 'static {
    /// A counted task reference. Cloning must not allocate or block.
    type Task: Clone + Send + Sync;

    /// Returns the calling task.
    ///
    /// Requires an installed current task.
    fn current() -> Self::Task;

    /// Reports whether both references name the same task.
    ///
    /// Called with the graph lock held; must not block, take run-queue
    /// locks, or reenter this crate.
    fn same_task(a: &Self::Task, b: &Self::Task) -> bool;

    /// Borrows the graph node embedded in `task`.
    ///
    /// The node must live as long as the task and must not be shared with
    /// another task. Called with the graph lock held; must not block, take
    /// run-queue locks, or reenter this crate.
    fn node(task: &Self::Task) -> &PiTaskNode<Self>;

    /// Returns the level this task donates while it waits, `0` for none.
    ///
    /// Includes any boost the task itself inherited, so a donation travels
    /// along the whole chain. Called with the graph lock held, so it must not
    /// block or take a run-queue lock.
    fn donor_priority(task: &Self::Task) -> u8;

    /// Sets the level `task` inherits, `0` to clear it.
    ///
    /// Called with no graph lock held; the host may take run-queue locks.
    /// Publish the updated donation level before returning.
    /// Must not voluntarily block: lock handoff calls this with preemption
    /// disabled until the new owner has been woken.
    /// Returns whether the task's donation level changed, which tells the
    /// graph to continue along the chain.
    fn set_boost(task: &Self::Task, priority: u8) -> bool;
}
