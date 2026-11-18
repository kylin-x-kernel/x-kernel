// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec;

use unittest::{assert, assert_eq};
use virtio_drivers::{config::ReadOnly, transport::DeviceType};
use vsock::{VsockAddr, VsockConnId, VsockConnectionInfo};

use super::*;
use crate::mock_virtio::{MockHal, MockTransport};

fn test_device() -> VirtIoVsockDev<MockHal, MockTransport> {
    let mut transport = MockTransport::new_with_type(DeviceType::Socket);
    transport.max_queue_size = QUEUE_SIZE as u32;
    transport
        .write_config_space(0, ReadOnly::new(66u32))
        .unwrap();
    VirtIoVsockDev::<MockHal, MockTransport>::try_new(transport).unwrap()
}

fn test_connection_info() -> VsockConnectionInfo {
    let mut info = VsockConnectionInfo::new(VsockConnId {
        peer_addr: VsockAddr { cid: 2, port: 1234 },
        local_port: 4321,
    });
    info.buf_alloc = 1024;
    info.peer_buf_alloc = 1024;
    info
}

fn test_connection() -> ConnectionInfo {
    VirtIoVsockDev::<MockHal, MockTransport>::to_connection_info(&test_connection_info())
}


fn fill_tx_buf(dev: &VirtIoVsockDev<MockHal, MockTransport>, payload: &[u8]) -> VsockTxBuf {
    let mut tx_buf = dev.alloc_tx_buf().unwrap();
    let payload_len = payload.len().min(tx_buf.payload_capacity());
    tx_buf.set_payload_len(payload_len).unwrap();
    tx_buf
        .payload_mut()
        .copy_from_slice(&payload[..payload_len]);
    tx_buf
}

fn send_test_bytes(
    dev: &VirtIoVsockDev<MockHal, MockTransport>,
    info: &VsockConnectionInfo,
    payload: &[u8],
) -> DriverResult<usize> {
    let tx_buf = fill_tx_buf(dev, payload);
    let payload_len = tx_buf.payload_len();
    dev.submit_tx_buf(info, tx_buf)?;
    Ok(payload_len)
}

#[unittest::def_test]
fn tx_buf_owns_its_payload() {
    let dev = test_device();
    let mut payload = vec![1, 2, 3, 4];
    let mut tx_buf = dev.alloc_tx_buf().unwrap();
    tx_buf.set_payload_len(payload.len()).unwrap();
    tx_buf.payload_mut().copy_from_slice(&payload);
    payload[0] = 9;

    assert_eq!(tx_buf.payload(), &[1, 2, 3, 4]);
    dev.release_tx_buf(tx_buf);
}

#[unittest::def_test]
fn tx_bufs_can_be_in_flight_together() {
    let dev = test_device();
    let connection_info = test_connection();

    let first_token = dev
        .queue_tx_buf(
            fill_tx_buf(&dev, b"first"),
            &connection_info,
            VsockTxPacketKind::Data { payload_len: 5 },
        )
        .unwrap();
    let second_token = dev
        .queue_tx_buf(
            fill_tx_buf(&dev, b"second"),
            &connection_info,
            VsockTxPacketKind::Data { payload_len: 6 },
        )
        .unwrap();

    assert_ne!(first_token, second_token);
    let inner = dev.inner.lock();
    let bufs = dev.tx_bufs.lock();
    assert!(bufs.tx_bufs[usize::from(first_token)].is_some());
    assert!(bufs.tx_bufs[usize::from(second_token)].is_some());
    assert_eq!(inner.available_tx_descriptors(), QUEUE_SIZE - 2);
}

#[unittest::def_test]
fn data_send_returns_after_submission() {
    let dev = test_device();
    let info = test_connection_info();

    assert_eq!(send_test_bytes(&dev, &info, b"hello"), Ok(5));
    assert_eq!(
        dev.tx_bufs
            .lock()
            .tx_bufs
            .iter()
            .filter(|packet| packet.is_some())
            .count(),
        1
    );
}

