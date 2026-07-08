// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX process credentials.

#![no_std]
#![warn(missing_docs)]

extern crate alloc;

mod credentials;
mod namespace;

pub use credentials::{AccessCredentials, AccessIdKind, CredentialError, Credentials, Gid, Uid};
pub use namespace::{NamespaceId, UserNamespace, initial_user_namespace};

#[cfg(unittest)]
mod tests;
