// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_std]
#![doc = include_str!("../README.md")]
#![allow(rustdoc::bare_urls, rustdoc::broken_intra_doc_links)]

//! # Backtrace - Stack Unwinding for x-kernel
//!
//! This crate provides frame-pointer based stack unwinding for bare-metal and
//! kernel environments.
//!
//! ## Design
//!
//! Stack unwinding and symbolication are decoupled:
//!
//! - **Unwinding** is always available: [`Backtrace::capture`] walks the
//!   frame-pointer chain and returns raw instruction addresses. This works
//!   without any debug data in the kernel image.
//! - **Symbolication** happens outside the kernel:
//!   - With the `dwarf` feature enabled, raw addresses are symbolicated in
//!     kernel with the embedded DWARF sections (legacy mode).
//!   - Otherwise the kernel prints raw addresses and the host side resolves
//!     them against the unstripped `kernel.debug.elf` (see the `xkmake
//!     symbolize` tool).
//!   - With the `symtab` feature enabled, a compact kernel symbol table adds
//!     `func+0xoff/0xsize` annotations to the raw addresses.
//!
//! ## Features
//!
//! - **Multi-architecture**: x86_64, aarch64, riscv32/64, loongarch64
//! - **Always-on frame-pointer unwinding**: raw addresses without debug data
//! - **Optional in-kernel DWARF symbolication** (`dwarf` feature)
//! - **Optional compact symbol table** (`symtab` feature)
//! - **Configurable**: unwinding depth and validation

extern crate alloc;

use alloc::vec::Vec;
use core::{fmt, ops::Range};

use klazy::Once;

// Modules
pub mod arch;
pub mod config;
pub mod error;
pub mod frame;

mod format;
mod unwinder;
pub use config::{max_depth, set_max_depth};
pub use error::{BacktraceError, Result};
pub use frame::Frame;
use unwinder::Unwinder;

#[cfg(feature = "dwarf")]
mod dwarf;
#[cfg(feature = "dwarf")]
pub use dwarf::{DwarfReader, FrameIter};

#[cfg(feature = "symtab")]
mod symtab;

use config::BacktraceConfig;

/// Global backtrace configuration.
static CONFIG: Once<BacktraceConfig> = Once::new();

/// Initializes the backtrace library.
///
/// # Arguments
///
/// * `ip_range` - Valid instruction pointer range.
/// * `fp_range` - Valid frame pointer range.
pub fn init(ip_range: Range<usize>, fp_range: Range<usize>) {
    CONFIG.call_once(|| BacktraceConfig::new(ip_range, fp_range));
    #[cfg(all(feature = "dwarf", not(test)))]
    dwarf::init();
    #[cfg(feature = "symtab")]
    symtab::init();
}

/// Returns whether the backtrace library is initialized.
pub fn is_initialized() -> bool {
    CONFIG.get().is_some()
}

// Unwind the stack from the given frame pointer.
/// Returns an empty vector if not initialized or on error.
///
/// # Examples
///
/// ```no_run
/// # use backtrace::{init, unwind_stack};
/// init(0..usize::MAX, 0..usize::MAX);
/// let frames = unwind_stack(0x7fff_0000);
/// ```
pub fn unwind_stack(fp: usize) -> Vec<Frame> {
    let Some(config) = CONFIG.get() else {
        log::error!("Backtrace not initialized. Call backtrace::init() first.");
        return Vec::new();
    };

    match unwinder::unwind_alloc(config, fp) {
        Ok(frames) => frames,
        Err(e) => {
            log::error!("Stack unwinding failed: {}", e);
            Vec::new()
        }
    }
}

/// Maximum number of frames a backtrace can hold.
///
/// The capture path is allocation-free (it runs on panic/NMI paths where
/// the allocator lock may be held), so frames live in a fixed buffer.
/// `set_max_depth` still caps the walk, but never beyond this bound.
const MAX_FRAMES: usize = 64;

/// Unwind into `out` without allocating; returns the number of frames
/// written. Used by [`Backtrace::capture`]/[`Backtrace::capture_trap`] so
/// panic/NMI paths never touch the heap (the allocator lock may be held by
/// the interrupted code).
fn capture_into(fp: usize, out: &mut [Frame]) -> usize {
    let Some(config) = CONFIG.get() else {
        log::error!("Backtrace not initialized. Call backtrace::init() first.");
        return 0;
    };
    Unwinder::new(config).unwind(fp, out)
}

