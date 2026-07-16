// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};

use kerrno::KError;
use unittest::def_test;

use crate::{
    Handle, HandleEventMask, HandleKind, HandleSet, HandleSetCommand, HandleSetEntry, HandleTable,
    IPC_CHAN_MAX_BUF_SIZE, IPC_CHAN_MAX_BUFS, IPC_MAX_MSG_HANDLES, IPC_PORT_PATH_MAX,
    IpcConnectFlags, IpcPortFlags, IpcUuid, MemRef, ipc_port_connect_async, ipc_port_create,
    ipc_port_publish,
};

#[def_test]
fn ipc_port_create_validates_trusty_limits() {
    let uuid = IpcUuid::from_bytes([1; 16]);
    assert_eq!(
        ipc_port_create(
            uuid,
            "com.xkernel.test.tipc.create".to_string(),
            IPC_CHAN_MAX_BUFS,
            IPC_CHAN_MAX_BUF_SIZE,
            IpcPortFlags::ALLOW_TA_CONNECT,
        )
        .map(|_| ()),
        Ok(())
    );
    assert_eq!(
        ipc_port_create(uuid, String::new(), 1, 32, IpcPortFlags::ALLOW_TA_CONNECT).map(|_| ()),
        Err(KError::InvalidInput)
    );
    assert_eq!(
        ipc_port_create(
            uuid,
            "com.xkernel\0test".to_string(),
            1,
            32,
            IpcPortFlags::ALLOW_TA_CONNECT,
        )
        .map(|_| ()),
        Err(KError::InvalidInput)
    );
    assert_eq!(
        ipc_port_create(
            uuid,
            "a".repeat(IPC_PORT_PATH_MAX),
            1,
            32,
            IpcPortFlags::ALLOW_TA_CONNECT,
        )
        .map(|_| ()),
        Err(KError::OutOfRange)
    );
    assert_eq!(
        ipc_port_create(
            uuid,
            "com.xkernel.test.tipc.too-many-bufs".to_string(),
            IPC_CHAN_MAX_BUFS + 1,
            32,
            IpcPortFlags::ALLOW_TA_CONNECT,
        )
        .map(|_| ()),
        Err(KError::InvalidInput)
    );
    assert_eq!(
        ipc_port_create(
            uuid,
            "com.xkernel.test.tipc.too-large-buf".to_string(),
            1,
            IPC_CHAN_MAX_BUF_SIZE + 1,
            IpcPortFlags::ALLOW_TA_CONNECT,
        )
        .map(|_| ()),
        Err(KError::InvalidInput)
    );
    assert_eq!(
        ipc_port_connect_async(
            uuid,
            "com.xkernel\0test",
            IpcConnectFlags::WAIT_FOR_PORT | IpcConnectFlags::ASYNC,
        )
        .map(|_| ()),
        Err(KError::InvalidInput)
    );
}

#[def_test]
fn port_registry_rejects_duplicate_live_port_and_allows_republish_after_close() {
    let path = "com.xkernel.test.tipc.duplicate-port";
    let first = ipc_port_create(
        IpcUuid::from_bytes([21; 16]),
        path.to_string(),
        1,
        32,
        IpcPortFlags::ALLOW_TA_CONNECT,
    )
    .unwrap();
    let second = ipc_port_create(
        IpcUuid::from_bytes([22; 16]),
        path.to_string(),
        1,
        32,
        IpcPortFlags::ALLOW_TA_CONNECT,
    )
    .unwrap();

    ipc_port_publish(&first).unwrap();
    assert_eq!(ipc_port_publish(&second), Err(KError::AlreadyExists));

    first.close();
    ipc_port_publish(&second).unwrap();
    second.close();
}

#[def_test]
fn connect_requires_existing_port_unless_wait_for_port_is_requested() {
    let path = "com.xkernel.test.tipc.missing-port";
    assert_eq!(
        ipc_port_connect_async(IpcUuid::from_bytes([23; 16]), path, IpcConnectFlags::ASYNC)
            .map(|_| ()),
        Err(KError::NotFound)
    );

    let client = ipc_port_connect_async(
        IpcUuid::from_bytes([24; 16]),
        path,
        IpcConnectFlags::WAIT_FOR_PORT | IpcConnectFlags::ASYNC,
    )
    .unwrap();
    assert_eq!(client.state(), crate::IpcChanState::Connecting);
    client.close();
}

