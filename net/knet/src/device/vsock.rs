// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Vsock device integration helpers.
use alloc::collections::VecDeque;
use core::sync::atomic::{AtomicU64, Ordering};

use kclass::{ClassDevice, prelude::*};
use kdevice::DeviceId;
use kerrno::{KError, KResult, k_bail};
use ksync::{Mutex, static_lock};
use ktask::future::{block_on, interruptible};
use ktime_types::TimeSpan;

use crate::{alloc::string::ToString, vsock::connection_manager::VSOCK_CONN_MANAGER};

static_lock! {
    static VSOCK_DEV: Mutex<Option<ClassDevice<VsockDeviceImpl>>> = Mutex::new(None);
}
static_lock! {
    static VSOCK_EVENT_QUEUE: Mutex<VecDeque<VsockDriverEventType>> = Mutex::new(VecDeque::new());
}
static_lock! {
    static POLLER_STATE: Mutex<PollerState> = Mutex::new(PollerState::new());
}

const VSOCK_RX_SCRATCH_SIZE: usize = 0x1000; // 4KiB scratch buffer for vsock receive

fn get_vsock_dev() -> KResult<ClassDevice<VsockDeviceImpl>> {
    VSOCK_DEV.lock().as_ref().cloned().ok_or(KError::NotFound)
}

/// Registers a vsock device. Only one vsock device can be registered.
pub fn register_vsock_dev(dev: ClassDevice<VsockDeviceImpl>) -> KResult {
    let mut guard = VSOCK_DEV.lock();
    if guard.is_some() {
        k_bail!(AlreadyExists, "vsock device already registered");
    }
    *guard = Some(dev);
    drop(guard);
    Ok(())
}

pub fn unregister_vsock_dev(id: DeviceId) -> bool {
    let mut guard = VSOCK_DEV.lock();
    if guard.as_ref().is_none_or(|dev| dev.id() != id) {
        return false;
    }
    *guard = None;
    VSOCK_EVENT_QUEUE.lock().clear();
    let mut state = POLLER_STATE.lock();
    state.ref_count = 0;
    true
}

static POLL_BACKOFF: PollBackoff = PollBackoff::new();

struct PollerState {
    ref_count: usize,
    active: bool,
}

impl PollerState {
    const fn new() -> Self {
        Self {
            ref_count: 0,
            active: false,
        }
    }
}

struct PollBackoff {
    consecutive_idle: AtomicU64,
}

impl PollBackoff {
    const fn new() -> Self {
        Self {
            consecutive_idle: AtomicU64::new(0),
        }
    }

    fn next_interval(&self) -> TimeSpan {
        let idle = self.consecutive_idle.load(Ordering::Relaxed);
        let interval_us = match idle {
            0..=3 => 100,     //  3 ：100μs
            4..=10 => 500,    // 4-10 ：500μs
            11..=20 => 2_000, // 11-20 ：2ms
            _ => 10_000,      // 20+ ：10ms
        };
        TimeSpan::from_micros(interval_us)
    }

    fn on_activity(&self) {
        self.consecutive_idle.store(0, Ordering::Release);
    }

    fn on_idle_tick(&self) {
        self.consecutive_idle.fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self) -> (u64, u64) {
        let idle = self.consecutive_idle.load(Ordering::Relaxed);
        let interval = self.next_interval().as_micros() as u64;
        (idle, interval)
    }
}

/// Start the background vsock polling task if needed.
pub fn start_vsock_polling() {
    let mut state = POLLER_STATE.lock();
    state.ref_count += 1;
    debug!("start_vsock_polling: ref_count -> {}", state.ref_count);
    if state.ref_count == 1 {
        if !state.active {
            state.active = true;
            drop(state);
            debug!("Starting vsock poll task");
            ktask::spawn_with_name(vsock_poll_task, "vsock-poll".to_string());
        } else {
            debug!("Poll task already running");
        }
    }
}

pub fn stop_vsock_polling() {
    let mut state = POLLER_STATE.lock();
    if state.ref_count == 0 {
        // this should not happen, log a warning
        warn!("stop_vsock_polling called but ref_count already 0");
        return;
    }
    state.ref_count -= 1;
    debug!("stop_vsock_polling: ref_count -> {}", state.ref_count);
}

fn vsock_poll_task() {
    loop {
        if should_stop_vsock_poll_task() {
            break;
        }

        let _ = block_on(interruptible(poll_vsock_adaptive()));
    }
}

fn should_stop_vsock_poll_task() -> bool {
    let mut state = POLLER_STATE.lock();
    if state.ref_count != 0 {
        return false;
    }
    state.active = false;
    debug!("Vsock poll task exiting (no active connections)");
    true
}

async fn poll_vsock_adaptive() -> KResult<()> {
    let has_events = poll_vsock_devices()?;

    if has_events {
        POLL_BACKOFF.on_activity();
        ktask::yield_now();
        return Ok(());
    }

    POLL_BACKOFF.on_idle_tick();
    let interval = POLL_BACKOFF.next_interval();

    let (idle_count, interval_us) = POLL_BACKOFF.snapshot();
    if idle_count > 0 && idle_count % 10 == 0 {
        trace!("Poll frequency: idle_count={idle_count}, interval={interval_us}μs",);
    }
    ktask::future::sleep(interval).await;
    Ok(())
}

