// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! CSV Guest Device for Hygon CSV (China Secure Virtualization)
//!
//! This module provides a character device `/dev/csv-guest` that allows
//! userspace applications to request attestation reports from the hypervisor.
//! The attestation report contains information about the guest VM, including
//! a sealing key that can be used for data protection.

use core::any::Any;

use fs_ng_vfs::{NodeFlags, VfsResult};
use kcore::vfs::DeviceOps;
use kerrno::KError;
use khal::mem::{VirtAddr, v2p};

/// Size constants matching the userspace tool
const PAGE_SIZE: usize = 4096;
const REPORT_USER_DATA_SIZE: usize = 64;
const REPORT_MNONCE_SIZE: usize = 16;
const SM3_HASH_BLOCK_SIZE: usize = 32;
const ATTESTATION_MAGIC_LEN: usize = 16;

/// Magic string for extension-aware attestation requests
const CSV_ATTESTATION_MAGIC_STRING: &[u8; 16] = b"ATTESTATION_EXT\0";

/// Hypercall number for VM attestation (specific to Hygon platform)
const KVM_HC_VM_ATTESTATION: u64 = 100;

/// IOCTL command for getting attestation report
/// Defined as _IOWR('D', 1, struct csv_report_req) in userspace
/// 'D' = 0x44, type = 1, size of csv_report_req = 16 bytes
/// _IOWR encoding: direction (3) << 30 | size << 16 | type << 8 | nr
const CSV_CMD_GET_REPORT: u32 = 0xC010_4401; // _IOWR('D', 1, 16)

/// Request structure for CSV_CMD_GET_REPORT IOCTL
/// This must match the userspace struct csv_report_req
#[repr(C, packed)]
struct CsvReportReq {
    /// User buffer address containing request data and receiving response
    report_address: u64,
    /// Length of the user buffer
    len: u32,
    /// Reserved for alignment
    _reserved: u32,
}

/// CSV Guest Device
///
/// Provides access to CSV attestation functionality through ioctl.
pub struct CsvGuestDevice;

impl CsvGuestDevice {
    /// Create a new CSV Guest Device
    pub fn new() -> Self {
        Self
    }

    /// Perform a hypercall to request attestation report from the hypervisor
    ///
    /// # Arguments
    /// * `nr` - Hypercall number (KVM_HC_VM_ATTESTATION = 100)
    /// * `pa` - Physical address of the request/response buffer
    /// * `len` - Length of the buffer
    ///
    /// # Returns
    /// Return value from the hypercall (0 on success)
    #[cfg(target_arch = "x86_64")]
    fn hypercall(nr: u64, pa: u64, len: u64) -> i64 {
        let ret: i64;
        unsafe {
            // Note: rbx is reserved by LLVM, so we need to save/restore it manually
            core::arch::asm!(
                "push rbx",
                "mov rbx, {pa}",
                "vmmcall",
                "pop rbx",
                pa = in(reg) pa,
                inout("rax") nr => ret,
                in("rcx") len,
                options()
            );
        }
        ret
    }

    #[cfg(not(target_arch = "x86_64"))]
    fn hypercall(_nr: u64, _pa: u64, _len: u64) -> i64 {
        // Not supported on other architectures
        -1
    }

    /// Handle the GET_REPORT ioctl command
    ///
    /// This function:
    /// 1. Reads the request structure from userspace
    /// 2. Copies user data (user_data, mnonce, hash) to a kernel buffer
    /// 3. Makes a hypercall to request the attestation report
    /// 4. Copies the response back to userspace
    fn handle_get_report(&self, arg: usize) -> VfsResult<usize> {
        // Read the request structure from userspace
        let req_ptr = arg as *const CsvReportReq;
        let req = unsafe { core::ptr::read_unaligned(req_ptr) };

        let user_addr = req.report_address as usize;
        let buf_len = req.len as usize;

        if buf_len == 0 || buf_len > PAGE_SIZE {
            warn!("csv-guest: invalid buffer length: {}", buf_len);
            return Err(KError::InvalidInput);
        }

        // Allocate a kernel buffer for the attestation request/response
        // We use a page-aligned buffer for the hypercall
        let mut kernel_buf = alloc::vec![0u8; PAGE_SIZE];

        // Copy user data from userspace to kernel buffer
        // Legacy format: user_data (64) + mnonce (16) + hash (32) = 112 bytes
        // Extension format: user_data (64) + mnonce (16) + hash (32) + magic (16) + flags (4) = 132 bytes
        // We copy the legacy size first, then check for extension magic if available
        let legacy_input_size = REPORT_USER_DATA_SIZE + REPORT_MNONCE_SIZE + SM3_HASH_BLOCK_SIZE;
        let ext_input_size = legacy_input_size + ATTESTATION_MAGIC_LEN + 4; // +4 for flags

        // Ensure the caller provided at least the legacy input size
        if buf_len < legacy_input_size {
            warn!(
                "csv-guest: buffer too small for legacy attestation request: {} (min {})",
                buf_len, legacy_input_size
            );
            return Err(KError::InvalidInput);
        }

        // Copy up to the available data, but never more than the extension input size
        let copy_len = core::cmp::min(buf_len, ext_input_size);
        let user_slice = unsafe { core::slice::from_raw_parts(user_addr as *const u8, copy_len) };
        kernel_buf[..copy_len].copy_from_slice(user_slice);

        // Check if this is an extension-aware request by looking for the magic string
        let mut is_ext_aware = false;
        if buf_len >= ext_input_size {
            let magic_offset = legacy_input_size;
            is_ext_aware = &kernel_buf
                [magic_offset..magic_offset + CSV_ATTESTATION_MAGIC_STRING.len()]
                == CSV_ATTESTATION_MAGIC_STRING;
        }

        if is_ext_aware {
            debug!("csv-guest: extension-aware attestation request detected");
        }

        // Get the physical address of the kernel buffer
        let kernel_buf_va = VirtAddr::from(kernel_buf.as_ptr() as usize);
        let kernel_buf_pa = v2p(kernel_buf_va);

        debug!(
            "csv-guest: hypercall pa={:#x}, len={}",
            kernel_buf_pa.as_usize(),
            buf_len
        );

        // Make the hypercall to request attestation report
        let ret = Self::hypercall(
            KVM_HC_VM_ATTESTATION,
            kernel_buf_pa.as_usize() as u64,
            buf_len as u64,
        );

        if ret != 0 {
            warn!("csv-guest: hypercall failed with error: {}", ret);
            return Err(KError::Io);
        }

        // Copy the response back to userspace
        let user_slice_out =
            unsafe { core::slice::from_raw_parts_mut(user_addr as *mut u8, buf_len) };
        user_slice_out.copy_from_slice(&kernel_buf[..buf_len]);

        info!("csv-guest: attestation report retrieved successfully");
        Ok(0)
    }
}

impl DeviceOps for CsvGuestDevice {
    fn read_at(&self, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        // Reading from the device is not supported
        Err(KError::InvalidInput)
    }

    fn write_at(&self, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        // Writing to the device is not supported
        Err(KError::InvalidInput)
    }

    fn ioctl(&self, cmd: u32, arg: usize) -> VfsResult<usize> {
        debug!("csv-guest: ioctl cmd={:#x}, arg={:#x}", cmd, arg);

        match cmd {
            CSV_CMD_GET_REPORT => self.handle_get_report(arg),
            _ => {
                warn!("csv-guest: unsupported ioctl cmd={:#x}", cmd);
                Err(KError::InvalidInput)
            }
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
    }
}
