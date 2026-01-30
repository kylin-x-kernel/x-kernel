// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Stack unwinding implementation.

use alloc::vec::Vec;

use crate::{
    config::BacktraceConfig,
    error::{BacktraceError, Result},
    frame::Frame,
};

/// Stack unwinder.
pub struct Unwinder<'a> {
    config: &'a BacktraceConfig,
}

impl<'a> Unwinder<'a> {
    /// Create a new unwinder with the given configuration.
    pub const fn new(config: &'a BacktraceConfig) -> Self {
        Self { config }
    }

    /// Unwind the stack from the given frame pointer.
    pub fn unwind(&self, mut fp: usize) -> Result<Vec<Frame>> {
        // Validate initial frame pointer
        if !self.config.validate_fp(fp) {
            return Err(BacktraceError::OutOfRange {
                fp,
                range: (self.config.fp_range.start, self.config.fp_range.end),
            });
        }

        let mut frames = Vec::with_capacity(self.config.max_depth);
        let mut depth = 0;
        let mut prev_fp = 0;

        while depth < self.config.max_depth {
            // Validate frame pointer bounds
            if !self.config.validate_fp(fp) {
                break;
            }

            // Read frame
            let frame = match Frame::read(fp) {
                Ok(f) => f,
                Err(_) => break, // Stop on first invalid frame
            };

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
                return Err(BacktraceError::StackTooLarge {
                    fp: frame.fp,
                    prev_fp,
                    size: frame.fp,
                });
            }

            // Add frame
            frames.push(frame);

            // Move to next frame
            prev_fp = fp;
            fp = frame.fp;
            depth += 1;
        }

        Ok(frames)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_unwinder_validates_fp_range() {
        let config = BacktraceConfig::new(0..0x1000, 0..0x1000);
        let unwinder = Unwinder::new(&config);

        // Out of range frame pointer
        let result = unwinder.unwind(0x2000);
        assert!(matches!(result, Err(BacktraceError::OutOfRange { .. })));
    }
}
