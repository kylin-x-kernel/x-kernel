// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! GIC firmware-node selection and binding-specific configuration decoding.

use ::fwnode::{FwError, FwMmioResource, FwNode, FwNodeId};
use memaddr::PhysAddr;

use super::{GicConfig, GicMmioRegion, GicRedistributorConfig, GicVersion, MAX_GICR_REGIONS};

const REGION_CAPACITY: usize = 1 + MAX_GICR_REGIONS;

pub(super) fn is_gicv2_compatible(compatible: &str) -> bool {
    matches!(
        compatible,
        "arm,gic-400"
            | "arm,cortex-a15-gic"
            | "arm,cortex-a9-gic"
            | "arm,cortex-a7-gic"
            | "arm,gic-v2"
    )
}

pub(super) fn is_gicv3_compatible(compatible: &str) -> bool {
    matches!(compatible, "arm,gic-v3" | "arm,gic-v4")
}

/// One discovered GIC controller and the firmware node that described it.
///
/// The node is retained so the caller can associate the exact controller
/// instance with its IRQ domain after initialization. Discovery must not drop
/// the node and let the IRQ core guess the controller from a compatible string.
#[derive(Debug, Clone, Copy)]
pub struct GicSelection<'a> {
    /// Controller configuration decoded from the node.
    pub config: GicConfig,
    /// Firmware node the configuration came from.
    pub controller: FwNode<'a>,
}

/// A rejection while translating a firmware node into [`GicConfig`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GicConfigError {
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
    /// A required or optional binding property is malformed.
    Property {
        /// Offending firmware node.
        node: FwNodeId,
        /// Binding property name.
        property: &'static str,
        /// Backend failure.
        error: FwError,
    },
    /// The node does not describe enough MMIO regions for its version.
    MissingRegion {
        /// Offending firmware node.
        node: FwNodeId,
    },
    /// A firmware address or size does not fit the CPU address width.
    AddressWidth {
        /// Offending firmware node.
        node: FwNodeId,
    },
    /// The `#redistributor-regions` declaration is zero.
    InvalidRedistributorCount {
        /// Offending firmware node.
        node: FwNodeId,
    },
    /// The node describes more MMIO regions than this driver can retain.
    TooManyRegions {
        /// Offending firmware node.
        node: FwNodeId,
    },
}

impl GicConfig {
    /// Translates the first available supported GIC node below `root`.
    ///
    /// Returns `Ok(None)` when the tree contains no GIC node with a recognized
    /// compatible string. A recognized but disabled node is skipped, while a
    /// malformed availability property is an error: silently falling back to a
    /// different controller would hide broken firmware.
    ///
    /// # Errors
    ///
    /// Returns [`GicConfigError`] when a candidate's availability, register
    /// resources, or binding properties cannot be interpreted, or when a
    /// GICv3 candidate describes an invalid redistributor region count.
    pub fn from_fwnode<'a>(root: FwNode<'a>) -> Result<Option<GicSelection<'a>>, GicConfigError> {
        for node in root.descendants() {
            let id = node.id();
            if !node
                .is_interrupt_controller()
                .map_err(|error| GicConfigError::Controller { node: id, error })?
            {
                continue;
            }
            let version = if node.firmware_ids().any(is_gicv3_compatible) {
                GicVersion::V3
            } else if node.firmware_ids().any(is_gicv2_compatible) {
                GicVersion::V2
            } else {
                continue;
            };
            if !node
                .is_available()
                .map_err(|error| GicConfigError::Status { node: id, error })?
            {
                continue;
            }
            let regions = read_regions(node)?;
            let config = match version {
                GicVersion::V2 => gicv2_config(id, &regions)?,
                GicVersion::V3 => gicv3_config(node, id, &regions)?,
            };
            return Ok(Some(GicSelection {
                config,
                controller: node,
            }));
        }
        Ok(None)
    }
}

