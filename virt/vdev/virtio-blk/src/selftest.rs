// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Host-side self-test for the virtio-blk device.
//!
//! Drives [`VirtioBlkDevice`] through the exact MMIO sequence a Linux driver
//! performs — status/feature negotiation, queue programming, `QUEUE_READY`,
//! descriptor-chain submission, `QUEUE_NOTIFY`, used-ring completion and
//! interrupt injection — without creating a VM or running a guest. Guest memory
//! is a host buffer behind a fake [`GuestDma`], and a recording [`IrqSender`]
//! counts used-ring notifications.
//!
//! Run it on the QEMU unittest/kernel-runtime path via
//! `vdev_virtio_blk::selftest::vmm_selftest_virtio_blk()`.

use alloc::{sync::Arc, vec, vec::Vec};

use ksync::Mutex as KsyncMutex;
use vdev_core::{DmaError, GuestDma, IrqSender, MmioDevice};
use vdev_virtio::constants::{
    VIRTIO_STATUS_ACKNOWLEDGE, VIRTIO_STATUS_DRIVER, VIRTIO_STATUS_DRIVER_OK,
    VIRTIO_STATUS_FEATURES_OK, VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE,
    mmio::{
        VIRTIO_MMIO_QUEUE_AVAIL_HIGH, VIRTIO_MMIO_QUEUE_AVAIL_LOW, VIRTIO_MMIO_QUEUE_DESC_HIGH,
        VIRTIO_MMIO_QUEUE_DESC_LOW, VIRTIO_MMIO_QUEUE_NOTIFY, VIRTIO_MMIO_QUEUE_NUM,
        VIRTIO_MMIO_QUEUE_READY, VIRTIO_MMIO_QUEUE_SEL, VIRTIO_MMIO_QUEUE_USED_HIGH,
        VIRTIO_MMIO_QUEUE_USED_LOW, VIRTIO_MMIO_STATUS,
    },
};

use crate::{
    bus::{DEFAULT_CAPACITY_BYTES, VirtioBlkDevice},
    constants::{VIRTIO_BLK_S_OK, VIRTIO_BLK_T_FLUSH, VIRTIO_BLK_T_IN, VIRTIO_BLK_T_OUT},
};

/// Guest memory layout for the selftest (inside the fake 16 MiB guest RAM).
const DESC_TABLE: u64 = 0x1000;
const AVAIL_RING: u64 = 0x1100;
const USED_RING: u64 = 0x1200;
const REQ_HEADER: u64 = 0x2000;
const REQ_DATA: u64 = 0x2100;
const REQ_STATUS: u64 = 0x2200;
const QUEUE_SIZE: u16 = 4;
const SECTOR_BYTES: usize = 512;

/// Fake guest RAM served as the DMA translation for the device under test.
struct TestGuestDma {
    mem: KsyncMutex<Vec<u8>>,
}

impl TestGuestDma {
    fn new() -> Self {
        Self {
            mem: KsyncMutex::new(vec![0u8; DEFAULT_CAPACITY_BYTES as usize]),
        }
    }
}

impl GuestDma for TestGuestDma {
    fn read(&self, gpa: u64, buf: &mut [u8]) -> Result<(), DmaError> {
        let mem = self.mem.lock();
        let start = gpa as usize;
        let end = start
            .checked_add(buf.len())
            .ok_or(DmaError::RangeOverflow)?;
        mem.get(start..end)
            .map(|src| buf.copy_from_slice(src))
            .ok_or(DmaError::AddressFault)
    }

    fn write(&self, gpa: u64, buf: &[u8]) -> Result<(), DmaError> {
        let mut mem = self.mem.lock();
        let start = gpa as usize;
        let end = start
            .checked_add(buf.len())
            .ok_or(DmaError::RangeOverflow)?;
        mem.get_mut(start..end)
            .map(|dst| dst.copy_from_slice(buf))
            .ok_or(DmaError::AddressFault)
    }
}

/// Records injected interrupts instead of wiring them to a vGIC.
#[derive(Default)]
struct RecordingIrqSender {
    injections: KsyncMutex<Vec<(u32, u32)>>,
}

impl IrqSender for RecordingIrqSender {
    fn inject(&self, vcpu_id: u32, irq: u32) {
        self.injections.lock().push((vcpu_id, irq));
    }
}

impl RecordingIrqSender {
    fn count(&self) -> usize {
        self.injections.lock().len()
    }
}

