// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{
    collections::BTreeMap,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};

use kerrno::{KError, KResult};
use klazy::Lazy;
use kspin::SpinNoIrq;
use log::error;

use crate::{Handle, IPC_PORT_PATH_MAX, IpcChan, IpcConnectFlags, IpcPort, IpcPortFlags, IpcUuid};

/// Global TIPC namespace state keyed by service port path.
///
/// The registry mirrors the process-independent IPC port lists: published
/// ports are discoverable by `connect`, while clients that requested
/// `WAIT_FOR_PORT` are kept until a matching service is published.
#[derive(Default)]
struct PortRegistryState {
    /// Published service ports, held weakly so the creating process owns the
    /// port lifetime through its handle table.
    ports: BTreeMap<String, Weak<IpcPort>>,
    /// Client endpoints waiting for a service path that has not been
    /// published yet.
    waiting_for_port: BTreeMap<String, Vec<Weak<IpcChan>>>,
}

/// Locked wrapper around the global TIPC service namespace.
struct PortRegistry {
    state: SpinNoIrq<PortRegistryState>,
}

/// Global TIPC port registry shared by all callers in the kernel.
static PORT_REGISTRY: Lazy<PortRegistry> = Lazy::new(|| PortRegistry {
    state: SpinNoIrq::new(PortRegistryState::default()),
});

fn validate_port_path(path: &str) -> KResult {
    if path.is_empty() || path.as_bytes().contains(&0) {
        return Err(KError::InvalidInput);
    }
    if path.len() >= IPC_PORT_PATH_MAX {
        return Err(KError::OutOfRange);
    }
    Ok(())
}

/// Creates an unpublished service port.
pub fn ipc_port_create(
    uuid: IpcUuid,
    path: String,
    num_recv_bufs: usize,
    recv_buf_size: usize,
    flags: IpcPortFlags,
) -> KResult<Arc<IpcPort>> {
    validate_port_path(&path)?;
    IpcPort::new(uuid, path, num_recv_bufs, recv_buf_size, flags)
}

/// Publishes a service port in the global path registry.
pub fn ipc_port_publish(port: &Arc<IpcPort>) -> KResult {
    let waiting = {
        let mut state = PORT_REGISTRY.state.lock();
        // Remove any stale weak references to ports that have been dropped.
        if let Some(stale_port) = state.ports.get(port.path())
            && stale_port.upgrade().is_none()
        {
            state.ports.remove(port.path());
        }
        // Check for duplicates
        if state
            .ports
            .get(port.path())
            .and_then(Weak::upgrade)
            .is_some()
        {
            return Err(KError::AlreadyExists);
        }
        // Mark the port as published (set as listening) and move it into the registry.
        port.mark_published()?;
        state.ports.insert(port.path().into(), Arc::downgrade(port));
        // Move any clients that were waiting for this port into a local list to be notified after we unlock the registry
        state
            .waiting_for_port
            .remove(port.path())
            .unwrap_or_default()
    };

    for client in waiting {
        let Some(client) = client.upgrade() else {
            continue;
        };
        if port.port_attach_client(&client).is_err() {
            error!("failed to attach waiting client to port {}", port.path());
            client.close();
        }
    }
    Ok(())
}

pub(crate) fn unpublish_port(path: &str, expected: &IpcPort) {
    let mut state = PORT_REGISTRY.state.lock();
    let remove = state
        .ports
        .get(path)
        .and_then(Weak::upgrade)
        .is_some_and(|port| core::ptr::eq(port.as_ref(), expected));
    if remove {
        state.ports.remove(path);
    }
}

/// Creates a client channel and starts an asynchronous connection attempt.
///
/// If the destination does not exist and `WAIT_FOR_PORT` is set, the returned
/// channel remains in `Connecting` state until a matching port is published.
pub fn ipc_port_connect_async(
    uuid: IpcUuid,
    path: &str,
    flags: IpcConnectFlags,
) -> KResult<Arc<IpcChan>> {
    if flags.bits() & !IpcConnectFlags::all().bits() != 0 {
        return Err(KError::InvalidInput);
    }
    validate_port_path(path)?;

    let client = IpcChan::new_client(uuid);
    let port = {
        let mut state = PORT_REGISTRY.state.lock();
        if let Some(port) = state.ports.get(path).and_then(Weak::upgrade) {
            Some(port)
        } else if flags.contains(IpcConnectFlags::WAIT_FOR_PORT) {
            state
                .waiting_for_port
                .entry(path.into())
                .or_default()
                .push(Arc::downgrade(&client));
            None
        } else {
            return Err(KError::NotFound);
        }
    };

    if let Some(port) = port {
        port.port_attach_client(&client)?;
    }
    Ok(client)
}