fn poll_vsock_devices() -> KResult<bool> {
    let dev = get_vsock_dev()?;
    let mut event_count = 0;
    let mut buf = alloc::vec![0; VSOCK_RX_SCRATCH_SIZE];

    // Process pending events first
    // Use core::mem::take to atomically move all events out and empty the global queue
    let pending_events = core::mem::take(&mut *VSOCK_EVENT_QUEUE.lock());
    for event in pending_events {
        handle_vsock_event(event, &dev, &mut buf);
    }

    loop {
        match dev.poll_event() {
            Ok(None) => break, // no more events
            Ok(Some(event)) => {
                event_count += 1;
                handle_vsock_event(event, &dev, &mut buf);
            }
            Err(e) => {
                info!("Failed to poll vsock event: {e:?}");
                break;
            }
        }
    }
    Ok(event_count > 0)
}

fn handle_vsock_event(event: VsockDriverEventType, dev: &dyn VsockDevice, buf: &mut [u8]) {
    debug!("Handling vsock event: {event:?}");

    #[cfg(feature = "vsock_tipc_bridge")]
    if crate::vsock::bridge::route_event(event) {
        return;
    }

    match event {
        VsockDriverEventType::ConnectionRequest(conn_id) => {
            if let Err(e) = VSOCK_CONN_MANAGER.lock().on_connection_request(conn_id) {
                info!("Connection request failed: {conn_id:?}, error={e:?}");
            }
        }

        VsockDriverEventType::Received(conn_id, len) => {
            // Look up the connection, then release the manager lock before
            // touching the connection or the device. This keeps the global
            // order `VSOCK_CONN_MANAGER -> conn` and never holds the manager
            // lock across the `conn` lock or device IO.
            let Some(conn) = VSOCK_CONN_MANAGER.lock().get_connection(conn_id) else {
                info!("Received data for unknown connection: {conn_id:?}");
                return;
            };

            let free_space = conn.lock().rx_buffer_free();
            if free_space == 0 {
                VSOCK_EVENT_QUEUE
                    .lock()
                    .push_back(VsockDriverEventType::Received(conn_id, len));
                return;
            }

            let max_read = core::cmp::min(free_space, buf.len());
            match dev.recv(conn_id, &mut buf[..max_read]) {
                Ok(read_len) => {
                    if let Err(e) = VSOCK_CONN_MANAGER
                        .lock()
                        .on_data_received(conn_id, &buf[..read_len])
                    {
                        info!(
                            "Failed to dispatch_irq received data: conn_id={conn_id:?}, \
                             error={e:?}",
                        );
                    }
                }
                Err(e) => {
                    info!("Failed to receive vsock data: conn_id={conn_id:?}, error={e:?}",);
                }
            }
        }

        VsockDriverEventType::Disconnected(conn_id) => {
            if let Err(e) = VSOCK_CONN_MANAGER.lock().on_disconnected(conn_id) {
                info!("Failed to dispatch_irq disconnection: {conn_id:?}, error={e:?}",);
            }
        }

        VsockDriverEventType::Connected(conn_id) => {
            if let Err(e) = VSOCK_CONN_MANAGER.lock().on_connected(conn_id) {
                info!("Failed to dispatch_irq connection established: {conn_id:?}, error={e:?}",);
            }
        }

        VsockDriverEventType::CreditUpdate(conn_id) => {
            if let Err(e) = VSOCK_CONN_MANAGER.lock().on_credit_update(conn_id) {
                info!("Failed to handle credit update: {conn_id:?}, error={e:?}",);
            }
        }

        VsockDriverEventType::Unknown => warn!("Received unknown vsock event"),
    }
}

pub fn vsock_listen(addr: VsockAddr) -> KResult<()> {
    let dev = get_vsock_dev()?;
    dev.listen(addr.port);
    Ok(())
}

fn map_dev_err(e: DriverError) -> KError {
    match e {
        DriverError::AlreadyExists => KError::AlreadyExists,
        DriverError::WouldBlock => KError::WouldBlock,
        DriverError::InvalidInput => KError::InvalidInput,
        DriverError::Io => KError::Io,
        _ => KError::BadState,
    }
}

pub fn vsock_connect(conn_id: VsockConnId) -> KResult<()> {
    let dev = get_vsock_dev()?;
    dev.connect(conn_id).map_err(map_dev_err)
}

pub fn vsock_send(
    conn_id: VsockConnId,
    buf: &[u8],
    tx_wait_queue: &ktask::WaitQueue,
) -> KResult<usize> {
    let max_retries = 10; // Tests have shown that no more than two retries will be notified
    for _ in 0..max_retries {
        let result = {
            let dev = get_vsock_dev()?;
            dev.send(conn_id, buf)
        };
        match result {
            Ok(len) => return Ok(len),
            Err(DriverError::WouldBlock) => {
                tx_wait_queue.wait_timeout(TimeSpan::from_millis(10));
            }
            Err(e) => return Err(map_dev_err(e)),
        }
    }
    Err(map_dev_err(DriverError::WouldBlock))
}

pub fn vsock_disconnect(conn_id: VsockConnId) -> KResult<()> {
    let dev = get_vsock_dev()?;
    dev.disconnect(conn_id).map_err(map_dev_err)
}

pub fn vsock_guest_cid() -> KResult<u64> {
    let dev = get_vsock_dev()?;
    Ok(dev.guest_cid())
}
