// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! procfs errno adaptation for the shared CPU text codec.

use kcpu_id_map::text::CpuTextError;
use kerrno::LinuxError;
use kvfs::VfsError;

pub(crate) fn map_cpu_text(error: CpuTextError) -> VfsError {
    match error {
        CpuTextError::InvalidSyntax => VfsError::from(LinuxError::EINVAL),
        CpuTextError::OutOfRange => VfsError::from(LinuxError::ERANGE),
        CpuTextError::Overflow => VfsError::from(LinuxError::EOVERFLOW),
        CpuTextError::EmptyDomain => VfsError::from(LinuxError::ENODEV),
        CpuTextError::Output => VfsError::from(LinuxError::EIO),
    }
}
