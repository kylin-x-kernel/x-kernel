// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Concrete-work-independent acquisition and execution contracts.
//!
//! Ownership of a claim is not ownership of Work storage. A management adapter
//! must preserve the external owner's lifetime contract through callback/cleanup,
//! independently of the temporary source inspection. Runtime integration must
//! establish CPU/category context before acquisition.

/// Remaining service demand observed after an acquisition attempt.
///
/// Independent of whether that attempt acquired an execution. Concurrent source
/// notifications are retained separately by the dispatcher.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceRemainder {
    /// No further eligible demand was observed; this is not a queue-drained guarantee.
    Empty,
    /// Retain demand for another source inspection, even if no execution was acquired.
    More,
}

/// Management-side access to one partition, independent of its temporary inspection.
pub trait PartitionWorkSource {
    /// A unique claim valid after this source's inspection and reference retire.
    /// Must not borrow the temporary acquire receiver or depend on its retention.
    type Execution: AcquiredExecution;

    /// Claims at most one occurrence and snapshots remaining eligible demand.
    /// Called unlocked; no callback execution or retained management lock here.
    /// The remainder controls source requeueing, not worker occupancy.
    fn acquire(&self) -> (Option<Self::Execution>, SourceRemainder);
}

/// An acquired execution duty consumed after source inspection has ended.
pub trait AcquiredExecution {
    /// Consumes the claim exactly once, including callback and management cleanup.
    /// Runtime supplies CPU/category; no recoverable abandonment after acquisition.
    fn run(self);
}

unittest::path_mod_test!(execution_unittest, "../ut-tests/execution_unittest.rs");