#[def_test]
fn channel_message_lifecycle_and_backpressure() {
    let port = ipc_port_create(
        IpcUuid::from_bytes([1; 16]),
        "com.xkernel.test.tipc.lifecycle".to_string(),
        1,
        32,
        IpcPortFlags::ALLOW_TA_CONNECT,
    )
    .unwrap();
    ipc_port_publish(&port).unwrap();

    let client = ipc_port_connect_async(
        IpcUuid::from_bytes([2; 16]),
        port.path(),
        IpcConnectFlags::ASYNC,
    )
    .unwrap();
    assert!(port.poll(false).contains(HandleEventMask::READY));

    let (server, peer) = port.ipc_port_accept().unwrap();
    assert_eq!(peer, IpcUuid::from_bytes([2; 16]));
    assert!(client.poll(true).contains(HandleEventMask::READY));

    assert_eq!(client.ipc_send_msg(b"first").unwrap(), 5);
    assert!(server.poll(false).contains(HandleEventMask::MSG));
    assert_eq!(client.ipc_send_msg(b"second"), Err(KError::WouldBlock));

    let info = server.ipc_get_msg().unwrap();
    let mut out = [0u8; 8];
    assert_eq!(server.ipc_read_msg(info.id, 0, &mut out).unwrap(), 5);
    assert_eq!(&out[..5], b"first");
    server.ipc_put_msg(info.id).unwrap();

    assert!(client.poll(true).contains(HandleEventMask::SEND_UNBLOCKED));
    assert_eq!(client.ipc_send_msg(b"second").unwrap(), 6);

    server.close();
    assert!(client.poll(false).contains(HandleEventMask::HUP));
    port.close();
}

#[def_test]
fn message_queue_rejects_invalid_state_and_bounds() {
    let port = ipc_port_create(
        IpcUuid::from_bytes([25; 16]),
        "com.xkernel.test.tipc.message-errors".to_string(),
        1,
        4,
        IpcPortFlags::ALLOW_TA_CONNECT,
    )
    .unwrap();
    ipc_port_publish(&port).unwrap();
    let client = ipc_port_connect_async(
        IpcUuid::from_bytes([26; 16]),
        port.path(),
        IpcConnectFlags::ASYNC,
    )
    .unwrap();
    let (server, _) = port.ipc_port_accept().unwrap();

    assert_eq!(client.ipc_send_msg(b"too-large"), Err(KError::OutOfRange));
    assert_eq!(server.ipc_get_msg(), Err(KError::WouldBlock));
    assert_eq!(client.peer_recv_buf_size().unwrap(), 4);
    assert_eq!(server.peer_recv_buf_size().unwrap(), 4);

    client.ipc_send_msg(b"ok").unwrap();
    let mut out = [0u8; 4];
    assert_eq!(server.ipc_read_msg(0, 0, &mut out), Err(KError::BadState));

    let info = server.ipc_get_msg().unwrap();
    assert_eq!(server.ipc_read_msg(99, 0, &mut out), Err(KError::NotFound));
    assert_eq!(
        server.ipc_read_msg(info.id, info.len + 1, &mut out),
        Err(KError::OutOfRange)
    );
    assert_eq!(
        server
            .ipc_read_msg_handles(info.id, IPC_MAX_MSG_HANDLES + 1)
            .map(|_| ()),
        Err(KError::OutOfRange)
    );

    server.ipc_put_msg(info.id).unwrap();
    assert_eq!(server.ipc_put_msg(info.id), Err(KError::BadState));

    server.close();
    port.close();
}

