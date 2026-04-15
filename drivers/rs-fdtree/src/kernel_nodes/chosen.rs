// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux kernel /chosen helpers.
use crate::node::FdtNode;

#[derive(Debug, Clone, Copy)]
pub struct Chosen<'b, 'a> {
    pub(crate) node: FdtNode<'b, 'a>,
}

impl<'b, 'a: 'b> Chosen<'b, 'a> {
    pub fn bootargs(self) -> Option<&'a str> {
        self.node.property_str("bootargs")
    }

    pub fn stdout_path(self) -> Option<&'a str> {
        self.node
            .property_str("stdout-path")
            .or_else(|| self.node.property_str("linux,stdout-path"))
    }
}
