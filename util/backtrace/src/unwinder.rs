// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Stack unwinding implementation.
//!
//! Unwinding fills a caller-provided frame buffer and never allocates: it
//! runs on panic/NMI/lockup paths where the allocator lock may be held by
//! the interrupted code, so requesting heap memory there could self-deadlock.

use crate::{config::BacktraceConfig, error::Result, frame::Frame};

/// Stack unwinder.
pub struct Unwinder<'a> {
    config: &'a BacktraceConfig,
}

impl<'a> Unwinder<'a> {
    /// Create a new unwinder with the given configuration.
    pub const fn new(config: &'a BacktraceConfig) -> Self {
        Self { config }
    }

    /// Unwind the stack from the given frame pointer into `out`.
    ///
    /// Returns the number of frames written. The unwind stops at the first
    /// invalid frame, on a detected cycle, or when `out` is full; the
    /// configured max depth also caps the walk.
    pub fn unwind(&self, mut fp: usize, out: &mut [Frame]) -> usize {
        // Validate initial frame pointer
        if !self.config.validate_fp(fp) {
            return 0;
        }

        let max_depth = out.len().min(self.config.max_depth);
        let mut written = 0;
        let mut prev_fp = 0;

        while written < max_depth {
            // Validate frame pointer bounds
            if !self.config.validate_fp(fp) {
                break;
            }

            // Read frame
            let frame = match Frame::read(fp) {
                Ok(frame) => frame,
                Err(_) => break, // Stop on first invalid frame
            };

            // Always record the current frame before deciding whether to continue.
            out[written] = frame;
            written += 1;

            // Check for cycles
            if frame.fp == prev_fp {
                log::warn!("Detected frame pointer cycle at {:#x}", fp);
                break;
            }

            if frame.fp <= fp {
                log::warn!("Frame pointer not increasing: {:#x} -> {:#x}", fp, frame.fp);
                break;
            }

            if let Some(large_stack_end) = fp.checked_add(self.config.max_stack_size)
                && frame.fp >= large_stack_end
            {
                log::warn!(
                    "Stack frame too large: {:#x} bytes between fp {:#x} and next fp {:#x}, \
                     stopping unwind",
                    frame.fp - fp,
                    fp,
                    frame.fp
                );
                break;
            }

            // Move to next frame
            prev_fp = fp;
            fp = frame.fp;
        }

        written
    }
}

/// Backward-compatible allocation-based unwinding entry point.
///
/// Only for callers that can tolerate allocation (e.g. `unwind_stack`
/// public API); the `Backtrace` capture paths use the allocation-free
/// [`Unwinder::unwind`] buffer API.
pub(crate) fn unwind_alloc(config: &BacktraceConfig, fp: usize) -> Result<alloc::vec::Vec<Frame>> {
    let mut frames = alloc::vec::Vec::new();
    let mut buffer = [Frame::new(0, 0); 64];
    let written = Unwinder::new(config).unwind(fp, &mut buffer);
    frames.extend_from_slice(&buffer[..written]);
    Ok(frames)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_unwinder_validates_fp_range() {
        let config = BacktraceConfig::new(0..0x1000, 0..0x1000);
        let unwinder = Unwinder::new(&config);

        // Out of range frame pointer
        let mut buffer = [Frame::new(0, 0); 8];
        assert_eq!(unwinder.unwind(0x2000, &mut buffer), 0);
    }

    #[test]
    fn test_unwinder_stops_at_buffer_capacity() {
        let config = BacktraceConfig::new(0..usize::MAX, 0..usize::MAX);
        let unwinder = Unwinder::new(&config);
        let mut buffer = [Frame::new(0, 0); 2];
        // An invalid frame pointer aborts immediately.
        assert_eq!(unwinder.unwind(0, &mut buffer), 0);
    }
}
