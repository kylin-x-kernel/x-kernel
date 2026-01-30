// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux kernel chosen nodes

use crate::node::FdtNode;

/// Represents the node with interrupt-controller property
#[derive(Debug, Clone, Copy)]
pub struct InterruptController<'b, 'a> {
    pub(crate) node: FdtNode<'b, 'a>,
}

impl<'b, 'a: 'b> InterruptController<'b, 'a> {
    /// returns compatible property
    pub fn compatible(self) -> Option<&'a str> {
        self.node.compatible().and_then(|comp| comp.first())
    }
}
