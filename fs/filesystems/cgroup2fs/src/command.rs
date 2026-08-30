// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec::Vec;
use core::str;

use kvfs::{VfsError, VfsResult};

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct ControllerCommand<'a> {
    pub(crate) name: &'a str,
    pub(crate) is_enabled: bool,
}

pub(crate) fn parse_command(data: &[u8]) -> VfsResult<&str> {
    if data.contains(&b'\0') {
        return Err(VfsError::InvalidInput);
    }
    str::from_utf8(data)
        .map(str::trim)
        .map_err(|_| VfsError::InvalidInput)
}

pub(crate) fn parse_subtree_control(data: &[u8]) -> VfsResult<Vec<ControllerCommand<'_>>> {
    let command = parse_command(data)?;
    let mut operations = Vec::new();
    for operation in command.split_ascii_whitespace() {
        let (prefix, name) = operation
            .split_at_checked(1)
            .ok_or(VfsError::InvalidInput)?;
        if name.is_empty() {
            return Err(VfsError::InvalidInput);
        }
        let is_enabled = match prefix {
            "+" => true,
            "-" => false,
            _ => return Err(VfsError::InvalidInput),
        };
        operations.push(ControllerCommand { name, is_enabled });
    }
    if operations.is_empty() {
        return Err(VfsError::InvalidInput);
    }
    Ok(operations)
}