/// State of a captured backtrace.
// `Unsupported` is only constructed on architectures without unwinding
// support, so it is dead on supported targets.
#[allow(dead_code)]
// The fixed capture buffer dwarfs the empty variant; that is the point of
// the allocation-free design.
#[allow(clippy::large_enum_variant)]
#[derive(PartialEq, Eq, PartialOrd, Ord, Clone)]
enum Inner {
    /// Architecture does not support unwinding.
    Unsupported,
    /// Successfully captured backtrace (fixed buffer + frame count).
    Captured([Frame; MAX_FRAMES], usize),
}

/// A captured stack backtrace.
///
/// This type represents a captured stack trace of a running program,
/// which can be printed or inspected for debugging purposes.
#[derive(PartialEq, Eq, PartialOrd, Ord, Clone)]
pub struct Backtrace {
    inner: Inner,
}

impl Backtrace {
    /// Capture the current thread's stack backtrace.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use backtrace::Backtrace;
    ///
    /// let bt = Backtrace::capture();
    /// println!("Backtrace:\n{}", bt);
    /// ```
    pub fn capture() -> Self {
        // Check if architecture is supported
        #[cfg(not(any(
            target_arch = "x86_64",
            target_arch = "aarch64",
            target_arch = "riscv32",
            target_arch = "riscv64",
            target_arch = "loongarch64"
        )))]
        {
            return Self {
                inner: Inner::Unsupported,
            };
        }

        use arch::{ArchBacktrace, CurrentArch};
        let fp = CurrentArch::current_fp();
        let mut frames = [Frame::new(0, 0); MAX_FRAMES];
        let count = capture_into(fp, &mut frames);
        // prevent this frame from being tail-call optimised away
        core::hint::black_box(());

        Self {
            inner: Inner::Captured(frames, count),
        }
    }

    /// Capture a backtrace from a trap/exception context.
    ///
    /// # Arguments
    ///
    /// * `fp` - Frame pointer from trap context
    /// * `ip` - Instruction pointer where trap occurred
    /// * `ra` - Return address from trap context
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use backtrace::Backtrace;
    ///
    /// // In exception handler
    /// let bt = Backtrace::capture_trap(
    ///     trap_frame.fp,
    ///     trap_frame.pc,
    ///     trap_frame.ra,
    /// );
    /// ```
    pub fn capture_trap(fp: usize, ip: usize, ra: usize) -> Self {
        let mut frames = [Frame::new(0, 0); MAX_FRAMES];
        // Reserve slot 0 for the synthetic trap frame; the unwound frames
        // follow it. The trap ip is stored as `ip + 1` so that the display
        // side's `adjust_ip()` (which subtracts 1) reproduces the exact
        // faulting address.
        frames[0] = Frame::new(fp, ip.wrapping_add(1));
        let mut count = capture_into(fp, &mut frames[1..]);
        // Fix up the first unwound frame if needed
        if count > 0
            && let Some(config) = CONFIG.get()
            && !config.validate_ip(frames[1].ip)
        {
            frames[1].ip = ra;
        }
        count += 1;

        Self {
            inner: Inner::Captured(frames, count),
        }
    }

    /// Visit each stack frame in the captured backtrace.
    ///
    /// Returns `None` if backtrace is not captured or DWARF is not available.
    /// Visit each stack frame in the captured backtrace.
    ///
    /// Returns `None` if backtrace is not captured or DWARF is not available.
    #[cfg(feature = "dwarf")]
    pub fn frames(&self) -> Option<FrameIter<'_>> {
        match &self.inner {
            Inner::Captured(..) => Some(FrameIter::new(self.frame_slice())),
            _ => None,
        }
    }

    /// Get the raw frames without symbolication.
    pub fn raw_frames(&self) -> Option<&[Frame]> {
        match &self.inner {
            Inner::Captured(frames, count) => Some(&frames[..*count]),
            _ => None,
        }
    }

    /// Returns the number of frames in this backtrace.
    pub fn frame_count(&self) -> usize {
        match &self.inner {
            Inner::Captured(_, count) => *count,
            _ => 0,
        }
    }
}

impl Backtrace {
    fn frame_slice(&self) -> &[Frame] {
        match &self.inner {
            Inner::Captured(frames, count) => &frames[..*count],
            Inner::Unsupported => &[],
        }
    }
}

impl fmt::Display for Backtrace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.inner {
            Inner::Unsupported => {
                writeln!(f, "<unwinding unsupported on this architecture>")
            }
            Inner::Captured(..) => {
                let frames = self.frame_slice();
                #[cfg(feature = "dwarf")]
                {
                    if dwarf::is_ready() {
                        writeln!(f, "Backtrace:")?;
                        return dwarf::fmt_frames(f, frames);
                    }
                }
                format::fmt_frames(f, frames)
            }
        }
    }
}

impl fmt::Debug for Backtrace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}