#[def_test]
fn async_connect_waits_without_blocking_for_port_publish() {
    let path = "com.xkernel.test.tipc.wait-for-port";
    let client = ipc_port_connect_async(
        IpcUuid::from_bytes([3; 16]),
        path,
        IpcConnectFlags::WAIT_FOR_PORT | IpcConnectFlags::ASYNC,
    )
    .unwrap();
    assert_eq!(client.state(), crate::IpcChanState::Connecting);

    let port = ipc_port_create(
        IpcUuid::from_bytes([4; 16]),
        path.to_string(),
        1,
        32,
        IpcPortFlags::ALLOW_TA_CONNECT,
    )
    .unwrap();
    ipc_port_publish(&port).unwrap();
    assert!(port.poll(false).contains(HandleEventMask::READY));

    let (server, peer) = port.ipc_port_accept().unwrap();
    assert_eq!(peer, IpcUuid::from_bytes([3; 16]));
    assert_eq!(server.uuid(), IpcUuid::from_bytes([4; 16]));
    assert!(client.poll(true).contains(HandleEventMask::READY));
    assert_eq!(client.state(), crate::IpcChanState::Connected);

    server.close();
    port.close();
}

#[def_test]
fn port_flags_gate_ta_and_ns_clients() {
    let ns_only = ipc_port_create(
        IpcUuid::from_bytes([5; 16]),
        "com.xkernel.test.tipc.ns-only".to_string(),
        1,
        32,
        IpcPortFlags::ALLOW_NS_CONNECT,
    )
    .unwrap();
    ipc_port_publish(&ns_only).unwrap();

    assert_eq!(
        ipc_port_connect_async(
            IpcUuid::from_bytes([6; 16]),
            ns_only.path(),
            IpcConnectFlags::ASYNC,
        )
        .map(|_| ()),
        Err(KError::PermissionDenied)
    );
    assert!(
        ipc_port_connect_async(IpcUuid::default(), ns_only.path(), IpcConnectFlags::ASYNC).is_ok()
    );
    ns_only.close();
}

#[def_test]
fn handle_set_is_not_sendable() {
    let port = ipc_port_create(
        IpcUuid::from_bytes([7; 16]),
        "com.xkernel.test.tipc.no-send".to_string(),
        1,
        32,
        IpcPortFlags::ALLOW_TA_CONNECT,
    )
    .unwrap();
    ipc_port_publish(&port).unwrap();
    let client = ipc_port_connect_async(
        IpcUuid::from_bytes([8; 16]),
        port.path(),
        IpcConnectFlags::ASYNC,
    )
    .unwrap();
    let (server, _) = port.ipc_port_accept().unwrap();

    let hset: alloc::sync::Arc<dyn Handle> = HandleSet::handle_set_create();
    assert_eq!(
        client.ipc_send_msg_with_handles(b"", &[hset]),
        Err(KError::PermissionDenied)
    );

    server.close();
    port.close();
}

#[def_test]
fn message_handle_transfer_rejects_too_many_handles() {
    let port = ipc_port_create(
        IpcUuid::from_bytes([27; 16]),
        "com.xkernel.test.tipc.too-many-handles".to_string(),
        1,
        32,
        IpcPortFlags::ALLOW_TA_CONNECT,
    )
    .unwrap();
    ipc_port_publish(&port).unwrap();
    let client = ipc_port_connect_async(
        IpcUuid::from_bytes([28; 16]),
        port.path(),
        IpcConnectFlags::ASYNC,
    )
    .unwrap();
    let (server, _) = port.ipc_port_accept().unwrap();

    let mut handles: Vec<Arc<dyn Handle>> = Vec::new();
    for idx in 0..=IPC_MAX_MSG_HANDLES {
        handles.push(
            MemRef::create(0x1000 + idx * 0x1000, 0x1000, crate::MMAP_FLAG_PROT_READ).unwrap(),
        );
    }
    assert_eq!(
        client.ipc_send_msg_with_handles(b"", &handles),
        Err(KError::OutOfRange)
    );

    server.close();
    port.close();
}

#[def_test]
fn handle_set_matches_trusty_empty_and_nested_rules() {
    let hset = HandleSet::handle_set_create();
    assert_eq!(hset.poll_one(), Err(KError::NotFound));

    let child = HandleSet::handle_set_create();
    assert_eq!(
        hset.handle_set_ctrl(
            HandleSetCommand::Add,
            HandleSetEntry {
                handle_id: 10,
                handle: child as Arc<dyn Handle>,
                event: HandleEventMask::READY,
                cookie: 0,
            },
        ),
        Err(KError::InvalidInput)
    );
}

