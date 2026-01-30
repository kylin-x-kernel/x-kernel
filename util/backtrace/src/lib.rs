#![no_std]
#![doc = include_str!("../README.md")]
#![allow(rustdoc::bare_urls, rustdoc::broken_intra_doc_links)]

//! # Backtrace - Stack Unwinding for x-kernel
//!
//! This crate provides stack unwinding and symbolication support for bare-metal
//! and kernel environments.
//!
//! ## Features
//!
//! - **Multi-architecture**: Supports x86_64, aarch64, riscv32/64, loongarch64
//! - **DWARF symbolication**: Convert addresses to function names and source locations
//! - **Configurable**: Control unwinding depth and validation
//! - **Safe**: Comprehensive error handling and validation
//!
//! ## Quick Start
//!
//! ```no_run
//! use backtrace::{Backtrace, init};
//!
//! // Initialize with valid memory ranges
//! let code_range = 0x8000_0000..0x9000_0000;
//! let stack_range = 0x7000_0000..0x8000_0000;
//! init(code_range, stack_range);
//!
//! // Capture current backtrace
//! let bt = Backtrace::capture();
//! println!("{}", bt);
//! ```

#[cfg(feature = "alloc")]
extern crate alloc;

#[cfg(feature = "alloc")]
use alloc::vec::Vec;
use core::{fmt, ops::Range};

use spin::Once;

// Modules
pub mod arch;
pub mod config;
pub mod error;
pub mod frame;

#[cfg(feature = "alloc")]
mod unwinder;
pub use config::{max_depth, set_max_depth};
pub use error::{BacktraceError, Result};
pub use frame::Frame;
#[cfg(feature = "alloc")]
use unwinder::Unwinder;

#[cfg(feature = "dwarf")]
mod dwarf;

use config::BacktraceConfig;
#[cfg(feature = "dwarf")]
pub use dwarf::{DwarfReader, FrameIter};

/// Global backtrace configuration.
static CONFIG: Once<BacktraceConfig> = Once::new();

/// Initializes the backtrace library.
pub fn init(ip_range: Range<usize>, fp_range: Range<usize>) {
    CONFIG.call_once(|| BacktraceConfig::new(ip_range, fp_range));
    #[cfg(all(feature = "dwarf", not(test)))]
    dwarf::init();
}

/// Returns whether the backtrace library is initialized.
pub fn is_initialized() -> bool {
    CONFIG.get().is_some()
}

/// Returns whether the backtrace feature is enabled.
pub const fn is_enabled() -> bool {
    cfg!(feature = "dwarf")
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
#[cfg(feature = "alloc")]
pub fn unwind_stack(fp: usize) -> Vec<Frame> {
    let Some(config) = CONFIG.get() else {
        log::error!("Backtrace not initialized. Call backtrace::init() first.");
        return Vec::new();
    };

    let unwinder = Unwinder::new(config);
    match unwinder.unwind(fp) {
        Ok(frames) => frames,
        Err(e) => {
            log::error!("Stack unwinding failed: {}", e);
            Vec::new()
        }
    }
}

/// State of a captured backtrace.
#[allow(dead_code)]
#[derive(PartialEq, Eq, PartialOrd, Ord, Clone)]
enum Inner {
    /// Architecture does not support unwinding.
    Unsupported,
    /// Backtrace capturing is disabled.
    Disabled,
    /// Successfully captured backtrace.
    #[cfg(feature = "dwarf")]
    Captured(Vec<Frame>),
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
        #[cfg(not(feature = "dwarf"))]
        {
            Self {
                inner: Inner::Disabled,
            }
        }

        #[cfg(feature = "dwarf")]
        {
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
            let frames = unwind_stack(fp);
            // prevent this frame from being tail-call optimised away
            core::hint::black_box(());

            Self {
                inner: Inner::Captured(frames),
            }
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
    #[allow(unused_variables)]
    pub fn capture_trap(fp: usize, ip: usize, ra: usize) -> Self {
        #[cfg(not(feature = "dwarf"))]
        {
            Self {
                inner: Inner::Disabled,
            }
        }
        #[cfg(feature = "dwarf")]
        {
            let mut frames = unwind_stack(fp);
            // Fix up the first frame if needed
            if let Some(first) = frames.first_mut() {
                if let Some(config) = CONFIG.get() {
                    if !config.validate_ip(first.ip) {
                        first.ip = ra;
                    }
                }
            }

            frames.insert(0, Frame::new(fp, ip.wrapping_add(1)));

            Self {
                inner: Inner::Captured(frames),
            }
        }
    }

    /// Visit each stack frame in the captured backtrace.
    ///
    /// Returns `None` if backtrace is not captured or DWARF is not available.
    #[cfg(feature = "dwarf")]
    pub fn frames(&self) -> Option<FrameIter<'_>> {
        match &self.inner {
            Inner::Captured(frames) => Some(FrameIter::new(frames)),
            _ => None,
        }
    }

    /// Get the raw frames without symbolication.
    #[cfg(feature = "alloc")]
    pub fn raw_frames(&self) -> Option<&[Frame]> {
        match &self.inner {
            #[cfg(feature = "dwarf")]
            Inner::Captured(frames) => Some(frames),
            _ => None,
        }
    }

    /// Returns the number of frames in this backtrace.
    pub fn frame_count(&self) -> usize {
        match &self.inner {
            #[cfg(feature = "dwarf")]
            Inner::Captured(frames) => frames.len(),
            _ => 0,
        }
    }
}

impl fmt::Display for Backtrace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.inner {
            Inner::Unsupported => {
                writeln!(f, "<unwinding unsupported on this architecture>")
            }
            Inner::Disabled => {
                writeln!(
                    f,
                    "<backtrace disabled: enable the \"dwarf\" feature to capture backtraces>"
                )
            }
            #[cfg(feature = "dwarf")]
            Inner::Captured(frames) => {
                writeln!(f, "Backtrace:")?;
                dwarf::fmt_frames(f, frames)
            }
        }
    }
}

impl fmt::Debug for Backtrace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}
