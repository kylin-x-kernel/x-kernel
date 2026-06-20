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
    use core::cell::UnsafeCell;

    use super::*;

    struct StaticBuffer<const N: usize>(UnsafeCell<[u8; N]>);

    impl<const N: usize> StaticBuffer<N> {
        const fn new() -> Self {
            Self(UnsafeCell::new([0; N]))
        }

        fn as_mut_ptr(&self) -> *mut u8 {
            self.0.get().cast::<u8>()
        }
    }

    // SAFETY: these buffers are process-global fallback storage that the
    // profiling runtime accesses through raw pointers, matching the compiler-rt
    // model for section-backed memory.
    unsafe impl<const N: usize> Sync for StaticBuffer<N> {}

    // Static zero-initialized buffers for non-instrumented builds.
    // These are replaced at link time when the binary uses -Cinstrument-coverage.
    static DATA_BUFFER: StaticBuffer<{ core::mem::size_of::<LlvmProfileData>() }> =
        StaticBuffer::new();
    static COUNTERS_BUFFER: StaticBuffer<8> = StaticBuffer::new();
    static BITMAP_BUFFER: StaticBuffer<1> = StaticBuffer::new();
    static NAMES_BUFFER: StaticBuffer<1> = StaticBuffer::new();
    static VNODES_BUFFER: StaticBuffer<{ core::mem::size_of::<ValueProfNode>() }> =
        StaticBuffer::new();

    pub fn begin_data() -> *const LlvmProfileData {
        DATA_BUFFER.as_mut_ptr().cast()
    }
    pub fn end_data() -> *const LlvmProfileData {
        DATA_BUFFER.as_mut_ptr().cast()
    }
    pub fn begin_counters() -> *const u8 {
        COUNTERS_BUFFER.as_mut_ptr().cast()
    }
    pub fn end_counters() -> *const u8 {
        COUNTERS_BUFFER.as_mut_ptr().cast()
    }
    pub fn begin_bitmap() -> *const u8 {
        BITMAP_BUFFER.as_mut_ptr().cast()
    }
    pub fn end_bitmap() -> *const u8 {
        BITMAP_BUFFER.as_mut_ptr().cast()
    }
    pub fn begin_names() -> *const u8 {
        NAMES_BUFFER.as_mut_ptr().cast()
    }
    pub fn end_names() -> *const u8 {
        NAMES_BUFFER.as_mut_ptr().cast()
    }
    pub fn begin_vnodes() -> *const ValueProfNode {
        VNODES_BUFFER.as_mut_ptr().cast()
    }
    pub fn end_vnodes() -> *const ValueProfNode {
        VNODES_BUFFER.as_mut_ptr().cast()
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
