// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device descriptors, objects, handles, and resource metadata.

mod binding;
pub use binding::BindingRef;
#[cfg(unittest)]
pub(crate) use binding::fail_next_data_install;
pub(crate) use binding::{BindingSource, DataInstallError, DataInstallFailureKind, DataTakeError};
pub mod desc;
pub mod handles;
/// Device-owned cleanup through explicit capability providers.
pub mod managed;
pub mod object;
mod publication;
pub use publication::PublicationId;
pub mod resource;
