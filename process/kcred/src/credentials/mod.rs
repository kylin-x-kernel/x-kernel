// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX process credentials.

mod error;
mod group;
mod model;
mod user;

pub use error::CredentialError;
pub use group::Gid;
pub use model::Credentials;
pub use user::Uid;