#[def_test]
fn handle_set_reports_registered_ready_handle() {
    let port = ipc_port_create(
        IpcUuid::from_bytes([11; 16]),
        "com.xkernel.test.tipc.hset-ready".to_string(),
        1,
        32,
        IpcPortFlags::ALLOW_TA_CONNECT,
    )
    .unwrap();
    ipc_port_publish(&port).unwrap();
    let client = ipc_port_connect_async(
        IpcUuid::from_bytes([12; 16]),
        port.path(),
        IpcConnectFlags::ASYNC,
    )
    .unwrap();

    let hset = HandleSet::handle_set_create();
    hset.handle_set_ctrl(
        HandleSetCommand::Add,
        HandleSetEntry {
            handle_id: 42,
            handle: port.clone() as Arc<dyn Handle>,
            event: HandleEventMask::READY,
            cookie: 0x55,
        },
    )
    .unwrap();

    let event = hset.poll_one().unwrap().unwrap();
    assert_eq!(event.handle, 42);
    assert_eq!(event.event, HandleEventMask::READY);
    assert_eq!(event.cookie, 0x55);

    client.close();
    port.close();
}

#[def_test]
fn handle_table_close_detaches_handle_set_entries() {
    let hset = HandleSet::handle_set_create();
    let port = ipc_port_create(
        IpcUuid::from_bytes([13; 16]),
        "com.xkernel.test.tipc.hset-detach".to_string(),
        1,
        32,
        IpcPortFlags::ALLOW_TA_CONNECT,
    )
    .unwrap();

    let mut table = HandleTable::new();
    let _hset_id = table
        .uctx_handle_install(hset.clone() as Arc<dyn Handle>)
        .unwrap();
    let port_id = table
        .uctx_handle_install(port.clone() as Arc<dyn Handle>)
        .unwrap();

    hset.handle_set_ctrl(
        HandleSetCommand::Add,
        HandleSetEntry {
            handle_id: port_id,
            handle: port as Arc<dyn Handle>,
            event: HandleEventMask::ERROR,
            cookie: 0x66,
        },
    )
    .unwrap();
    assert!(hset.poll_one().unwrap().is_some());

    table.uctx_handle_remove(port_id).unwrap();
    assert_eq!(hset.poll_one(), Err(KError::NotFound));
}

#[def_test]
fn handle_table_forgets_removed_handle_sets() {
    let hset = HandleSet::handle_set_create();
    let port = ipc_port_create(
        IpcUuid::from_bytes([14; 16]),
        "com.xkernel.test.tipc.hset-remove".to_string(),
        1,
        32,
        IpcPortFlags::ALLOW_TA_CONNECT,
    )
    .unwrap();

    let mut table = HandleTable::new();
    let hset_id = table
        .uctx_handle_install(hset.clone() as Arc<dyn Handle>)
        .unwrap();
    let port_id = table
        .uctx_handle_install(port.clone() as Arc<dyn Handle>)
        .unwrap();

    hset.handle_set_ctrl(
        HandleSetCommand::Add,
        HandleSetEntry {
            handle_id: port_id,
            handle: port.clone() as Arc<dyn Handle>,
            event: HandleEventMask::ERROR,
            cookie: 0x77,
        },
    )
    .unwrap();

    table.uctx_handle_remove(hset_id).unwrap();
    table.uctx_handle_remove(port_id).unwrap();
    assert_eq!(hset.poll_one(), Err(KError::NotFound));
}