/// Bind one descriptor in the fake guest memory.
fn put_desc(guest: &TestGuestDma, index: usize, addr: u64, len: u32, flags: u16, next: u16) {
    let offset = DESC_TABLE + index as u64 * 16;
    let mut desc = [0u8; 16];
    desc[0..8].copy_from_slice(&addr.to_le_bytes());
    desc[8..12].copy_from_slice(&len.to_le_bytes());
    desc[12..14].copy_from_slice(&flags.to_le_bytes());
    desc[14..16].copy_from_slice(&next.to_le_bytes());
    guest.write(offset, &desc).expect("descriptor write");
}

/// Append `head` to the available ring and advance its idx to `next_idx`.
fn avail_push(guest: &TestGuestDma, next_idx: u16, head: u16) {
    let entry = AVAIL_RING + 4 + ((next_idx - 1) % QUEUE_SIZE) as u64 * 2;
    guest
        .write(entry, &head.to_le_bytes())
        .expect("available entry write");
    guest
        .write(AVAIL_RING + 2, &next_idx.to_le_bytes())
        .expect("available idx write");
}

/// Program a header+data+status chain and notify; the last descriptor index
/// owned by this helper is written to the status slot.
fn submit(guest: &TestGuestDma, request_type: u32, data_len: u32, write_to_device: bool) {
    let mut header = [0u8; 16];
    header[0..4].copy_from_slice(&request_type.to_le_bytes());
    // sector stays 0 (first 512-byte sector of the RAM disk).
    guest.write(REQ_HEADER, &header).expect("header write");
    if write_to_device {
        let mut data = vec![0u8; SECTOR_BYTES];
        data[..7].copy_from_slice(b"virtio!"); // recognizable write payload
        guest.write(REQ_DATA, &data).expect("write data");
    }
    put_desc(guest, 0, REQ_HEADER, 16, VIRTQ_DESC_F_NEXT, 1);
    if data_len > 0 {
        let base = if write_to_device {
            0
        } else {
            VIRTQ_DESC_F_WRITE
        };
        let data_flags = base | VIRTQ_DESC_F_NEXT;
        put_desc(guest, 1, REQ_DATA, data_len, data_flags, 2);
        put_desc(guest, 2, REQ_STATUS, 1, VIRTQ_DESC_F_WRITE, 0);
    } else {
        // FLUSH style: header + status only.
        put_desc(guest, 1, REQ_STATUS, 1, VIRTQ_DESC_F_WRITE, 0);
    }
    guest.write(REQ_STATUS, &[0xff]).expect("status write");
}

/// Drive one request through the device and validate status, used ring and
/// interrupt injection.
fn drive_and_check(
    dev: &mut VirtioBlkDevice,
    guest: &TestGuestDma,
    sender: &RecordingIrqSender,
    avail_idx: u16,
    expected_used_idx: u16,
    expected_used_len: u32,
) {
    avail_push(guest, avail_idx, 0);
    let injections_before = sender.count();
    dev.write_for_vcpu(VIRTIO_MMIO_QUEUE_NOTIFY as u64, 4, 0, 0);

    let mut status = [0xffu8; 1];
    guest.read(REQ_STATUS, &mut status).expect("status read");
    assert_eq!(status[0], VIRTIO_BLK_S_OK as u8, "request must complete OK");

    let mut used_idx = [0u8; 2];
    guest
        .read(USED_RING + 2, &mut used_idx)
        .expect("used idx read");
    assert_eq!(
        u16::from_le_bytes(used_idx),
        expected_used_idx,
        "used ring idx must advance once per request",
    );
    let mut elem = [0u8; 8];
    guest
        .read(
            USED_RING + 4 + (expected_used_idx - 1) as u64 * 8,
            &mut elem,
        )
        .expect("used elem read");
    assert_eq!(
        u32::from_le_bytes(elem[0..4].try_into().unwrap()),
        0,
        "used elem id"
    );
    assert_eq!(
        u32::from_le_bytes(elem[4..8].try_into().unwrap()),
        expected_used_len,
        "used elem len",
    );
    assert!(
        sender.count() > injections_before,
        "each completed request must inject a used-ring interrupt",
    );
}

