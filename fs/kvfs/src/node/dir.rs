// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Directory entry helpers shared by VFS directory operations.

use crate::NodeType;

/// A trait for a sink that can receive directory entries.
pub trait DirEntrySink {
    /// Accept a directory entry, returns `false` if the sink is full.
    ///
    /// `offset` is the offset of the next entry to be read.
    ///
    /// It's not recommended to operate on the node inside the `accept`
    /// function, since some filesystem may impose a lock while iterating the
    /// directory, and operating on the node may cause deadlock.
    fn accept(&mut self, name: &str, ino: u64, node_type: NodeType, offset: u64) -> bool;
}

impl<F: FnMut(&str, u64, NodeType, u64) -> bool> DirEntrySink for F {
    fn accept(&mut self, name: &str, ino: u64, node_type: NodeType, offset: u64) -> bool {
        self(name, ino, node_type, offset)
    }
}

/// Directory iteration context passed to file operations.
pub struct DirContext<'a> {
    pos: u64,
    sink: &'a mut dyn DirEntrySink,
}

impl<'a> DirContext<'a> {
    /// Creates a context starting at `pos`.
    pub fn new(pos: u64, sink: &'a mut dyn DirEntrySink) -> Self {
        Self { pos, sink }
    }

    /// Returns the next directory offset.
    pub fn pos(&self) -> u64 {
        self.pos
    }

    /// Sets the next directory offset.
    pub fn set_pos(&mut self, pos: u64) {
        self.pos = pos;
    }

    /// Emits one directory entry and advances the context position if accepted.
    pub fn emit(&mut self, name: &str, ino: u64, node_type: NodeType, next_pos: u64) -> bool {
        let accepted = self.sink.accept(name, ino, node_type, next_pos);
        if accepted {
            self.pos = next_pos;
        }
        accepted
    }
}

impl DirEntrySink for DirContext<'_> {
    fn accept(&mut self, name: &str, ino: u64, node_type: NodeType, offset: u64) -> bool {
        self.emit(name, ino, node_type, offset)
    }
}
