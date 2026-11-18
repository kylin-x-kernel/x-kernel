// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Complete-command transport shared by kvfs command consumers.
//!
//! The transport only copies one bounded request. It does not invoke a
//! provider callback, implement read state, or interpret a command.

use alloc::vec::Vec;

use iov_iter::IovIterSource;

use crate::{VfsError, VfsResult};

/// Behavior for a zero-length command request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EmptyCommandPolicy {
    /// Preserve the legacy `CommandFile` behavior: return success without
    /// invoking a provider callback.
    NoOp,
    /// Pass an empty command to the provider, allowing it to report its own
    /// empty-input error.
    Callback,
}

/// Bounds and empty-request policy for one complete command copy.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CompleteCommandOptions {
    pub max_bytes: usize,
    pub empty: EmptyCommandPolicy,
}

/// Copies one complete bounded command from `iter`.
///
/// `Ok(None)` is returned only for an empty request under
/// [`EmptyCommandPolicy::NoOp`]. Copy failure reverts the transport bytes
/// already consumed and returns the original error. Allocation is fallible.
pub(crate) fn copy_complete_command(
    iter: &mut IovIterSource<'_>,
    options: CompleteCommandOptions,
) -> VfsResult<Option<Vec<u8>>> {
    let count = iter.count();
    if count == 0 {
        return Ok(match options.empty {
            EmptyCommandPolicy::NoOp => None,
            EmptyCommandPolicy::Callback => Some(Vec::new()),
        });
    }
    if count > options.max_bytes {
        return Err(VfsError::FileTooLarge);
    }

    let mut command = Vec::new();
    command
        .try_reserve_exact(count)
        .map_err(|_| VfsError::NoMemory)?;
    command.resize(count, 0);

    let mut copied = 0;
    while copied < count {
        match iter.copy_from_iter(&mut command[copied..]) {
            Ok(read) if read != 0 => copied += read,
            Ok(_) => {
                iter.revert(copied)?;
                return Err(VfsError::BadAddress);
            }
            Err(error) => {
                iter.revert(copied)?;
                return Err(error);
            }
        }
    }
    Ok(Some(command))
}

unittest::path_mod_test!(tests, "../ut-tests/command.rs");
