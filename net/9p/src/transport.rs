// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Provider-neutral transport connection used by the 9P client.

use kerrno::KResult;

/// One connected 9P transport endpoint.
pub trait P9Transport: Send {
    /// Sends one raw request and waits for its response.
    ///
    /// This operation may block and must run in task context without a
    /// spinlock held or interrupts disabled.
    ///
    /// # Errors
    ///
    /// Returns the transport-specific kernel error when the request cannot be
    /// completed. The response buffer contents are unspecified on error.
    fn request(&mut self, request: &[u8], response: &mut [u8]) -> KResult<usize>;
}
