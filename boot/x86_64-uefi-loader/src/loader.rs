// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{string::String, vec::Vec};

use log::{error, info};
use uefi::{
    boot::AllocateType,
    mem::memory_map::MemoryType,
    prelude::*,
    proto::media::file::{File, FileAttribute, FileInfo, FileMode, FileType, RegularFile},
};
use x86_boot_common::{LoadedKernel, kernel_image_size, load_kernel_elf};

use crate::pages_for;

pub(crate) fn load_kernel(image: Handle, kernel_paths: &[String]) -> Result<Vec<u8>, Status> {
    let mut fs = uefi::boot::get_image_file_system(image).map_err(|e| e.status())?;
    let mut root = fs.open_volume().map_err(|e| e.status())?;

    for path in kernel_paths {
        let sanitized_path = match sanitize_kernel_path(path) {
            Ok(path) => path,
            Err(err) => {
                error!("rejecting invalid kernel path {:?}: {:?}", path, err);
                continue;
            }
        };
        let path16 = uefi::CString16::try_from(sanitized_path.as_str())
            .map_err(|_| Status::INVALID_PARAMETER)?;
        info!("trying kernel path: {}", sanitized_path);
        match root.open(&path16, FileMode::Read, FileAttribute::empty()) {
            Ok(handle) => {
                let file = match handle.into_type().map_err(|e| e.status())? {
                    FileType::Regular(f) => f,
                    _ => return Err(Status::UNSUPPORTED),
                };
                info!("kernel opened: {}", sanitized_path);
                return read_file(file);
            }
            Err(err) => {
                info!("open failed for {}: {:?}", sanitized_path, err.status());
                continue;
            }
        }
    }

    error!("kernel file not found in EFI root");
    Err(Status::NOT_FOUND)
}

pub(crate) fn sanitize_kernel_path(path: &str) -> Result<String, Status> {
    if path.is_empty() || path.starts_with('/') || path.starts_with('\\') || path.contains(':') {
        return Err(Status::INVALID_PARAMETER);
    }

    let mut normalized = String::new();
    for component in path.split(['/', '\\']) {
        if component.is_empty() || component == "." || component == ".." {
            return Err(Status::INVALID_PARAMETER);
        }
        if !normalized.is_empty() {
            normalized.push('\\');
        }
        normalized.push_str(component);
    }

    if normalized.is_empty() {
        return Err(Status::INVALID_PARAMETER);
    }

    Ok(normalized)
}

fn read_file(mut file: RegularFile) -> Result<Vec<u8>, Status> {
    let info = file.get_boxed_info::<FileInfo>().map_err(|e| e.status())?;
    let file_size = info.file_size() as usize;
    info!("kernel file size = {} bytes", file_size);
    let mut data = alloc::vec![0u8; file_size];
    let mut read = 0usize;
    while read < file_size {
        let slice = &mut data[read..];
        let len = file.read(slice).map_err(|e| e.status())?;
        if len == 0 {
            break;
        }
        read += len;
    }
    data.truncate(read);
    info!("kernel file read = {} bytes", read);
    Ok(data)
}

pub(crate) fn load_kernel_image(image: &[u8]) -> Result<LoadedKernel, Status> {
    info!("kernel image size = {} bytes", image.len());
    let image_pages = pages_for(kernel_image_size(image).map_err(|_| Status::LOAD_ERROR)?);
    let load_paddr =
        uefi::boot::allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, image_pages)
            .map_err(|e| e.status())?
            .as_ptr() as u64;
    let loaded = load_kernel_elf(image, load_paddr).map_err(|_| Status::LOAD_ERROR)?;
    info!(
        "load_elf: image_vaddr={:#x}..{:#x} load_paddr={:#x} pages={}",
        loaded.image_vaddr_range.0, loaded.image_vaddr_range.1, loaded.load_paddr, image_pages
    );
    Ok(loaded)
}
