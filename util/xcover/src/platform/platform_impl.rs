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

use core::{marker::PhantomData, ptr};

use crate::abi::layout::{LlvmProfileData, ValueProfNode};

pub struct SectionRange<T> {
    begin: *const T,
    end: *const T,
    _marker: PhantomData<T>,
}

impl<T> Clone for SectionRange<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for SectionRange<T> {}

impl<T> SectionRange<T> {
    const fn new(begin: *const T, end: *const T) -> Self {
        Self {
            begin,
            end,
            _marker: PhantomData,
        }
    }

    pub fn begin(self) -> *const T {
        self.begin
    }

    pub fn end(self) -> *const T {
        self.end
    }

    pub fn byte_len(self) -> usize {
        (self.end as usize).saturating_sub(self.begin as usize)
    }
}

impl SectionRange<u8> {
    pub fn as_slice(self) -> &'static [u8] {
        // SAFETY: profile sections are process-lifetime linker sections, and
        // this immutable view is bounded by the captured section range.
        unsafe { core::slice::from_raw_parts(self.begin, self.byte_len()) }
    }
}

impl SectionRange<LlvmProfileData> {
    pub fn len(self) -> usize {
        self.byte_len() / core::mem::size_of::<LlvmProfileData>()
    }

    pub fn as_slice(self) -> &'static [LlvmProfileData] {
        // SAFETY: the linker-provided profile-data section contains initialized
        // `LlvmProfileData` records for the process lifetime.
        unsafe { core::slice::from_raw_parts(self.begin, self.len()) }
    }
}

#[derive(Clone)]
pub struct ProfileSections {
    pub data: SectionRange<LlvmProfileData>,
    pub counters: SectionRange<u8>,
    pub bitmap: SectionRange<u8>,
    pub names: SectionRange<u8>,
    pub vnodes: SectionRange<ValueProfNode>,
    pub vtables: SectionRange<u8>,
    pub vtabnames: SectionRange<u8>,
}

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

pub fn profile_sections() -> ProfileSections {
    ProfileSections {
        data: SectionRange::new(imp::begin_data(), imp::end_data()),
        counters: SectionRange::new(imp::begin_counters(), imp::end_counters()),
        bitmap: SectionRange::new(imp::begin_bitmap(), imp::end_bitmap()),
        names: SectionRange::new(imp::begin_names(), imp::end_names()),
        vnodes: SectionRange::new(imp::begin_vnodes(), imp::end_vnodes()),
        vtables: SectionRange::new(begin_vtables(), end_vtables()),
        vtabnames: SectionRange::new(begin_vtabnames(), end_vtabnames()),
    }
}

fn begin_vtables() -> *const u8 {
    ptr::null()
}

fn end_vtables() -> *const u8 {
    ptr::null()
}

fn begin_vtabnames() -> *const u8 {
    ptr::null()
}

fn end_vtabnames() -> *const u8 {
    ptr::null()
}
