// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! X-Kernel implementation of the [`device_res::MmioOp`] provider contract.

use core::ptr::NonNull;

use device_res::{MmioMapping, MmioOp, MmioRegion, ResError, ResResult};

use crate::XKernelResourceProvider;

impl MmioOp for XKernelResourceProvider {
    fn map_mmio(&self, region: MmioRegion, name: &'static str) -> ResResult<MmioMapping> {
        let vaddr =
            memspace::iomap_device(region.base.into(), region.size, name).map_err(map_iomap_err)?;
        let ptr = NonNull::new(vaddr.as_mut_ptr()).ok_or(ResError::MappingFailed)?;
        Ok(MmioMapping {
            vaddr: ptr.as_ptr() as usize,
            region,
        })
    }

    fn unmap_mmio(&self, mapping: MmioMapping) {
        let vaddr = memaddr::VirtAddr::from(mapping.vaddr);
        let _ = memspace::iounmap(vaddr);
    }
}

fn map_iomap_err(err: memspace::IoMapError) -> ResError {
    match err {
        memspace::IoMapError::NoMemory => ResError::NoMemory,
        memspace::IoMapError::InvalidRange => ResError::InvalidResource,
        memspace::IoMapError::MappingFailed => ResError::MappingFailed,
    }
}
