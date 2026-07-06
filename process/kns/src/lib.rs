// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Namespace proxy and namespace types for X-Kernel.
//!
//! This crate defines the `NsProxy` structure that bundles all namespace
//! references for a process, along with individual namespace types.

#![no_std]

extern crate alloc;

pub mod error;
pub mod ipc;
pub mod mnt;
pub mod net;
pub mod nsproxy;
pub mod pid;
pub mod time;
pub mod types;
pub mod user;
pub mod uts;

pub use error::{CloneNsError, UtsError};
pub use ipc::IpcNamespace;
pub use kcgroup::CgroupNamespace;
pub use mnt::MntNamespace;
pub use net::NetNamespace;
pub use nsproxy::NsProxy;
pub use pid::PidNamespace;
pub use time::TimeNamespace;
pub use types::{NamespaceFlags, NamespaceId, NamespaceType};
pub use user::UserNamespace;
pub use uts::UtsNamespace;
