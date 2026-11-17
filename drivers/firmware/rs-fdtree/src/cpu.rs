// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Private device-tree view of a CPU node.
//!
//! This module owns the DT representation of a processor: the `device_type`
//! classification and the `reg`/`#address-cells` identity decoding. It does not
//! map the identity to a logical CPU, construct a `RawCpuId`, translate MMIO or
//! decide controller reachability. Those are consumer or architecture policy.

use crate::FdtNode;

/// A malformed processor declaration in the validated device tree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CpuNodeError {
    /// The node is a processor, but its identity declaration is malformed.
    InvalidValue,
}

impl core::fmt::Display for CpuNodeError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str("malformed CPU identity declaration")
    }
}

impl core::error::Error for CpuNodeError {}

/// A validated view of one device-tree CPU node.
///
/// Construction only classifies the node as a processor. The identity is read
/// on demand so a CPU node without `reg` remains distinct from a malformed
/// identity declaration.
#[derive(Clone, Copy)]
pub struct CpuNode<'b, 'a> {
    node: FdtNode<'b, 'a>,
}

impl<'b, 'a> CpuNode<'b, 'a> {
    /// Returns `Some` only when `node` declares `device_type = "cpu"`.
    pub fn from_node(node: FdtNode<'b, 'a>) -> Option<Self> {
        (node.property_str("device_type") == Some("cpu")).then_some(Self { node })
    }

    /// Returns the firmware-declared hardware CPU identity.
    ///
    /// The value is read from `reg` using the parent's `#address-cells` width.
    /// A missing `#address-cells` keeps the existing X-Kernel CPU-node
    /// convention of one address cell. Only one- and two-cell identities fit
    /// the `u64` result; other widths, truncated values and payloads with extra
    /// cells are malformed. The identity uses exactly the declared address
    /// cells; a size cell is not part of it.
    ///
    /// `Ok(None)` means the processor declares no identity. Zero is a valid
    /// hardware identity.
    pub fn hardware_id(&self) -> Result<Option<u64>, CpuNodeError> {
        let Some(reg) = self.node.property("reg") else {
            return Ok(None);
        };
        let address_cells = match self.node.parent_property("#address-cells") {
            Some(property) => {
                let bytes: [u8; 4] = property
                    .value
                    .try_into()
                    .map_err(|_| CpuNodeError::InvalidValue)?;
                usize::try_from(u32::from_be_bytes(bytes))
                    .map_err(|_| CpuNodeError::InvalidValue)?
            }
            None => 1,
        };
        if !matches!(address_cells, 1 | 2) {
            return Err(CpuNodeError::InvalidValue);
        }
        let address_bytes = address_cells * 4;
        if reg.value.len() != address_bytes {
            return Err(CpuNodeError::InvalidValue);
        }
        let mut identity = 0u64;
        for cell in reg.value[..address_bytes].chunks_exact(4) {
            let bytes: [u8; 4] = cell.try_into().map_err(|_| CpuNodeError::InvalidValue)?;
            identity = (identity << 32) | u64::from(u32::from_be_bytes(bytes));
        }
        Ok(Some(identity))
    }
}
