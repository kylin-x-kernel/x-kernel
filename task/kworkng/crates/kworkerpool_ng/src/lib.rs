// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! CPU-bound normal and bottom-half execution services for prepared work.
//!
//! Implement [`PartitionWorkSource`] and [`AcquiredExecution`] to connect queue
//! management. Start [`NormalServices`] after task/timer initialization, or prepare
//! [`BottomHalfServices`] before installing the corresponding softirq actions.
//! Publish demand through [`PartitionDispatch::notify`] before invoking the
//! matching prepared notification. Source preparation allocates; notification and
//! inspection reuse that storage. Worker growth runs in manager task context.
//!
//! These are implementation contracts, not business workqueue APIs. Integrators
//! own global registration/readiness and must drain sources before stopping the
//! services. Source detachment is distinct from acquired callback completion.
//!
//! ```no_run
//! use core::num::NonZeroUsize;
//!
//! use ktime_types::TimeSpan;
//! use kworkerpool_ng::{AcquiredExecution, NormalServices, PartitionWorkSource, SourceRemainder};
//! struct IdleSource;
//! struct Execution;
//! impl AcquiredExecution for Execution {
//!     fn run(self) {}
//! }
//! impl PartitionWorkSource for IdleSource {
//!     type Execution = Execution;
//!
//!     fn acquire(&self) -> (Option<Execution>, SourceRemainder) {
//!         (None, SourceRemainder::Empty)
//!     }
//! }
//! // In sleepable context, after task/timer initialization on every boot CPU.
//! let services = NormalServices::<IdleSource>::start(
//!     NonZeroUsize::new(2).unwrap(),
//!     TimeSpan::from_millis(100),
//! )
//! .unwrap();
//! // No sources were published, so the detached services can be stopped directly.
//! services.stop().unwrap();
//! ```

#![no_std]

extern crate alloc;

mod bottom_half;
pub use bottom_half::{
    BottomHalfPrepareError, BottomHalfPriority, BottomHalfRunError, BottomHalfService,
    BottomHalfServices,
};

mod dispatch;
mod execution;

// Cross-crate implementation contracts; the kworkng facade does not re-export them.
pub use dispatch::{PartitionDispatch, PartitionSource};
pub use execution::{AcquiredExecution, PartitionWorkSource, SourceRemainder};
mod source_detachment;

mod source;

mod normal;
pub use normal::{NormalRuntimeError, NormalServices};
mod pool;
mod worker;
