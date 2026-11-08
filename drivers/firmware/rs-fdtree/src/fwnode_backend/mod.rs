// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Backends that expose device descriptions through the neutral `fwnode`
//! contract.
//!
//! Today this directory contains only the device-tree backend; additional
//! adapters owned by this crate can be added as sibling submodules. The crate
//! core keeps the generic device-tree interpretation, so these modules only
//! translate borrowed results into contract types.

mod dtb;

pub use dtb::DtBackend;