/// MMIO regions read from one node, in firmware order.
struct Regions {
    data: [Option<GicMmioRegion>; REGION_CAPACITY],
    len: usize,
}

impl Regions {
    fn len(&self) -> usize {
        self.len
    }

    fn get(&self, index: usize) -> Option<GicMmioRegion> {
        self.data.get(index).copied().flatten()
    }
}

fn read_regions(node: FwNode<'_>) -> Result<Regions, GicConfigError> {
    let id = node.id();
    let mut data = [None; REGION_CAPACITY];
    let mut len = 0usize;
    loop {
        match node.mmio_resource(len) {
            Ok(Some(resource)) => {
                if len == REGION_CAPACITY {
                    return Err(GicConfigError::TooManyRegions { node: id });
                }
                data[len] = Some(convert_resource(id, resource)?);
                len += 1;
            }
            Ok(None) => break,
            Err(error) => {
                return Err(GicConfigError::Registers { node: id, error });
            }
        }
    }
    Ok(Regions { data, len })
}

fn convert_resource(
    node: FwNodeId,
    resource: FwMmioResource<'_>,
) -> Result<GicMmioRegion, GicConfigError> {
    let width = GicConfigError::AddressWidth { node };
    let base = usize::try_from(resource.base).map_err(|_| width)?;
    let size = usize::try_from(resource.size).map_err(|_| width)?;
    base.checked_add(size).ok_or(width)?;
    Ok(GicMmioRegion {
        paddr: PhysAddr::from_usize(base),
        size,
    })
}

fn gicv2_config(node: FwNodeId, regions: &Regions) -> Result<GicConfig, GicConfigError> {
    let missing = GicConfigError::MissingRegion { node };
    if regions.len() < 2 {
        return Err(missing);
    }
    Ok(GicConfig {
        version: GicVersion::V2,
        gicd: regions.get(0).ok_or(missing)?,
        gicc: Some(regions.get(1).ok_or(missing)?),
        gicr: None,
        gich: regions.get(2),
        gicv: regions.get(3),
    })
}

fn gicv3_config(
    node: FwNode<'_>,
    id: FwNodeId,
    regions: &Regions,
) -> Result<GicConfig, GicConfigError> {
    let count = read_u32_property(node, id, "#redistributor-regions")?.unwrap_or(1);
    let count = usize::try_from(count).map_err(|_| GicConfigError::TooManyRegions { node: id })?;
    if count == 0 {
        return Err(GicConfigError::InvalidRedistributorCount { node: id });
    }
    if count > MAX_GICR_REGIONS {
        return Err(GicConfigError::TooManyRegions { node: id });
    }
    let missing = GicConfigError::MissingRegion { node: id };
    if regions.len() < 1 + count {
        return Err(missing);
    }
    let stride = read_u32_property(node, id, "redistributor-stride")?.unwrap_or(0) as usize;
    let mut gicr_regions = [None; MAX_GICR_REGIONS];
    for (index, slot) in gicr_regions.iter_mut().take(count).enumerate() {
        *slot = Some(regions.get(index + 1).ok_or(missing)?);
    }
    Ok(GicConfig {
        version: GicVersion::V3,
        gicd: regions.get(0).ok_or(missing)?,
        gicc: None,
        gicr: Some(GicRedistributorConfig {
            regions: gicr_regions,
            count,
            stride,
        }),
        gich: None,
        gicv: None,
    })
}

fn read_u32_property(
    node: FwNode<'_>,
    id: FwNodeId,
    property: &'static str,
) -> Result<Option<u32>, GicConfigError> {
    match node.property(property) {
        Ok(Some(value)) => value
            .as_u32()
            .map(Some)
            .map_err(|error| GicConfigError::Property {
                node: id,
                property,
                error,
            }),
        Ok(None) => Ok(None),
        Err(error) => Err(GicConfigError::Property {
            node: id,
            property,
            error,
        }),
    }
}
