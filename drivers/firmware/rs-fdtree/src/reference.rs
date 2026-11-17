// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Generic phandle references and interrupt specifiers.
//!
//! These are standard flattened-devicetree interpretations: a phandle names
//! another node, `interrupts` inherits `interrupt-parent`, and the target
//! controller's `#interrupt-cells` gives the width of each specifier. No
//! controller-specific meaning is assigned to the returned cells.
//!
//! Accessors come in two forms: the plain methods resolve phandles by scanning
//! the structure block, and the `*_with` variants use an allocation-backed
//! `NodeIndex` built once the heap exists.

#[cfg(feature = "alloc")]
use crate::NodeIndex;
use crate::{Cells, FdtNode, LinuxFdt, NodeProperty, PropertyError};

/// Why a phandle reference or interrupt specifier could not be resolved.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReferenceError {
    /// A required property is missing or malformed.
    Property(PropertyError),
    /// A phandle does not name a node in this tree.
    UnresolvedPhandle(u32),
    /// `interrupts` is present but no ancestor declares `interrupt-parent`.
    MissingInterruptParent,
}

impl From<PropertyError> for ReferenceError {
    fn from(error: PropertyError) -> Self {
        Self::Property(error)
    }
}

/// A resolved reference: a target node and its argument cells.
#[derive(Clone, Copy, Debug)]
pub struct NodeReference<'b, 'a> {
    /// Referenced node.
    pub node: FdtNode<'b, 'a>,
    /// Argument cells that followed the phandle, in firmware order.
    pub args: Cells<'a>,
}

/// One interrupt specifier: the controller plus its raw binding cells.
#[derive(Clone, Copy, Debug)]
pub struct InterruptSpec<'b, 'a> {
    /// Controller named by the specifier.
    pub controller: FdtNode<'b, 'a>,
    /// Raw interrupt cells, in firmware order.
    pub params: Cells<'a>,
}

/// Phandle lookup strategy shared by the plain and indexed accessors.
#[derive(Clone, Copy)]
pub(crate) enum Phandles<'r, 'a> {
    /// Scan the structure block on every lookup.
    Linear(&'r LinuxFdt<'a>),
    /// Binary-search a prebuilt node index.
    #[cfg(feature = "alloc")]
    Indexed(&'r NodeIndex<'a>),
}

impl<'r, 'a> Phandles<'r, 'a> {
    fn resolve(self, phandle: u32) -> Option<FdtNode<'r, 'a>> {
        match self {
            Self::Linear(tree) => tree.node_by_phandle(phandle),
            #[cfg(feature = "alloc")]
            Self::Indexed(index) => index.node_by_phandle(phandle),
        }
    }
}

impl<'a> LinuxFdt<'a> {
    /// Finds the node named by `phandle`, honoring the legacy `linux,phandle`.
    ///
    /// Zero and `u32::MAX` do not name a node. This is an allocation-free
    /// linear search; build a `NodeIndex` instead when repeated lookups
    /// matter.
    pub fn node_by_phandle(&self, phandle: u32) -> Option<FdtNode<'_, 'a>> {
        if phandle == 0 || phandle == u32::MAX {
            return None;
        }
        self.all_nodes()
            .find(|node| node.phandle().ok() == Some(phandle))
    }
}

