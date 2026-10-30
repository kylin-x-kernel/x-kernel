// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux kernel /chosen helpers.
use crate::node::FdtNode;

/// The `/chosen` node: firmware-to-kernel runtime parameters selected by
/// the boot loader.
#[derive(Debug, Clone, Copy)]
pub struct Chosen<'b, 'a> {
    pub(crate) node: FdtNode<'b, 'a>,
}

impl<'b, 'a: 'b> Chosen<'b, 'a> {
    /// Returns the kernel command line chosen by the boot loader.
    pub fn bootargs(self) -> Option<&'a str> {
        self.node.property_str("bootargs")
    }

    /// Returns the path of the node the boot loader designated as the
    /// console output device.
    pub fn stdout_path(self) -> Option<&'a str> {
        self.node
            .property_str("stdout-path")
            .or_else(|| self.node.property_str("linux,stdout-path"))
    }

    /// Returns the kernel log level this board asks for:
    ///
    /// ```dts
    /// chosen {
    ///     x-kernel,log-level = "debug";
    /// };
    /// ```
    ///
    /// A vendor property rather than `bootargs` so verbosity can be changed
    /// by editing the device tree alone — no kernel rebuild and no
    /// interference with the boot loader's command line.
    pub fn x_kernel_log_level(self) -> Option<&'a str> {
        self.node.property_str("x-kernel,log-level")
    }
}
