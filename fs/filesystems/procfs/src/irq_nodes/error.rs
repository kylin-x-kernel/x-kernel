// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! KIRQ affinity failures mapped to stable VFS errors.

use kerrno::LinuxError;
use kirq::{IrqAffinityError, IrqDescError, IrqLineHandle};
use kvfs::{VfsError, VfsResult};

pub(crate) fn map_affinity_read(error: IrqAffinityError) -> VfsError {
    match error {
        IrqAffinityError::Management(error) => map_management(error),
        IrqAffinityError::Unsupported | IrqAffinityError::OwnerFixed => {
            VfsError::from(LinuxError::EOPNOTSUPP)
        }
        IrqAffinityError::UserRestricted => VfsError::from(LinuxError::EACCES),
        IrqAffinityError::NoReadyCpu | IrqAffinityError::Rejected => {
            VfsError::from(LinuxError::EINVAL)
        }
        IrqAffinityError::HardwareFailure | IrqAffinityError::RecoveryRequiresDisable => {
            VfsError::from(LinuxError::EIO)
        }
    }
}

/// Maps an admitted user write. Linux's `write_irq_affinity` returns `EPERM`
/// when `irq_can_set_affinity_usr()` is false, including managed/fixed sources
/// and owner-restricted actions; read-side mask queries keep the adapter's
/// `EOPNOTSUPP`/`EACCES` observations.
pub(crate) fn map_affinity_write(error: IrqAffinityError) -> VfsError {
    match error {
        IrqAffinityError::Management(error) => map_management(error),
        IrqAffinityError::Unsupported
        | IrqAffinityError::OwnerFixed
        | IrqAffinityError::UserRestricted => VfsError::from(LinuxError::EPERM),
        IrqAffinityError::NoReadyCpu | IrqAffinityError::Rejected => {
            VfsError::from(LinuxError::EINVAL)
        }
        IrqAffinityError::HardwareFailure | IrqAffinityError::RecoveryRequiresDisable => {
            VfsError::from(LinuxError::EIO)
        }
    }
}

pub(crate) fn map_management(error: IrqDescError) -> VfsError {
    match error {
        IrqDescError::UnknownIrq => VfsError::NotFound,
        IrqDescError::NoIrqAction { .. } => VfsError::from(LinuxError::ENODEV),
        IrqDescError::TeardownInProgress { .. } => VfsError::from(LinuxError::EBUSY),
        IrqDescError::InvalidContext { .. } => VfsError::from(LinuxError::EIO),
        _ => VfsError::from(LinuxError::EIO),
    }
}

/// Rejects a fresh operation on a retired Line while allowing a live source
/// whose routing observation is unsupported or owner-fixed.
pub(crate) fn ensure_live(handle: &IrqLineHandle) -> VfsResult<()> {
    match handle.irq_affinity() {
        Ok(_) | Err(IrqAffinityError::Unsupported | IrqAffinityError::OwnerFixed) => Ok(()),
        Err(IrqAffinityError::Management(IrqDescError::UnknownIrq)) => Err(VfsError::NotFound),
        // The identity is still live; retry/read/write error mapping reports
        // the remaining policy, teardown or hardware condition.
        Err(_) => Ok(()),
    }
}
