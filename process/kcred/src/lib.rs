// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX process credentials.

#![no_std]
#![warn(missing_docs)]

extern crate alloc;

use alloc::sync::Arc;

use klazy::Once;

mod credentials;
mod namespace;

pub use credentials::{Cred, Gid, Uid};
pub use namespace::{NamespaceId, UserNamespace, initial_user_namespace};

static INITIAL_CRED: Once<Arc<Cred>> = Once::new();

/// Returns the credentials shared by the initial task and kernel-owned VFS objects.
pub fn initial_cred() -> Arc<Cred> {
    Arc::clone(INITIAL_CRED.call_once(|| Arc::new(Cred::root())))
}

#[cfg(unittest)]
mod tests;
