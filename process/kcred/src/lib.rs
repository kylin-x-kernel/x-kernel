// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Credential values and checked identity transitions for X-Kernel.
//!
//! Start with [`Cred`] to inspect user/group IDs or prepare a modified snapshot.
//! [`initial_cred`] returns the shared root credential; it is not the current
//! task's identity. [`NamespaceId`] and [`UserNamespace`] describe namespace
//! identity separately: credentials currently contain no namespace membership.
//!
//! The caller owns authorization of credential construction, supplementary-group
//! replacement, and publication. `kprocess` supplies task lookup and commit;
//! this crate has no current-thread dependency or commit operation.
//!
//! # Example
//!
//! Prepare and validate a replacement without changing a shared snapshot:
//!
//! ```
//! extern crate alloc;
//! use alloc::sync::Arc;
//!
//! use kcred::Cred;
//!
//! let committed = Arc::new(Cred::root());
//! let mut prepared = committed.prepare();
//! prepared.set_gid(100).unwrap(); // Change groups while still privileged.
//! prepared.set_uid(1000).unwrap();
//! assert_eq!((prepared.euid(), prepared.fsgid()), (1000, 100));
//! assert_eq!(committed.euid(), 0);
//! assert!(prepared.set_uid(2000).is_err());
//! // The task owner may now publish `prepared` using its commit interface.
//! ```
//!
//! # Execution context
//!
//! ID queries and transitions on existing credentials do not access a scheduler,
//! CPU-local state, or devices. Construction and group replacement require an
//! allocator. Initial singleton access may allocate and spin in [`klazy::Once`];
//! initialize singletons before interrupt use and avoid re-entering initialization.
//! Heap allocation and final deallocation inherit the allocator's context rules.

#![no_std]
#![warn(missing_docs)]

extern crate alloc;

use alloc::sync::Arc;

use klazy::Once;

mod credentials;
mod namespace;

/// Credential snapshots and the user/group identifier types used by their APIs.
pub use credentials::{Cred, Gid, Uid};
/// Namespace identities, parentage, and access to the shared initial user namespace.
pub use namespace::{NamespaceId, UserNamespace, initial_user_namespace};

static INITIAL_CRED: Once<Arc<Cred>> = Once::new();

/// Returns the credentials shared by the initial task and kernel-owned VFS objects.
///
/// Lazily allocates root credentials and returns a clone of the same `Arc` on
/// each call. This is not a lookup of the calling task's credentials.
/// Concurrent initialization may spin; first use requires an allocator and
/// must not re-enter this initializer.
///
/// # Panics
///
/// Panics if an earlier initialization unwound and poisoned the internal
/// [`klazy::Once`]. Allocation failure follows the allocator's failure policy.
pub fn initial_cred() -> Arc<Cred> {
    Arc::clone(INITIAL_CRED.call_once(|| Arc::new(Cred::root())))
}

#[cfg(unittest)]
#[path = "../ut-tests/kcred_unittest.rs"]
mod kcred_unittest_tests;