#[def_test]
fn memref_handle_can_be_transferred_in_message() {
    let port = ipc_port_create(
        IpcUuid::from_bytes([9; 16]),
        "com.xkernel.test.tipc.memref".to_string(),
        1,
        32,
        IpcPortFlags::ALLOW_TA_CONNECT,
    )
    .unwrap();
    ipc_port_publish(&port).unwrap();
    let client = ipc_port_connect_async(
        IpcUuid::from_bytes([10; 16]),
        port.path(),
        IpcConnectFlags::ASYNC,
    )
    .unwrap();
    let (server, _) = port.ipc_port_accept().unwrap();

    let memref = MemRef::create(0x1000, 0x2000, crate::MMAP_FLAG_PROT_READ).unwrap();
    assert_eq!(memref.addr(), 0x1000);
    assert_eq!(memref.size(), 0x2000);
    assert_eq!(
        client
            .ipc_send_msg_with_handles(
                b"with-handle",
                &[memref.clone() as alloc::sync::Arc<dyn Handle>],
            )
            .unwrap(),
        11
    );

    let info = server.ipc_get_msg().unwrap();
    assert_eq!(info.num_handles, 1);
    let handles = server.ipc_read_msg_handles(info.id, 1).unwrap();
    assert_eq!(handles.len(), 1);
    assert_eq!(handles[0].kind(), HandleKind::MemRef);
    let received = handles[0].as_any().downcast_ref::<MemRef>().unwrap();
    assert_eq!(received.addr(), 0x1000);
    server.ipc_put_msg(info.id).unwrap();

    server.close();
    port.close();
}

#[def_test]
fn memref_create_validates_range_and_protection_bits() {
    assert_eq!(
        MemRef::create(0x1000, 0, crate::MMAP_FLAG_PROT_READ).map(|_| ()),
        Err(KError::InvalidInput)
    );
    assert_eq!(
        MemRef::create(0x1000, 0x1000, crate::MMAP_FLAG_PROT_MASK << 1).map(|_| ()),
        Err(KError::InvalidInput)
    );
    assert_eq!(
        MemRef::create(0x1000, 0x1000, 0).map(|_| ()),
        Err(KError::InvalidInput)
    );
    assert_eq!(
        MemRef::create(0x1000, 0x1000, crate::MMAP_FLAG_PROT_EXEC).map(|_| ()),
        Err(KError::InvalidInput)
    );
    assert_eq!(
        MemRef::create(0x1000, 0x1000, crate::MMAP_FLAG_PROT_WRITE).map(|_| ()),
        Err(KError::InvalidInput)
    );
    assert_eq!(
        MemRef::create(0x1001, 0x1000, crate::MMAP_FLAG_PROT_READ).map(|_| ()),
        Err(KError::InvalidInput)
    );
    assert_eq!(
        MemRef::create(0x1000, 0x1001, crate::MMAP_FLAG_PROT_READ).map(|_| ()),
        Err(KError::InvalidInput)
    );
    assert_eq!(
        MemRef::create(usize::MAX, 2, crate::MMAP_FLAG_PROT_READ).map(|_| ()),
        Err(KError::OutOfRange)
    );
}

#[def_test]
fn memref_validate_mmap_checks_bounds_and_requested_permissions() {
    let read_only = MemRef::create(0x1000, 0x3000, crate::MMAP_FLAG_PROT_READ).unwrap();
    assert_eq!(
        read_only.validate_mmap(0, 0x1000, crate::MMAP_FLAG_PROT_READ),
        Ok(())
    );
    assert_eq!(
        read_only.validate_mmap(0x1001, 0x1000, crate::MMAP_FLAG_PROT_READ),
        Err(KError::InvalidInput)
    );
    assert_eq!(
        read_only.validate_mmap(0, 0x1001, crate::MMAP_FLAG_PROT_READ),
        Err(KError::InvalidInput)
    );
    assert_eq!(
        read_only.validate_mmap(0x3000, 0x1000, crate::MMAP_FLAG_PROT_READ),
        Err(KError::PermissionDenied)
    );
    assert_eq!(
        read_only.validate_mmap(
            0,
            0x1000,
            crate::MMAP_FLAG_PROT_READ | crate::MMAP_FLAG_PROT_WRITE
        ),
        Err(KError::PermissionDenied)
    );

    let read_write = MemRef::create(
        0x1000,
        0x3000,
        crate::MMAP_FLAG_PROT_READ | crate::MMAP_FLAG_PROT_WRITE,
    )
    .unwrap();
    assert_eq!(
        read_write.validate_mmap(0x1000, 0x1000, crate::MMAP_FLAG_PROT_READ),
        Ok(())
    );
    assert_eq!(
        read_write.validate_mmap(
            0x1000,
            0x1000,
            crate::MMAP_FLAG_PROT_READ | crate::MMAP_FLAG_PROT_WRITE
        ),
        Ok(())
    );
}
