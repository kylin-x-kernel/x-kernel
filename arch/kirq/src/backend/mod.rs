// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Contracts implemented by concrete interrupt controllers.
//!
//! Platform operations describe hardware capabilities and original claim
//! completion. MSI bridges controller-vector reservations into KIRQ mappings.
//! Register access and hardware discovery stay in drivers; generic admission,
//! action ownership and software drain stay in runtime.

pub(crate) mod msi;
pub(crate) mod platform;