#[unittest::def_test]
fn data_send_is_all_or_nothing_for_peer_credit() {
    let dev = test_device();
    let mut info = test_connection_info();
    info.peer_buf_alloc = 100;
    let credit_limited = vec![0; 200];

    assert!(matches!(
        send_test_bytes(&dev, &info, &credit_limited),
        Err(DriverError::WouldBlock)
    ));
    assert_eq!(
        dev.tx_bufs
            .lock()
            .tx_bufs
            .iter()
            .filter(|packet| packet.is_some())
            .count(),
        0
    );

    assert_eq!(send_test_bytes(&dev, &info, &credit_limited[..100]), Ok(100));
    assert_eq!(
        dev.tx_bufs
            .lock()
            .tx_bufs
            .iter()
            .find_map(|packet| packet.as_ref())
            .unwrap()
            .frame()
            .len(),
        VSOCK_HEADER_SIZE + 100
    );
}

#[unittest::def_test]
fn data_send_caps_packet_at_stream_chunk_size() {
    let dev = test_device();
    let mut info = test_connection_info();
    info.peer_buf_alloc = 256 * 1024;
    let oversized = vec![0; 100 * 1024];

    // The helper mirrors the stream layer's 64 KiB chunking, not the
    // transport-side rejection covered by the next test.
    assert_eq!(send_test_bytes(&dev, &info, &oversized), Ok(64 * 1024));
}

#[unittest::def_test]
fn submit_tx_buf_rejects_payload_above_transport_limit() {
    let dev = test_device();
    let mut info = test_connection_info();
    info.peer_buf_alloc = 256 * 1024;

    // Reset the header to zero so the payload can exceed TX_MAX_PAYLOAD_SIZE
    // while still fitting within the slot.  submit_tx_buf must reject this at
    // the transport layer (payload_len > TX_MAX_PAYLOAD_SIZE), before the
    // packet reaches the virtqueue.
    let mut tx_buf = dev.alloc_tx_buf().unwrap();
    tx_buf.reset();
    tx_buf.set_payload_len(TX_MAX_PAYLOAD_SIZE + 1).unwrap();

    assert!(matches!(
        dev.submit_tx_buf(&info, tx_buf),
        Err(DriverError::InvalidInput)
    ));
    assert_eq!(
        dev.tx_bufs
            .lock()
            .tx_bufs
            .iter()
            .filter(|packet| packet.is_some())
            .count(),
        0
    );
    // The rejected buffer is returned to the transport free list.
    assert!(dev.alloc_tx_buf().is_ok());
}

#[unittest::def_test]
fn data_send_reserves_packet_capacity_for_control() {
    let dev = test_device();
    let info = test_connection_info();
    let mut data_packets = Vec::new();

    for _ in 0..(QUEUE_SIZE - TX_CONTROL_RESERVED_BUFS) {
        data_packets.push(dev.alloc_tx_buf().unwrap());
    }
    assert!(matches!(dev.alloc_tx_buf(), Err(DriverError::WouldBlock)));

    let control_buf = dev.take_free_tx_buf(false).unwrap();
    assert!(matches!(
        dev.take_free_tx_buf(false),
        Err(DriverError::WouldBlock)
    ));
    dev.release_tx_buf(control_buf);
    for tx_buf in data_packets {
        dev.release_tx_buf(tx_buf);
    }
    assert_eq!(send_test_bytes(&dev, &info, b"x"), Ok(1));
}

#[unittest::def_test]
fn tx_buf_pool_reuses_fixed_slot_storage_and_is_bounded() {
    let dev = test_device();
    let payload = vec![0x5a; 64 * 1024];
    let tx_buf = fill_tx_buf(&dev, &payload);
    assert_eq!(tx_buf.payload_len(), 64 * 1024);
    dev.release_tx_buf(tx_buf);

    let mut tx_bufs = Vec::new();
    for _ in 0..QUEUE_SIZE {
        tx_bufs.push(dev.take_free_tx_buf(false).unwrap());
    }
    assert!(matches!(
        dev.take_free_tx_buf(false),
        Err(DriverError::WouldBlock)
    ));
    for tx_buf in tx_bufs {
        dev.release_tx_buf(tx_buf);
    }
    assert!(dev.can_send());
}