/// Run the virtio-blk device self-test.
///
/// Returns `true` when the whole driver sequence — negotiation, queue
/// programming, OUT/IN/FLUSH data path, used-ring accounting and interrupt
/// injection — behaves as a Linux virtio-blk driver expects.
pub fn vmm_selftest_virtio_blk() -> bool {
    log::warn!("[vmm] === VirtIO Block Device Self-Test ===");
    let guest = Arc::new(TestGuestDma::new());
    let dma: Arc<dyn GuestDma> = guest.clone();
    let sender = Arc::new(RecordingIrqSender::default());
    let mut dev = VirtioBlkDevice::new(sender.clone(), dma, DEFAULT_CAPACITY_BYTES);

    // ── Transport bring-up: status / feature negotiation ──
    for status in [
        VIRTIO_STATUS_ACKNOWLEDGE,
        VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER,
        VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER | VIRTIO_STATUS_FEATURES_OK,
        VIRTIO_STATUS_ACKNOWLEDGE
            | VIRTIO_STATUS_DRIVER
            | VIRTIO_STATUS_FEATURES_OK
            | VIRTIO_STATUS_DRIVER_OK,
    ] {
        dev.write_for_vcpu(VIRTIO_MMIO_STATUS as u64, 4, status as u64, 0);
        assert_eq!(
            dev.read(VIRTIO_MMIO_STATUS as u64, 4),
            status as u64,
            "device status must track driver writes",
        );
    }

    // ── Queue programming ──
    dev.write_for_vcpu(VIRTIO_MMIO_QUEUE_SEL as u64, 4, 0, 0);
    dev.write_for_vcpu(VIRTIO_MMIO_QUEUE_NUM as u64, 4, QUEUE_SIZE as u64, 0);
    dev.write_for_vcpu(VIRTIO_MMIO_QUEUE_DESC_LOW as u64, 4, DESC_TABLE, 0);
    dev.write_for_vcpu(VIRTIO_MMIO_QUEUE_DESC_HIGH as u64, 4, 0, 0);
    dev.write_for_vcpu(VIRTIO_MMIO_QUEUE_AVAIL_LOW as u64, 4, AVAIL_RING, 0);
    dev.write_for_vcpu(VIRTIO_MMIO_QUEUE_AVAIL_HIGH as u64, 4, 0, 0);
    dev.write_for_vcpu(VIRTIO_MMIO_QUEUE_USED_LOW as u64, 4, USED_RING, 0);
    dev.write_for_vcpu(VIRTIO_MMIO_QUEUE_USED_HIGH as u64, 4, 0, 0);
    dev.write_for_vcpu(VIRTIO_MMIO_QUEUE_READY as u64, 4, 1, 0);
    assert_eq!(
        dev.read(VIRTIO_MMIO_QUEUE_READY as u64, 4),
        1,
        "QUEUE_READY must be visible once the layout is accepted",
    );

    // ── Data path: write, read, flush ──
    // 1. OUT: guest writes sector 0.
    submit(&guest, VIRTIO_BLK_T_OUT, SECTOR_BYTES as u32, true);
    drive_and_check(&mut dev, &guest, &sender, 1, 1, 1);

    // 2. IN: device reads sector 0 back into the data buffer.
    submit(&guest, VIRTIO_BLK_T_IN, SECTOR_BYTES as u32, false);
    drive_and_check(&mut dev, &guest, &sender, 2, 2, SECTOR_BYTES as u32 + 1);
    let mut readback = vec![0u8; SECTOR_BYTES];
    guest
        .read(REQ_DATA, &mut readback)
        .expect("read data buffer");
    assert_eq!(
        &readback[..7],
        b"virtio!",
        "IN must return the sector written by OUT",
    );

    // 3. FLUSH: header (type FLUSH) + status only.
    let mut flush_header = [0u8; 16];
    flush_header[0..4].copy_from_slice(&VIRTIO_BLK_T_FLUSH.to_le_bytes());
    guest
        .write(REQ_HEADER, &flush_header)
        .expect("flush header");
    put_desc(&guest, 0, REQ_HEADER, 16, VIRTQ_DESC_F_NEXT, 1);
    put_desc(&guest, 1, REQ_STATUS, 1, VIRTQ_DESC_F_WRITE, 0);
    guest.write(REQ_STATUS, &[0xff]).expect("flush status");
    drive_and_check(&mut dev, &guest, &sender, 3, 3, 1);

    log::info!(
        "[vmm] virtio-blk selftest: {} used-ring interrupts injected",
        sender.count(),
    );

    log::info!("[vmm] === VirtIO Block Device Self-Test: PASSED ===");
    true
}