impl<'b, 'a: 'b> FdtNode<'b, 'a> {
    /// Returns this node's phandle, preferring `phandle` over the legacy
    /// `linux,phandle`.
    ///
    /// # Errors
    ///
    /// Returns [`PropertyError::Missing`] when neither property exists, or a
    /// strict property error when the value is duplicated or malformed.
    pub fn phandle(self) -> Result<u32, PropertyError> {
        match self.read_u32("phandle") {
            Ok(phandle) => Ok(phandle),
            Err(PropertyError::Missing) => self.read_u32("linux,phandle"),
            Err(error) => Err(error),
        }
    }

    /// Returns the effective `interrupt-parent`.
    ///
    /// The declaration is resolved and inherited while the tree is traversed,
    /// so this does not walk ancestors; only the final phandle-to-node lookup
    /// remains. A malformed declaration on this node or an ancestor is
    /// reported rather than treated as absence.
    ///
    /// # Errors
    ///
    /// Returns a property error for a duplicated or malformed declaration,
    /// or [`ReferenceError::UnresolvedPhandle`] when the phandle names no node.
    pub fn interrupt_parent(self) -> Result<Option<Self>, ReferenceError> {
        self.interrupt_parent_resolved(Phandles::Linear(self.as_tree()))
    }

    /// Returns the effective `interrupt-parent`, resolving its phandle with
    /// `index`.
    ///
    /// # Errors
    ///
    /// Same as [`Self::interrupt_parent`].
    #[cfg(feature = "alloc")]
    pub fn interrupt_parent_with<'i>(
        self,
        index: &'i NodeIndex<'a>,
    ) -> Result<Option<FdtNode<'i, 'a>>, ReferenceError> {
        self.interrupt_parent_resolved(Phandles::Indexed(index))
    }

    /// Resolves reference `index` of `property`.
    ///
    /// A reference is one phandle followed by `arg_cells` cells. The argument
    /// count comes from the referenced binding, so callers supply it. Out of
    /// range indices return `Ok(None)`.
    ///
    /// # Errors
    ///
    /// Returns a property error for a duplicated or malformed property, or
    /// [`ReferenceError::UnresolvedPhandle`] for a dangling phandle.
    pub fn reference(
        self,
        property: &str,
        index: usize,
        arg_cells: usize,
    ) -> Result<Option<NodeReference<'b, 'a>>, ReferenceError> {
        self.reference_resolved(Phandles::Linear(self.as_tree()), property, index, arg_cells)
    }

    /// Resolves reference `index` of `property`, resolving its phandle with
    /// `phandles`.
    ///
    /// # Errors
    ///
    /// Same as [`Self::reference`].
    #[cfg(feature = "alloc")]
    pub fn reference_with<'i>(
        self,
        phandles: &'i NodeIndex<'a>,
        property: &str,
        index: usize,
        arg_cells: usize,
    ) -> Result<Option<NodeReference<'i, 'a>>, ReferenceError> {
        self.reference_resolved(Phandles::Indexed(phandles), property, index, arg_cells)
    }

    /// Returns interrupt specifier `index`.
    ///
    /// `interrupts-extended` takes precedence and names its controller per
    /// entry. Plain `interrupts` uses the inherited `interrupt-parent` and
    /// that controller's `#interrupt-cells` width. The returned cells keep
    /// their binding meaning for the controller driver.
    ///
    /// # Errors
    ///
    /// Returns a property error for malformed or duplicated properties,
    /// [`ReferenceError::UnresolvedPhandle`] for a dangling phandle, or
    /// [`ReferenceError::MissingInterruptParent`] when `interrupts` has no
    /// inherited parent.
    pub fn irq_spec(self, index: usize) -> Result<Option<InterruptSpec<'b, 'a>>, ReferenceError> {
        self.irq_spec_resolved(Phandles::Linear(self.as_tree()), index)
    }

    /// Returns interrupt specifier `spec_index`, resolving controller
    /// phandles with `phandles`.
    ///
    /// # Errors
    ///
    /// Same as [`Self::irq_spec`].
    #[cfg(feature = "alloc")]
    pub fn irq_spec_with<'i>(
        self,
        phandles: &'i NodeIndex<'a>,
        spec_index: usize,
    ) -> Result<Option<InterruptSpec<'i, 'a>>, ReferenceError> {
        self.irq_spec_resolved(Phandles::Indexed(phandles), spec_index)
    }

    fn interrupt_parent_resolved<'r>(
        self,
        phandles: Phandles<'r, 'a>,
    ) -> Result<Option<FdtNode<'r, 'a>>, ReferenceError> {
        match self.effective_interrupt_parent() {
            Ok(Some(phandle)) => phandles
                .resolve(phandle)
                .ok_or(ReferenceError::UnresolvedPhandle(phandle))
                .map(Some),
            Ok(None) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn reference_resolved<'r>(
        self,
        phandles: Phandles<'r, 'a>,
        property: &str,
        index: usize,
        arg_cells: usize,
    ) -> Result<Option<NodeReference<'r, 'a>>, ReferenceError> {
        let Some(property) = optional_property(self, property)? else {
            return Ok(None);
        };
        let cells = property.cells()?;
        let stride = arg_cells
            .checked_add(1)
            .ok_or(PropertyError::InvalidEncoding)?;
        if !cells.len().is_multiple_of(stride) {
            return Err(PropertyError::InvalidEncoding.into());
        }
        let Some(start) = index.checked_mul(stride) else {
            return Ok(None);
        };
        let Some(phandle) = cells.get(start) else {
            return Ok(None);
        };
        let node = phandles
            .resolve(phandle)
            .ok_or(ReferenceError::UnresolvedPhandle(phandle))?;
        let args = cells
            .slice(start + 1, arg_cells)
            .ok_or(PropertyError::InvalidEncoding)?;
        Ok(Some(NodeReference { node, args }))
    }

    fn irq_spec_resolved<'r>(
        self,
        phandles: Phandles<'r, 'a>,
        index: usize,
    ) -> Result<Option<InterruptSpec<'r, 'a>>, ReferenceError> {
        if let Some(property) = optional_property(self, "interrupts-extended")? {
            return self.extended_irq_spec(phandles, property.cells()?, index);
        }
        let Some(property) = optional_property(self, "interrupts")? else {
            return Ok(None);
        };
        let cells = property.cells()?;
        let Some(controller) = self.interrupt_parent_resolved(phandles)? else {
            return Err(ReferenceError::MissingInterruptParent);
        };
        let count = controller.interrupt_cell_count()?;
        if count == 0 || !cells.len().is_multiple_of(count) {
            return Err(PropertyError::InvalidEncoding.into());
        }
        let Some(start) = index.checked_mul(count) else {
            return Ok(None);
        };
        let Some(params) = cells.slice(start, count) else {
            return Ok(None);
        };
        Ok(Some(InterruptSpec { controller, params }))
    }

    fn extended_irq_spec<'r>(
        self,
        phandles: Phandles<'r, 'a>,
        cells: Cells<'a>,
        index: usize,
    ) -> Result<Option<InterruptSpec<'r, 'a>>, ReferenceError> {
        let mut cursor = 0;
        let mut current = 0;
        while cursor < cells.len() {
            let phandle = cells.get(cursor).ok_or(PropertyError::InvalidEncoding)?;
            let controller = phandles
                .resolve(phandle)
                .ok_or(ReferenceError::UnresolvedPhandle(phandle))?;
            let count = controller.interrupt_cell_count()?;
            if count == 0 {
                return Err(PropertyError::InvalidEncoding.into());
            }
            let start = cursor + 1;
            let Some(params) = cells.slice(start, count) else {
                return Err(PropertyError::InvalidEncoding.into());
            };
            if current == index {
                return Ok(Some(InterruptSpec { controller, params }));
            }
            cursor = start + count;
            current += 1;
        }
        Ok(None)
    }

    fn interrupt_cell_count(self) -> Result<usize, ReferenceError> {
        let count = self.read_u32("#interrupt-cells")?;
        usize::try_from(count).map_err(|_| ReferenceError::Property(PropertyError::InvalidEncoding))
    }
}

fn optional_property<'b, 'a: 'b>(
    node: FdtNode<'b, 'a>,
    name: &str,
) -> Result<Option<NodeProperty<'a>>, PropertyError> {
    match node.property_strict(name) {
        Ok(property) => Ok(Some(property)),
        Err(PropertyError::Missing) => Ok(None),
        Err(error) => Err(error),
    }
}
