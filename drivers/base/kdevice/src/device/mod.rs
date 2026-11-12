// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device descriptors, objects, handles, and resource metadata.

mod binding;
pub use binding::BindingRef;
pub mod desc;
pub mod handles;
/// Device-owned cleanup through explicit capability providers.
pub mod managed;
pub mod object;
pub mod resource;
