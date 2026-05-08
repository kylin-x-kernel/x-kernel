// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux/macOS platform support.
//!
//! Accesses linker-generated section boundaries for profile data sections.
//!
//! ## ELF (Linux)
//!
//! The linker auto-generates `__start___section` / `__stop___section` boundary
//! symbols. We place fallback data in the sections so these symbols always exist.
//!
//! ## Mach-O (macOS)
//!
//! macOS does not auto-generate section boundary symbols like ELF.
//! When the final binary is compiled with `-Cinstrument-coverage`, the
//! compiler places profiling data into sections and provides accessor
//! functions. For library-only builds (testing), we use static buffers.

use core::ptr;

use crate::types::{LlvmProfileData, ValueProfNode};

#[cfg(target_os = "macos")]
mod imp {
    use super::*;

    // Static zero-initialized buffers for non-instrumented builds.
    // These are replaced at link time when the binary uses -Cinstrument-coverage.
    static mut DATA_BUFFER: [u8; core::mem::size_of::<LlvmProfileData>()] =
        [0; core::mem::size_of::<LlvmProfileData>()];
    static mut COUNTERS_BUFFER: [u8; 8] = [0; 8];
    static mut BITMAP_BUFFER: [u8; 1] = [0; 1];
    static mut NAMES_BUFFER: [u8; 1] = [0; 1];
    static mut VNODES_BUFFER: [u8; core::mem::size_of::<ValueProfNode>()] =
        [0; core::mem::size_of::<ValueProfNode>()];

    pub fn begin_data() -> *const LlvmProfileData {
        core::ptr::addr_of_mut!(DATA_BUFFER).cast()
    }
    pub fn end_data() -> *const LlvmProfileData {
        core::ptr::addr_of_mut!(DATA_BUFFER).cast()
    }
    pub fn begin_counters() -> *const u8 {
        core::ptr::addr_of_mut!(COUNTERS_BUFFER).cast()
    }
    pub fn end_counters() -> *const u8 {
        core::ptr::addr_of_mut!(COUNTERS_BUFFER).cast()
    }
    pub fn begin_bitmap() -> *const u8 {
        core::ptr::addr_of_mut!(BITMAP_BUFFER).cast()
    }
    pub fn end_bitmap() -> *const u8 {
        core::ptr::addr_of_mut!(BITMAP_BUFFER).cast()
    }
    pub fn begin_names() -> *const u8 {
        core::ptr::addr_of_mut!(NAMES_BUFFER).cast()
    }
    pub fn end_names() -> *const u8 {
        core::ptr::addr_of_mut!(NAMES_BUFFER).cast()
    }
    pub fn begin_vnodes() -> *const ValueProfNode {
        core::ptr::addr_of_mut!(VNODES_BUFFER).cast()
    }
    pub fn end_vnodes() -> *const ValueProfNode {
        core::ptr::addr_of_mut!(VNODES_BUFFER).cast()
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::*;

    unsafe extern "C" {
        static __start___llvm_prf_data: LlvmProfileData;
        static __stop___llvm_prf_data: LlvmProfileData;
        static __start___llvm_prf_cnts: u8;
        static __stop___llvm_prf_cnts: u8;
        static __start___llvm_prf_bits: u8;
        static __stop___llvm_prf_bits: u8;
        static __start___llvm_prf_names: u8;
        static __stop___llvm_prf_names: u8;
        static __start___llvm_prf_vnds: ValueProfNode;
        static __stop___llvm_prf_vnds: ValueProfNode;
    }

    pub fn begin_data() -> *const LlvmProfileData {
        ptr::addr_of!(__start___llvm_prf_data)
    }
    pub fn end_data() -> *const LlvmProfileData {
        ptr::addr_of!(__stop___llvm_prf_data)
    }
    pub fn begin_counters() -> *const u8 {
        ptr::addr_of!(__start___llvm_prf_cnts)
    }
    pub fn end_counters() -> *const u8 {
        ptr::addr_of!(__stop___llvm_prf_cnts)
    }
    pub fn begin_bitmap() -> *const u8 {
        ptr::addr_of!(__start___llvm_prf_bits)
    }
    pub fn end_bitmap() -> *const u8 {
        ptr::addr_of!(__stop___llvm_prf_bits)
    }
    pub fn begin_names() -> *const u8 {
        ptr::addr_of!(__start___llvm_prf_names)
    }
    pub fn end_names() -> *const u8 {
        ptr::addr_of!(__stop___llvm_prf_names)
    }
    pub fn begin_vnodes() -> *const ValueProfNode {
        ptr::addr_of!(__start___llvm_prf_vnds)
    }
    pub fn end_vnodes() -> *const ValueProfNode {
        ptr::addr_of!(__stop___llvm_prf_vnds)
    }
}

pub use imp::*;

pub fn begin_vtables() -> *const u8 {
    ptr::null()
}

pub fn end_vtables() -> *const u8 {
    ptr::null()
}

pub fn begin_vtabnames() -> *const u8 {
    ptr::null()
}

pub fn end_vtabnames() -> *const u8 {
    ptr::null()
}

pub fn write_binary_ids(_writer: *mut crate::types::ProfDataWriter) -> i32 {
    0
}

pub fn get_binary_ids_size() -> u64 {
    0
}
