// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process namespace references and namespace-specific data.
//!
//! [`NsProxy`] selects copied/shared references during child creation. Mount trees
//! belong to KVFS, PID numbers to kidentity, credentials to kcred, and cgroups to
//! kcgroup. [`UtsNamespace`] owns mutable hostname/domainname bytes. Unsupported
//! clone flags return an error instead of claiming isolation.
//!
//! # Example
//!
//! ```no_run
//! use kns::UtsNamespace;
//! let parent = UtsNamespace::new();
//! parent.set_nodename(b"parent").unwrap();
//! let child = UtsNamespace::clone_from(&parent);
//! child.set_nodename(b"child").unwrap();
//! assert_eq!(parent.nodename(), b"parent");
//! assert_eq!(child.nodename(), b"child");
//! ```

#![no_std]

extern crate alloc;

pub mod error;
pub mod ipc;
pub mod net;
pub mod nsfd;
pub mod nsproxy;
pub mod pid;
pub mod time;
pub mod types;
pub mod uts;

pub use error::{CloneNsError, UtsError};
pub use ipc::{IpcNamespace, register_ipc_ns_drop_hook};
pub use kcgroup::CgroupNamespace;
pub use kcred::{NamespaceId, UserNamespace};
pub use kvfs::MntNamespace;
pub use net::NetNamespace;
pub use nsfd::NsFd;
pub use nsproxy::{NamespaceFsContext, NsProxy, PreparedUnshare, UnshareContext, UnshareError};
pub use pid::PidNamespace;
pub use time::TimeNamespace;
pub use types::{NamespaceFlags, NamespaceType};
pub use uts::UtsNamespace;
