// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Writer sink — wraps a mutable byte buffer as a safe `ProfileWriter`.

use crate::{ProfileError, ProfileWriter};

/// Writes profile data into a borrowed byte buffer, advancing the cursor.
pub(crate) struct AbiSink<'a> {
    buf: &'a mut [u8],
}

impl<'a> AbiSink<'a> {
    /// Creates an `AbiSink` that writes directly to a buffer.
    pub(crate) fn from_buffer(buffer: &'a mut [u8]) -> Self {
        Self { buf: buffer }
    }
}

impl ProfileWriter for AbiSink<'_> {
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), ProfileError> {
        if bytes.len() > self.buf.len() {
            return Err(ProfileError::OutputTooSmall);
        }
        let written = bytes.len();
        self.buf[..written].copy_from_slice(bytes);
        // Advance the buffer slice.
        self.buf = core::mem::take(&mut self.buf).split_at_mut(written).1;
        Ok(())
    }
}
