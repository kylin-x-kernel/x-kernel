// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Mock block request and reset regressions in the kernel test harness.

use alloc::{sync::Arc, vec, vec::Vec};

use vdev_virtio::{
    GuestMemory, NoGuestMemoryAccessor,
    constants::{VIRTIO_MMIO_STATUS, VIRTQ_DESC_F_NEXT},
};

use super::*;

const DESC_TABLE: usize = 0x100;
const HEADER: usize = 0x300;
const DATA: usize = 0x400;
const STATUS: usize = 0x800;
const VIRTQ_DESC_F_WRITE: u16 = 2;
const IOERR: u8 = 1;

struct TestMemory(Vec<u8>);

impl TestMemory {
    fn new() -> Self {
        Self(vec![0; 0x1000])
    }

    fn set_descriptor(&mut self, index: usize, addr: usize, len: u32, flags: u16, next: u16) {
        let offset = DESC_TABLE + index * 16;
        self.0[offset..offset + 8].copy_from_slice(&(addr as u64).to_le_bytes());
        self.0[offset + 8..offset + 12].copy_from_slice(&len.to_le_bytes());
        self.0[offset + 12..offset + 14].copy_from_slice(&flags.to_le_bytes());
        self.0[offset + 14..offset + 16].copy_from_slice(&next.to_le_bytes());
    }

    fn set_header(&mut self, request_type: u32, sector: u64) {
        self.0[HEADER..HEADER + 4].copy_from_slice(&request_type.to_le_bytes());
        self.0[HEADER + 8..HEADER + 16].copy_from_slice(&sector.to_le_bytes());
    }
}

impl GuestMemory for TestMemory {
    fn read(&mut self, guest_addr: GuestPhysAddr, data: &mut [u8]) -> VirtioResult<()> {
        let start = guest_addr.as_usize();
        let source = self
            .0
            .get(start..start + data.len())
            .ok_or(VirtioError::InvalidAddress)?;
        data.copy_from_slice(source);
        Ok(())
    }

    fn write(&mut self, guest_addr: GuestPhysAddr, data: &[u8]) -> VirtioResult<()> {
        let start = guest_addr.as_usize();
        let destination = self
            .0
            .get_mut(start..start + data.len())
            .ok_or(VirtioError::InvalidAddress)?;
        destination.copy_from_slice(data);
        Ok(())
    }
}

struct TestBackend;

impl BlockBackend for TestBackend {
    fn read(&self, sector: u64, buffer: &mut [u8]) -> VirtioResult<usize> {
        if sector >= 8 {
            return Err(VirtioError::InvalidSector);
        }
        buffer.fill(0x5a);
        Ok(buffer.len())
    }

    fn write(&self, sector: u64, buffer: &[u8]) -> VirtioResult<usize> {
        if sector >= 8 {
            return Err(VirtioError::InvalidSector);
        }
        Ok(buffer.len())
    }

    fn flush(&self) -> VirtioResult<()> {
        Ok(())
    }
}

fn fixture(
    data_len: u32,
    sector: u64,
) -> (
    VirtioMmioBlockDevice<TestBackend, NoGuestMemoryAccessor>,
    VirtioQueue<NoGuestMemoryAccessor>,
    TestMemory,
) {
    let config = VirtioBlockConfig {
        capacity: 8,
        size_max: 64,
        seg_max: 1,
        ..VirtioBlockConfig::default()
    };
    let device = VirtioMmioBlockDevice::new(
        GuestPhysAddr::from(0x0a00_0000),
        0x200,
        TestBackend,
        config,
        NoGuestMemoryAccessor,
    )
    .unwrap();
    let accessor = Arc::new(NoGuestMemoryAccessor);
    let mut queue = VirtioQueue::new(0, 4, accessor);
    queue
        .set_desc_table_addr(GuestPhysAddr::from(DESC_TABLE))
        .unwrap();
    let mut memory = TestMemory::new();
    memory.set_descriptor(0, HEADER, VirtioBlockHeader::SIZE, VIRTQ_DESC_F_NEXT, 1);
    memory.set_descriptor(1, DATA, data_len, VIRTQ_DESC_F_NEXT, 2);
    memory.set_descriptor(2, STATUS, 1, VIRTQ_DESC_F_WRITE, 0);
    memory.set_header(VIRTIO_BLK_T_OUT, sector);
    memory.0[STATUS] = 0xff;
    (device, queue, memory)
}

#[unittest::def_test]
fn oversized_segment_completes_with_ioerr_without_allocating_guest_length() {
    let (device, queue, mut memory) = fixture(65, 0);

    assert_eq!(device.process_request(&queue, 0, &mut memory), Ok(Some(1)));
    assert_eq!(memory.0[STATUS], IOERR);
}

#[unittest::def_test]
fn out_of_capacity_request_completes_with_ioerr() {
    let (device, queue, mut memory) = fixture(64, 8);

    assert_eq!(device.process_request(&queue, 0, &mut memory), Ok(Some(1)));
    assert_eq!(memory.0[STATUS], IOERR);
}

#[unittest::def_test]
fn reset_discards_deferred_request_head() {
    let (device, _queue, mut memory) = fixture(64, 0);
    *device.pending_head.lock() = Some(3);

    let event = device.mmio_write_with_memory(
        GuestPhysAddr::from(0x0a00_0000 + VIRTIO_MMIO_STATUS),
        AccessWidth::Dword,
        0,
        &mut memory,
    );

    assert_eq!(event, Ok(BlockDeviceEvent::Reset));
    assert_eq!(*device.pending_head.lock(), None);
}
