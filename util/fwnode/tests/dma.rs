// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use fwnode::{
    FwError, FwIrqSpec, FwMmioResource, FwNode, FwNodeId, FwNodeOps, FwProperty, FwReference,
};

struct NoFirmware;

impl<'a> FwNodeOps<'a> for NoFirmware {
    fn name(&self, _id: FwNodeId) -> Option<&'a str> {
        None
    }

    fn parent(&self, _id: FwNodeId) -> Option<FwNodeId> {
        None
    }

    fn first_child(&self, _id: FwNodeId) -> Option<FwNodeId> {
        None
    }

    fn next_sibling(&self, _id: FwNodeId) -> Option<FwNodeId> {
        None
    }

    fn is_available(&self, _id: FwNodeId) -> Result<bool, FwError> {
        Err(FwError::InvalidNode)
    }

    fn is_interrupt_controller(&self, _id: FwNodeId) -> Result<bool, FwError> {
        Err(FwError::InvalidNode)
    }

    fn firmware_id(&self, _id: FwNodeId, _index: usize) -> Option<&'a str> {
        None
    }

    fn property(&self, _id: FwNodeId, _name: &str) -> Result<Option<FwProperty<'a>>, FwError> {
        Err(FwError::InvalidNode)
    }

    fn mmio_resource(
        &self,
        _id: FwNodeId,
        _index: usize,
    ) -> Result<Option<FwMmioResource<'a>>, FwError> {
        Err(FwError::InvalidNode)
    }

    fn irq_spec(&'a self, _id: FwNodeId, _index: usize) -> Result<Option<FwIrqSpec<'a>>, FwError> {
        Err(FwError::InvalidNode)
    }

    fn reference(
        &'a self,
        _id: FwNodeId,
        _property: &str,
        _index: usize,
        _arg_cells: usize,
    ) -> Result<Option<FwReference<'a>>, FwError> {
        Err(FwError::InvalidNode)
    }
}

#[test]
fn backends_without_dma_facts_do_not_invent_them() {
    let backend = NoFirmware;
    let node = FwNode::new(&backend, FwNodeId::new(7));

    assert_eq!(node.dma_ranges(), Err(FwError::Unsupported));
    assert_eq!(node.dma_range(0), Err(FwError::Unsupported));
    assert_eq!(node.dma_coherency(), Err(FwError::Unsupported));
}
