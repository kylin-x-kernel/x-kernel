// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Allocation-backed node index.
//!
//! The core parser stays allocation-free and rescans the structure block for
//! every offset or phandle lookup. Once the kernel heap exists, this index
//! records the same nodes during one traversal and answers both lookups with
//! a binary search. The stored slices still borrow the immutable blob, so the
//! index adds only the `Vec` metadata.

use alloc::vec::Vec;

use crate::{FdtNode, LinuxFdt, node::NodeParts};

/// A device tree plus its nodes in structure order.
///
/// Nodes are collected depth-first, which is also strictly increasing
/// structure-offset order, so offset lookup is a binary search. Phandles are
/// sorted separately and keep the first declaration in firmware order, which
/// matches `LinuxFdt::node_by_phandle`. Malformed or duplicate phandle
/// properties are ignored, as they are by the linear lookup.
pub struct NodeIndex<'a> {
    tree: LinuxFdt<'a>,
    nodes: Vec<NodeParts<'a>>,
    /// `(phandle, node index)`, sorted by phandle.
    phandles: Vec<(u32, usize)>,
}

impl<'a> NodeIndex<'a> {
    /// Traverses `tree` once and indexes every node.
    ///
    /// Allocates the index metadata. The tree itself is copied, so the index
    /// remains valid for as long as the borrowed blob does.
    pub fn new(tree: LinuxFdt<'a>) -> Self {
        let mut nodes = Vec::new();
        let mut phandles = Vec::new();
        for node in tree.all_nodes() {
            let index = nodes.len();
            if let Ok(phandle) = node.phandle()
                && phandle != 0
                && phandle != u32::MAX
            {
                phandles.push((phandle, index));
            }
            nodes.push(node.parts());
        }
        // Stable sort plus first-wins dedup preserves firmware order among
        // nodes that (incorrectly) declare the same phandle.
        phandles.sort_by_key(|&(phandle, _)| phandle);
        phandles.dedup_by_key(|entry| entry.0);
        Self {
            tree,
            nodes,
            phandles,
        }
    }

    /// Returns the indexed tree.
    pub fn tree(&self) -> LinuxFdt<'a> {
        self.tree
    }

    /// Returns the number of indexed nodes.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Returns whether the tree has no nodes (it always has a root).
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Returns the node at a structure-block offset.
    pub fn node_by_offset(&self, offset: u32) -> Option<FdtNode<'_, 'a>> {
        let index = self
            .nodes
            .binary_search_by_key(&offset, |node| node.offset)
            .ok()?;
        Some(self.node(index))
    }

    /// Returns the node named by `phandle`, honoring the legacy
    /// `linux,phandle`.
    ///
    /// Zero and `u32::MAX` do not name a node.
    pub fn node_by_phandle(&self, phandle: u32) -> Option<FdtNode<'_, 'a>> {
        if phandle == 0 || phandle == u32::MAX {
            return None;
        }
        let index = self
            .phandles
            .binary_search_by_key(&phandle, |&(candidate, _)| candidate)
            .ok()?;
        Some(self.node(self.phandles[index].1))
    }

    /// Iterates indexed nodes in firmware order.
    pub fn nodes(&self) -> impl Iterator<Item = FdtNode<'_, 'a>> + '_ {
        (0..self.nodes.len()).map(|index| self.node(index))
    }

    fn node(&self, index: usize) -> FdtNode<'_, 'a> {
        FdtNode::from_parts(&self.tree, self.nodes[index])
    }
}
