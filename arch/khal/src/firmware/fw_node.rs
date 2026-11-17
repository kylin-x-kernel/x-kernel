// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Publication of the firmware description root as a backend-neutral
//! [`FwNode`].
//!
//! Today the only backend is the device tree. ACPI will publish the same
//! contract from its namespace once it grows device/IRQ translation, and the
//! static-configuration backend publishes it from its board table. Consumers
//! above this module must not reach back into `of` or `acpi` to walk firmware
//! tables; the only transitional exception is `of`'s shared interrupt-cell
//! decoder, which moves to the controller drivers with `irq_domain`.

use core::fmt;

use fwnode::FwNode;
use lazyinit::LazyInit;
use rs_fdtree::{DtBackend, LinuxFdt};

static DT_BACKEND: LazyInit<DtBackend<'static>> = LazyInit::new();
static ROOT: LazyInit<FwNode<'static>> = LazyInit::new();

/// Failure to publish the firmware root node.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FwNodeInitError {
    /// The device tree has no root node.
    MissingDtRoot,
}

impl fmt::Display for FwNodeInitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingDtRoot => f.write_str("device tree has no root node"),
        }
    }
}

/// Publishes `tree` as the firmware root node.
///
/// Called once, after the device tree itself has been validated and
/// published by `of`.
pub(super) fn init(tree: &'static LinuxFdt<'static>) -> Result<(), FwNodeInitError> {
    let backend = DT_BACKEND.init_once(DtBackend::new(*tree));
    let root = backend.root().ok_or(FwNodeInitError::MissingDtRoot)?;
    ROOT.init_once(root);
    Ok(())
}

/// Returns the firmware description root.
///
/// Returns `None` before firmware initialization or when the active
/// firmware interface has no backend-neutral node representation yet.
pub fn root() -> Option<FwNode<'static>> {
    ROOT.get().copied()
}
