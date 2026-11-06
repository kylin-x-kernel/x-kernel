// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! PLIC firmware-node selection and binding-specific configuration decoding.

use core::num::NonZeroU32;

use fwnode::{FwError, FwNode, FwNodeId};
use memaddr::PhysAddr;

use super::PLIC_MAX_SOURCE;

/// One discovered PLIC and the firmware node that described it.
///
/// The node is retained so initialization can associate this exact controller
/// instance with its IRQ domain. The legacy no-firmware layout has no node and
/// therefore cannot serve firmware-declared interrupts.
#[derive(Debug, Clone, Copy)]
pub(super) struct PlicConfig<'a> {
    pub(super) paddr: PhysAddr,
    pub(super) size: usize,
    pub(super) sources: PlicSources,
    pub(super) controller: FwNode<'a>,
}

/// A rejection while translating a firmware node into [`PlicConfig`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlicConfigError {
    /// The node's interrupt-controller declaration is malformed.
    Controller {
        /// Offending firmware node.
        node: FwNodeId,
        /// Backend failure.
        error: FwError,
    },
    /// The node's availability declaration is malformed.
    Status {
        /// Offending firmware node.
        node: FwNodeId,
        /// Backend failure.
        error: FwError,
    },
    /// The node's translated MMIO resources are malformed.
    Registers {
        /// Offending firmware node.
        node: FwNodeId,
        /// Backend failure.
        error: FwError,
    },
    /// A binding property is malformed.
    Property {
        /// Offending firmware node.
        node: FwNodeId,
        /// Binding property name.
        property: &'static str,
        /// Backend failure.
        error: FwError,
    },
    /// A supported node does not describe an MMIO region.
    MissingRegion {
        /// Offending firmware node.
        node: FwNodeId,
    },
    /// A firmware address or size does not fit the CPU address width.
    AddressWidth {
        /// Offending firmware node.
        node: FwNodeId,
    },
    /// `riscv,ndev` is missing or outside the supported source range.
    InvalidSourceCount {
        /// Offending firmware node.
        node: FwNodeId,
    },
}

impl<'a> PlicConfig<'a> {
    /// Translates the first available supported PLIC node below `root`.
    ///
    /// Returns `Ok(None)` when the tree contains no PLIC node with a recognized
    /// compatible string. A recognized but disabled node is skipped, while a
    /// malformed availability property is an error: silently falling back to
    /// the legacy fixed layout would hide broken firmware.
    ///
    /// # Errors
    ///
    /// Returns [`PlicConfigError`] when a candidate's availability, register
    /// resources, or `riscv,ndev` declaration cannot be interpreted.
    pub(super) fn from_fwnode(root: FwNode<'a>) -> Result<Option<Self>, PlicConfigError> {
        for node in root.descendants() {
            let id = node.id();
            if !node
                .is_interrupt_controller()
                .map_err(|error| PlicConfigError::Controller { node: id, error })?
            {
                continue;
            }
            if !node.firmware_ids().any(is_plic_compatible) {
                continue;
            }
            if !node
                .is_available()
                .map_err(|error| PlicConfigError::Status { node: id, error })?
            {
                continue;
            }
            let last = read_u32_property(node, id, "riscv,ndev")?
                .ok_or(PlicConfigError::InvalidSourceCount { node: id })?;
            let sources = PlicSources::from_last(last)
                .map_err(|_| PlicConfigError::InvalidSourceCount { node: id })?;
            let region = node
                .mmio_resource(0)
                .map_err(|error| PlicConfigError::Registers { node: id, error })?
                .ok_or(PlicConfigError::MissingRegion { node: id })?;
            let width = PlicConfigError::AddressWidth { node: id };
            let base = usize::try_from(region.base).map_err(|_| width)?;
            let size = usize::try_from(region.size).map_err(|_| width)?;
            base.checked_add(size).ok_or(width)?;
            return Ok(Some(Self {
                paddr: PhysAddr::from_usize(base),
                // The driver programs context registers through the declared
                // window; never map less than one register page.
                size: size.max(0x1000),
                sources,
                controller: node,
            }));
        }
        Ok(None)
    }
}

fn is_plic_compatible(compatible: &str) -> bool {
    matches!(compatible, "sifive,plic-1.0.0" | "riscv,plic0")
}

fn read_u32_property(
    node: FwNode<'_>,
    id: FwNodeId,
    property: &'static str,
) -> Result<Option<u32>, PlicConfigError> {
    match node.property(property) {
        Ok(Some(value)) => value
            .as_u32()
            .map(Some)
            .map_err(|error| PlicConfigError::Property {
                node: id,
                property,
                error,
            }),
        Ok(None) => Ok(None),
        Err(error) => Err(PlicConfigError::Property {
            node: id,
            property,
            error,
        }),
    }
}

/// Source-number range and per-source bookkeeping bound declared by firmware.
///
/// A controller's register array is sized from `last`, so this is retained by
/// the runtime controller rather than re-read from firmware later.
#[derive(Debug, Clone, Copy)]
pub(super) struct PlicSources {
    pub(super) last: u32,
}

impl PlicSources {
    fn from_last(last: u32) -> Result<Self, &'static str> {
        if last == 0 || last > PLIC_MAX_SOURCE {
            return Err("PLIC riscv,ndev exceeds supported source range 1..=1023");
        }
        Ok(Self { last })
    }

    pub(super) fn from_property(value: Option<&[u8]>) -> Result<Self, &'static str> {
        let bytes: [u8; 4] = value
            .ok_or("PLIC node is missing riscv,ndev")?
            .try_into()
            .map_err(|_| "PLIC riscv,ndev must contain exactly one u32")?;
        Self::from_last(u32::from_be_bytes(bytes))
    }

    pub(super) fn source(self, id: usize) -> Option<NonZeroU32> {
        let id = u32::try_from(id).ok()?;
        (id <= self.last).then(|| NonZeroU32::new(id)).flatten()
    }
}
